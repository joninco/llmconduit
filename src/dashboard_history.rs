//! Durable SQLite-backed dashboard history.
//!
//! The live dashboard stores remain the low-latency source for current traffic. This
//! module persists the exact coordinated five-second [`DashboardSnapshot`] cuts plus
//! monitor updates on a dedicated blocking worker, allowing the REST API to fall back
//! after the in-memory 30-minute TTL or a process restart. Large turn bodies stay in
//! the existing atomic per-turn files; SQLite indexes those artifacts rather than
//! duplicating them into the database/WAL.

use crate::dashboard_flow::SnapshotFlowSummary;
use crate::metrics::DashboardSnapshot;
use crate::monitor::{DebugUpdate, DebugWsMessage};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};

const ENV_HISTORY_DB: &str = "LLMCONDUIT_DASHBOARD_HISTORY_DB";
const ENV_RETENTION_HOURS: &str = "LLMCONDUIT_DASHBOARD_HISTORY_RETENTION_HOURS";
const DEFAULT_RETENTION_HOURS: u64 = 24;
// Enough to absorb a short WAL/fsync stall without allowing retained body-free cuts or monitor
// payloads to grow into a second unbounded history store in memory.
const WRITER_QUEUE_CAPACITY: usize = 32;
const SQLITE_SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
enum Command {
    PersistCut {
        cut_id: u64,
        taken_at_ms: i64,
        monitor_seq: i64,
        snapshot: Vec<u8>,
        summaries: Vec<(String, i64, Vec<u8>)>,
    },
    PersistMonitor {
        sequence: i64,
        update: Vec<u8>,
    },
    IndexArtifact {
        api_call_id: String,
        path: PathBuf,
        bytes: i64,
        modified_ms: i64,
    },
}

#[derive(Debug)]
struct Inner {
    path: PathBuf,
    artifact_dir: Option<PathBuf>,
    sender: SyncSender<Command>,
    dropped_writes: AtomicU64,
}

/// Cloneable handle to the optional durable history store. `disabled()` contains no
/// channel, thread, connection, allocation-heavy state, or filesystem path.
#[derive(Clone, Debug, Default)]
pub struct DashboardHistory {
    inner: Option<Arc<Inner>>,
}

#[derive(Debug, Clone)]
pub struct HistoricalCut {
    pub cut_id: u64,
    pub snapshot: Arc<DashboardSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DurableHistoryMetadata {
    pub oldest_at_ms: Option<u128>,
    pub newest_at_ms: Option<u128>,
    pub retained_cuts: usize,
    pub database_bytes: usize,
    pub dropped_writes: u64,
}

#[derive(Debug, Clone, Copy)]
enum CutLookup {
    Latest,
    AtOrBefore(u64),
    Nearest(u64),
    Id(u64),
}

impl DashboardHistory {
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Build from the dashboard-specific environment. History is armed only when the
    /// debug UI is enabled AND `LLMCONDUIT_DASHBOARD_HISTORY_DB` is nonblank, preserving
    /// the dashboard-disabled zero-task/zero-IO contract.
    pub fn from_env(debug_ui_enabled: bool, artifact_dir: Option<PathBuf>) -> Self {
        if !debug_ui_enabled {
            return Self::disabled();
        }
        let Some(path) = std::env::var_os(ENV_HISTORY_DB)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        else {
            return Self::disabled();
        };
        let retention_hours = std::env::var(ENV_RETENTION_HOURS)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|hours| *hours > 0)
            .unwrap_or(DEFAULT_RETENTION_HOURS);
        let retention_ms = retention_hours
            .saturating_mul(60)
            .saturating_mul(60)
            .saturating_mul(1000)
            .min(i64::MAX as u64) as i64;

        match Self::open(path, artifact_dir, retention_ms) {
            Ok(history) => history,
            Err(error) => {
                tracing::error!(%error, "dashboard history disabled: SQLite initialization failed");
                Self::disabled()
            }
        }
    }

    fn open(
        path: PathBuf,
        artifact_dir: Option<PathBuf>,
        retention_ms: i64,
    ) -> rusqlite::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let mut connection = Connection::open(&path)?;
        initialize(&mut connection)?;
        if let Some(dir) = artifact_dir.as_deref() {
            index_existing_artifacts(&connection, dir)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("dashboard-history".to_string())
            .spawn(move || writer_loop(connection, receiver, retention_ms))
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        tracing::info!(
            path = %path.display(),
            retention_hours = retention_ms / (60 * 60 * 1000),
            "dashboard SQLite history enabled"
        );
        Ok(Self {
            inner: Some(Arc::new(Inner {
                path,
                artifact_dir,
                sender,
                dropped_writes: AtomicU64::new(0),
            })),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn persist_cut(&self, snapshot: Arc<DashboardSnapshot>) {
        if self.inner.is_none() {
            return;
        }
        let cut_id = match u64::try_from(snapshot.taken_at_ms) {
            Ok(value) => value,
            Err(_) => return,
        };
        let Ok(snapshot_blob) = serde_cbor::to_vec(snapshot.as_ref()) else {
            tracing::error!(cut_id, "failed to serialize dashboard history cut");
            return;
        };
        let summaries = snapshot
            .summaries
            .iter()
            .filter_map(|summary| {
                let revision = i64::try_from(summary.revision).ok()?;
                let blob = serde_cbor::to_vec(summary).ok()?;
                Some((summary.api_call_id.clone(), revision, blob))
            })
            .collect();
        self.try_send(Command::PersistCut {
            cut_id,
            taken_at_ms: cut_id as i64,
            monitor_seq: snapshot.cursors.monitor_seq.min(i64::MAX as u64) as i64,
            snapshot: snapshot_blob,
            summaries,
        });
    }

    pub fn persist_monitor_update(&self, update: DebugUpdate) {
        if self.inner.is_none() {
            return;
        }
        let Ok(blob) = serde_cbor::to_vec(&update) else {
            return;
        };
        self.try_send(Command::PersistMonitor {
            sequence: update.sequence.min(i64::MAX as u64) as i64,
            update: blob,
        });
    }

    pub fn index_artifact(&self, api_call_id: String, path: PathBuf) {
        if self.inner.is_none() {
            return;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            return;
        };
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        self.try_send(Command::IndexArtifact {
            api_call_id,
            path,
            bytes: metadata.len().min(i64::MAX as u64) as i64,
            modified_ms,
        });
    }

    fn try_send(&self, command: Command) {
        let Some(inner) = &self.inner else { return };
        match inner.sender.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                inner.dropped_writes.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("dashboard history writer unavailable; durable cut/update dropped");
            }
        }
    }

    pub async fn latest_cut(&self) -> Option<HistoricalCut> {
        self.load_cut(CutLookup::Latest).await
    }

    pub async fn cut_at_or_before(&self, at_ms: u64) -> Option<HistoricalCut> {
        self.load_cut(CutLookup::AtOrBefore(at_ms)).await
    }

    pub async fn nearest_cut(&self, at_ms: u64) -> Option<HistoricalCut> {
        self.load_cut(CutLookup::Nearest(at_ms)).await
    }

    pub async fn cut_by_id(&self, cut_id: u64) -> Option<HistoricalCut> {
        self.load_cut(CutLookup::Id(cut_id)).await
    }

    pub async fn cuts_between(
        &self,
        from_ms: Option<u64>,
        to_ms: Option<u64>,
    ) -> Vec<HistoricalCut> {
        let Some(inner) = self.inner.clone() else {
            return Vec::new();
        };
        tokio::task::spawn_blocking(move || load_cuts_sync(&inner.path, from_ms, to_ms))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    /// Latest body-free version of every flow observed at or before `cut_id`. Unlike
    /// the in-memory snapshot's 30-minute summary population, this spans the configured
    /// durable retention and is therefore the source for historical flow lists and
    /// exact chart drilldowns.
    pub async fn flow_summaries_as_of(&self, cut_id: u64) -> Vec<SnapshotFlowSummary> {
        let Some(inner) = self.inner.clone() else {
            return Vec::new();
        };
        tokio::task::spawn_blocking(move || load_flow_summaries_as_of_sync(&inner.path, cut_id))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    async fn load_cut(&self, lookup: CutLookup) -> Option<HistoricalCut> {
        let inner = self.inner.clone()?;
        tokio::task::spawn_blocking(move || load_cut_sync(&inner.path, lookup))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub async fn latest_flow_summary(&self, api_call_id: &str) -> Option<SnapshotFlowSummary> {
        let inner = self.inner.clone()?;
        let id = api_call_id.to_string();
        tokio::task::spawn_blocking(move || load_flow_summary_sync(&inner.path, &id, None))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub async fn flow_summary_at(
        &self,
        api_call_id: &str,
        cut_id: u64,
    ) -> Option<SnapshotFlowSummary> {
        let inner = self.inner.clone()?;
        let id = api_call_id.to_string();
        tokio::task::spawn_blocking(move || load_flow_summary_sync(&inner.path, &id, Some(cut_id)))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub async fn monitor_messages_through(&self, sequence: u64) -> Vec<DebugWsMessage> {
        let Some(inner) = self.inner.clone() else {
            return Vec::new();
        };
        tokio::task::spawn_blocking(move || load_monitor_messages_sync(&inner.path, sequence))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    pub async fn artifact_path(&self, api_call_id: &str) -> Option<PathBuf> {
        let inner = self.inner.clone()?;
        let id = api_call_id.to_string();
        tokio::task::spawn_blocking(move || {
            let connection = reader(&inner.path)?;
            let indexed: Option<String> = connection
                .query_row(
                    "SELECT path FROM artifacts WHERE api_call_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(path) = indexed.map(PathBuf::from).filter(|path| path.is_file()) {
                return Ok::<Option<PathBuf>, rusqlite::Error>(Some(path));
            }
            let derived = inner
                .artifact_dir
                .as_ref()
                .map(|dir| dir.join(format!("{id}.json")));
            Ok::<Option<PathBuf>, rusqlite::Error>(derived.filter(|path| path.is_file()))
        })
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
    }

    pub async fn metadata(&self) -> DurableHistoryMetadata {
        let Some(inner) = self.inner.clone() else {
            return DurableHistoryMetadata::default();
        };
        let dropped = inner.dropped_writes.load(Ordering::Relaxed);
        tokio::task::spawn_blocking(move || metadata_sync(&inner.path, dropped))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn enabled_for_test(path: PathBuf, artifact_dir: Option<PathBuf>) -> Self {
        Self::open(path, artifact_dir, 24 * 60 * 60 * 1000).expect("test history opens")
    }

    #[cfg(test)]
    fn enabled_for_test_with_retention(path: PathBuf, retention_ms: i64) -> Self {
        Self::open(path, None, retention_ms).expect("test history opens")
    }
}

/// Persist the authoritative monitor broadcast independently of request processing. A
/// lagged receiver records a warning and resumes at the next available update; cuts still
/// carry their monitor cursor, so the UI can bound replay and expose any transcript gap.
pub fn spawn_monitor_history_task(
    history: DashboardHistory,
    mut receiver: tokio::sync::broadcast::Receiver<DebugUpdate>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !history.is_enabled() {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(update) => history.persist_monitor_update(update),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "dashboard history monitor subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

fn initialize(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "busy_timeout", 5_000i64)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS dashboard_cuts (
            cut_id INTEGER PRIMARY KEY,
            taken_at_ms INTEGER NOT NULL UNIQUE,
            monitor_seq INTEGER NOT NULL,
            snapshot BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS dashboard_cuts_taken_idx
            ON dashboard_cuts(taken_at_ms);
         CREATE TABLE IF NOT EXISTS flow_versions (
            api_call_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            observed_at_ms INTEGER NOT NULL,
            summary BLOB NOT NULL,
            PRIMARY KEY(api_call_id, revision)
         );
         CREATE INDEX IF NOT EXISTS flow_versions_asof_idx
            ON flow_versions(api_call_id, observed_at_ms DESC, revision DESC);
         CREATE TABLE IF NOT EXISTS monitor_updates (
            sequence INTEGER PRIMARY KEY,
            update_blob BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifacts (
            api_call_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            bytes INTEGER NOT NULL,
            modified_ms INTEGER NOT NULL
         );",
    )?;
    connection.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
    Ok(())
}

fn writer_loop(mut connection: Connection, receiver: mpsc::Receiver<Command>, retention_ms: i64) {
    while let Ok(command) = receiver.recv() {
        let result = match command {
            Command::PersistCut {
                cut_id,
                taken_at_ms,
                monitor_seq,
                snapshot,
                summaries,
            } => persist_cut_sync(
                &mut connection,
                cut_id,
                taken_at_ms,
                monitor_seq,
                snapshot,
                summaries,
                retention_ms,
            ),
            Command::PersistMonitor { sequence, update } => connection
                .execute(
                    "INSERT OR REPLACE INTO monitor_updates(sequence, update_blob) VALUES (?1, ?2)",
                    params![sequence, update],
                )
                .map(|_| ()),
            Command::IndexArtifact {
                api_call_id,
                path,
                bytes,
                modified_ms,
            } => connection
                .execute(
                    "INSERT OR REPLACE INTO artifacts(api_call_id, path, bytes, modified_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![api_call_id, path.to_string_lossy(), bytes, modified_ms],
                )
                .map(|_| ()),
        };
        if let Err(error) = result {
            tracing::error!(%error, "dashboard history writer command failed");
        }
    }
}

fn persist_cut_sync(
    connection: &mut Connection,
    cut_id: u64,
    taken_at_ms: i64,
    monitor_seq: i64,
    snapshot: Vec<u8>,
    summaries: Vec<(String, i64, Vec<u8>)>,
    retention_ms: i64,
) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT OR REPLACE INTO dashboard_cuts(cut_id, taken_at_ms, monitor_seq, snapshot)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            cut_id.min(i64::MAX as u64) as i64,
            taken_at_ms,
            monitor_seq,
            snapshot
        ],
    )?;
    {
        let mut statement = transaction.prepare(
            "INSERT OR IGNORE INTO flow_versions(api_call_id, revision, observed_at_ms, summary)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (api_call_id, revision, summary) in summaries {
            statement.execute(params![api_call_id, revision, taken_at_ms, summary])?;
        }
    }
    let cutoff = taken_at_ms.saturating_sub(retention_ms);
    transaction.execute(
        "DELETE FROM dashboard_cuts WHERE taken_at_ms < ?1",
        [cutoff],
    )?;
    // Preserve the newest known version of a long-running/unchanged flow even when the version was
    // first observed before the retention boundary. Repeated cuts intentionally de-duplicate an
    // unchanged `(api_call_id, revision)`; deleting it solely by its first-observed timestamp would
    // make that flow disappear from newer retained cuts.
    transaction.execute(
        "DELETE FROM flow_versions AS stale
         WHERE stale.observed_at_ms < ?1
           AND EXISTS (
             SELECT 1 FROM flow_versions AS newer
             WHERE newer.api_call_id = stale.api_call_id
               AND newer.revision > stale.revision
           )",
        [cutoff],
    )?;
    let oldest_monitor: Option<i64> = transaction
        .query_row("SELECT MIN(monitor_seq) FROM dashboard_cuts", [], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    if let Some(sequence) = oldest_monitor {
        transaction.execute(
            "DELETE FROM monitor_updates WHERE sequence < ?1",
            [sequence],
        )?;
    }
    transaction.commit()
}

fn reader(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "query_only", true)?;
    connection.pragma_update(None, "busy_timeout", 5_000i64)?;
    Ok(connection)
}

fn load_cut_sync(path: &Path, lookup: CutLookup) -> rusqlite::Result<Option<HistoricalCut>> {
    let connection = reader(path)?;
    let (sql, value): (&str, Option<i64>) = match lookup {
        CutLookup::Latest => (
            "SELECT cut_id, snapshot FROM dashboard_cuts ORDER BY taken_at_ms DESC LIMIT 1",
            None,
        ),
        CutLookup::AtOrBefore(at) => (
            "SELECT cut_id, snapshot FROM dashboard_cuts WHERE taken_at_ms <= ?1 ORDER BY taken_at_ms DESC LIMIT 1",
            Some(at.min(i64::MAX as u64) as i64),
        ),
        CutLookup::Id(id) => (
            "SELECT cut_id, snapshot FROM dashboard_cuts WHERE cut_id = ?1 LIMIT 1",
            Some(id.min(i64::MAX as u64) as i64),
        ),
        CutLookup::Nearest(at) => {
            let at = at.min(i64::MAX as u64) as i64;
            let row: Option<(i64, Vec<u8>)> = connection
                .query_row(
                    "SELECT cut_id, snapshot FROM dashboard_cuts
                     ORDER BY ABS(taken_at_ms - ?1), taken_at_ms ASC LIMIT 1",
                    [at],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            return decode_cut(row);
        }
    };
    let row = if let Some(value) = value {
        connection
            .query_row(sql, [value], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
    } else {
        connection
            .query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
    };
    decode_cut(row)
}

fn load_cuts_sync(
    path: &Path,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
) -> rusqlite::Result<Vec<HistoricalCut>> {
    let connection = reader(path)?;
    let from = from_ms.unwrap_or(0).min(i64::MAX as u64) as i64;
    let to = to_ms.unwrap_or(i64::MAX as u64).min(i64::MAX as u64) as i64;
    let mut statement = connection.prepare(
        "SELECT cut_id, snapshot FROM dashboard_cuts
         WHERE taken_at_ms >= ?1 AND taken_at_ms <= ?2 ORDER BY taken_at_ms ASC",
    )?;
    let mut rows = statement.query(params![from, to])?;
    let mut cuts = Vec::new();
    while let Some(row) = rows.next()? {
        let cut_id: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        if let Ok(snapshot) = serde_cbor::from_slice::<DashboardSnapshot>(&blob) {
            cuts.push(HistoricalCut {
                cut_id: cut_id.max(0) as u64,
                snapshot: Arc::new(snapshot),
            });
        }
    }
    Ok(cuts)
}

fn decode_cut(row: Option<(i64, Vec<u8>)>) -> rusqlite::Result<Option<HistoricalCut>> {
    row.map(|(cut_id, blob)| {
        serde_cbor::from_slice::<DashboardSnapshot>(&blob)
            .map(|snapshot| HistoricalCut {
                cut_id: cut_id.max(0) as u64,
                snapshot: Arc::new(snapshot),
            })
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    blob.len(),
                    rusqlite::types::Type::Blob,
                    Box::new(error),
                )
            })
    })
    .transpose()
}

fn load_flow_summary_sync(
    path: &Path,
    api_call_id: &str,
    cut_id: Option<u64>,
) -> rusqlite::Result<Option<SnapshotFlowSummary>> {
    let connection = reader(path)?;
    let blob: Option<Vec<u8>> = if let Some(cut_id) = cut_id {
        connection
            .query_row(
                "SELECT summary FROM flow_versions
                 WHERE api_call_id = ?1 AND observed_at_ms <= (
                    SELECT taken_at_ms FROM dashboard_cuts WHERE cut_id = ?2
                 )
                 ORDER BY observed_at_ms DESC, revision DESC LIMIT 1",
                params![api_call_id, cut_id.min(i64::MAX as u64) as i64],
                |row| row.get(0),
            )
            .optional()?
    } else {
        connection
            .query_row(
                "SELECT summary FROM flow_versions WHERE api_call_id = ?1
                 ORDER BY observed_at_ms DESC, revision DESC LIMIT 1",
                [api_call_id],
                |row| row.get(0),
            )
            .optional()?
    };
    blob.map(|blob| {
        serde_cbor::from_slice(&blob).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                blob.len(),
                rusqlite::types::Type::Blob,
                Box::new(error),
            )
        })
    })
    .transpose()
}

fn load_flow_summaries_as_of_sync(
    path: &Path,
    cut_id: u64,
) -> rusqlite::Result<Vec<SnapshotFlowSummary>> {
    let connection = reader(path)?;
    let taken_at: Option<i64> = connection
        .query_row(
            "SELECT taken_at_ms FROM dashboard_cuts WHERE cut_id = ?1",
            [cut_id.min(i64::MAX as u64) as i64],
            |row| row.get(0),
        )
        .optional()?;
    let Some(taken_at) = taken_at else {
        return Ok(Vec::new());
    };
    let mut statement = connection.prepare(
        "SELECT fv.summary
         FROM flow_versions fv
         JOIN (
           SELECT api_call_id, MAX(revision) AS revision
           FROM flow_versions
           WHERE observed_at_ms <= ?1
           GROUP BY api_call_id
         ) latest
         ON latest.api_call_id = fv.api_call_id AND latest.revision = fv.revision
         ORDER BY fv.observed_at_ms DESC",
    )?;
    let mut rows = statement.query([taken_at])?;
    let mut summaries = Vec::new();
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        if let Ok(summary) = serde_cbor::from_slice(&blob) {
            summaries.push(summary);
        }
    }
    Ok(summaries)
}

fn load_monitor_messages_sync(path: &Path, through: u64) -> rusqlite::Result<Vec<DebugWsMessage>> {
    let connection = reader(path)?;
    let mut statement = connection.prepare(
        "SELECT update_blob FROM monitor_updates WHERE sequence <= ?1 ORDER BY sequence ASC",
    )?;
    let mut rows = statement.query([through.min(i64::MAX as u64) as i64])?;
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        if let Ok(update) = serde_cbor::from_slice::<DebugUpdate>(&blob) {
            messages.extend(update.messages);
        }
    }
    Ok(messages)
}

fn metadata_sync(path: &Path, dropped_writes: u64) -> rusqlite::Result<DurableHistoryMetadata> {
    let connection = reader(path)?;
    let (oldest, newest, count): (Option<i64>, Option<i64>, i64) = connection.query_row(
        "SELECT MIN(taken_at_ms), MAX(taken_at_ms), COUNT(*) FROM dashboard_cuts",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let database_bytes = std::fs::metadata(path)
        .map(|metadata| metadata.len().min(usize::MAX as u64) as usize)
        .unwrap_or(0);
    Ok(DurableHistoryMetadata {
        oldest_at_ms: oldest.map(|value| value.max(0) as u128),
        newest_at_ms: newest.map(|value| value.max(0) as u128),
        retained_cuts: count.max(0) as usize,
        database_bytes,
        dropped_writes,
    })
}

fn index_existing_artifacts(connection: &Connection, dir: &Path) -> rusqlite::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut statement = connection.prepare(
        "INSERT OR REPLACE INTO artifacts(api_call_id, path, bytes, modified_ms)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(api_call_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        if !api_call_id.starts_with("api_") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        statement.execute(params![
            api_call_id,
            path.to_string_lossy(),
            metadata.len().min(i64::MAX as u64) as i64,
            modified_ms,
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_flow::{FlowStatus, PhaseTimings, TerminalCostConfidence};
    use crate::metrics::{DomainCursors, MetricsView};
    use crate::upstream::ProviderHealthSnapshot;

    #[test]
    fn disabled_history_has_no_state() {
        let history = DashboardHistory::disabled();
        assert!(!history.is_enabled());
        history.persist_monitor_update(DebugUpdate {
            sequence: 1,
            messages: Vec::new(),
        });
    }

    #[tokio::test]
    async fn sqlite_cut_and_flow_version_survive_the_memory_store() {
        let root =
            std::env::temp_dir().join(format!("llmconduit-history-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp root");
        let history =
            DashboardHistory::enabled_for_test(root.join("history.sqlite3"), Some(root.clone()));
        let summary = SnapshotFlowSummary {
            revision: 4,
            api_call_id: "api_persisted".to_string(),
            response_id: Some("resp_persisted".to_string()),
            method: "POST".to_string(),
            uri: "/v1/chat/completions".to_string(),
            model_requested: Some("model-a".to_string()),
            model_served: Some("model-b".to_string()),
            upstream_target: Some("provider-a".to_string()),
            usage: None,
            terminal_cost_usd: None,
            terminal_cost_confidence: TerminalCostConfidence::Unavailable,
            cache_price_impact_usd: None,
            effective_route_limit: None,
            status: FlowStatus::Completed,
            started_ms: 1_000,
            finished_ms: Some(2_000),
            elapsed_ms: Some(1_000),
            terminal_reason: Some("response.completed".to_string()),
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
            client_label: Some("test-client".to_string()),
            client_source: Some(crate::dashboard_flow::ClientSource::ConfiguredHeader),
        };
        let cut = Arc::new(DashboardSnapshot {
            taken_at_ms: 5_000,
            cursors: DomainCursors {
                flow_seq: 4,
                metrics_seq: 5,
                topology_seq: 6,
                monitor_seq: 7,
            },
            summaries: vec![summary],
            flow_summaries_truncated: false,
            metrics: MetricsView::default(),
            topology: Arc::new(ProviderHealthSnapshot::default()),
        });
        serde_cbor::to_vec(cut.as_ref()).expect("snapshot serializes");
        history.persist_cut(cut);
        let mut loaded = None;
        for _ in 0..100 {
            loaded = history.cut_by_id(5_000).await;
            if loaded.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let loaded = loaded.expect("writer persisted cut");
        assert_eq!(loaded.snapshot.cursors.monitor_seq, 7);
        assert_eq!(loaded.snapshot.summaries[0].api_call_id, "api_persisted");
        let flow = history
            .flow_summary_at("api_persisted", 5_000)
            .await
            .expect("flow version indexed");
        assert_eq!(flow.revision, 4);

        // An unchanged long-running flow retains the same revision across cuts. Its first-observed
        // row must survive retention cleanup while it remains the newest version, otherwise it
        // disappears from a newer retained cut.
        let compact =
            DashboardHistory::enabled_for_test_with_retention(root.join("compact.sqlite3"), 100);
        for taken_at_ms in [1_000u128, 2_000] {
            compact.persist_cut(Arc::new(DashboardSnapshot {
                taken_at_ms,
                cursors: loaded.snapshot.cursors,
                summaries: vec![flow.clone()],
                flow_summaries_truncated: false,
                metrics: MetricsView::default(),
                topology: Arc::new(ProviderHealthSnapshot::default()),
            }));
        }
        for _ in 0..100 {
            if compact.cut_by_id(2_000).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            compact
                .flow_summary_at("api_persisted", 2_000)
                .await
                .is_some(),
            "newest unchanged flow version survives retention"
        );
        drop(compact);
        drop(history);
        let _ = std::fs::remove_dir_all(root);
    }
}
