//! Durable SQLite-backed dashboard history.
//!
//! The live dashboard stores remain the low-latency source for current traffic. This
//! module persists the exact coordinated five-second [`DashboardSnapshot`] cuts plus
//! monitor updates on a dedicated blocking worker, allowing the REST API to fall back
//! after the in-memory 30-minute TTL or a process restart. Large turn bodies stay in
//! the existing atomic per-turn files; SQLite indexes those artifacts rather than
//! duplicating them into the database/WAL.

use crate::dashboard_flow::{
    ClientSource, FlowMutation, FlowMutationPhase, FlowStatus, SnapshotFlowSummary,
    TerminalCostConfidence,
};
use crate::metrics::{DashboardSnapshot, DomainCursors};
use crate::monitor::{DebugUpdate, DebugWsMessage};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, oneshot};

const ENV_HISTORY_DB: &str = "LLMCONDUIT_DASHBOARD_HISTORY_DB";
const ENV_DURABILITY: &str = "LLMCONDUIT_DASHBOARD_DURABILITY";
const FINE_CUT_RETENTION_MS: i64 = 24 * 60 * 60 * 1_000;
// Enough to absorb a short WAL/fsync stall without allowing retained body-free cuts or monitor
// payloads to grow into a second unbounded history store in memory.
const WRITER_QUEUE_CAPACITY: usize = 32;
const SQLITE_SCHEMA_VERSION: i64 = 2;

/// Runtime contract for the dashboard archive. `BestEffort` preserves the existing
/// opt-in development behavior. `Required` is the production mode: missing storage
/// fails startup and every accepted write is acknowledged by the SQLite worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityMode {
    Disabled,
    #[default]
    BestEffort,
    Required,
}

impl DurabilityMode {
    fn from_env() -> Self {
        match std::env::var(ENV_DURABILITY)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "required" | "strict" | "1" | "true" | "yes" => Self::Required,
            "off" | "disabled" | "0" | "false" | "no" => Self::Disabled,
            _ => Self::BestEffort,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DashboardHistoryError(pub String);

impl std::fmt::Display for DashboardHistoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DashboardHistoryError {}

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
    PersistFlow {
        observed_at_ms: i64,
        phase: FlowMutationPhase,
        record_seq: i64,
        summary: Box<SnapshotFlowSummary>,
    },
    IndexArtifact {
        api_call_id: String,
        path: PathBuf,
        bytes: i64,
        modified_ms: i64,
    },
}

#[derive(Debug)]
struct QueuedCommand {
    command: Command,
    ack: oneshot::Sender<Result<(), String>>,
}

#[derive(Debug)]
struct Inner {
    path: PathBuf,
    artifact_dir: Option<PathBuf>,
    sender: mpsc::Sender<QueuedCommand>,
    failed_writes: AtomicU64,
    mode: DurabilityMode,
    bootstrap_last_activity: Option<crate::metrics::LastActivitySample>,
    bootstrap_cursors: DomainCursors,
    bootstrap_terminals: Arc<Vec<SnapshotFlowSummary>>,
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
    pub resolution_ms: u64,
    pub cut_kind: String,
    pub archive_event_id: u64,
}

#[derive(Debug, Clone, Default)]
pub struct HistoricalCutSelection {
    pub cuts: Vec<HistoricalCut>,
    pub retained_cuts: usize,
    pub oldest_at_ms: Option<u128>,
    pub newest_at_ms: Option<u128>,
}

#[derive(Debug, Clone, Default)]
pub struct DurableFlowFilter {
    pub status: Option<FlowStatus>,
    pub model: Option<String>,
    pub upstream: Option<String>,
    pub client: Option<String>,
    pub search: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DurableFlowPage {
    pub summaries: Vec<SnapshotFlowSummary>,
    pub total: usize,
    pub next_cursor: Option<String>,
    pub as_of_event_id: u64,
    pub generated_at_ms: u128,
}

#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct DurableFacetCount {
    pub key: String,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct DurableFailureCount {
    pub provider: String,
    pub model: String,
    pub reason: String,
    pub count: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct DurableClientRollup {
    pub key: String,
    pub count: usize,
    pub failed: usize,
    pub source: Option<ClientSource>,
    pub cost_usd: Option<f64>,
    pub cost_confidence: TerminalCostConfidence,
    pub priced: usize,
    pub average_latency_ms: Option<f64>,
    pub timed: usize,
}

#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct DurableContextRollup {
    pub measurable: usize,
    pub near_limit: usize,
    pub over_limit: usize,
    pub peak_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct DurableFlowRollup {
    pub total: usize,
    pub models: Vec<DurableFacetCount>,
    pub upstreams: Vec<DurableFacetCount>,
    pub clients: Vec<DurableClientRollup>,
    pub unattributed: usize,
    pub statuses: Vec<DurableFacetCount>,
    pub failures: Vec<DurableFailureCount>,
    pub context: DurableContextRollup,
    pub as_of_event_id: u64,
    pub generated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableCursor {
    started_ms: i64,
    api_call_id: String,
    as_of_event_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableHistoryMetadata {
    pub oldest_at_ms: Option<u128>,
    pub newest_at_ms: Option<u128>,
    pub retained_cuts: usize,
    pub database_bytes: usize,
    pub dropped_writes: u64,
    pub archived_flows: usize,
    pub terminal_flows: usize,
    pub artifact_bytes: usize,
    pub last_commit_ms: Option<u128>,
    pub pending_commits: usize,
    pub mode: DurabilityMode,
    pub activity_cuts: usize,
    pub fine_cuts: usize,
    pub minute_cuts: usize,
    pub coarse_cuts: usize,
}

impl Default for DurableHistoryMetadata {
    fn default() -> Self {
        Self {
            oldest_at_ms: None,
            newest_at_ms: None,
            retained_cuts: 0,
            database_bytes: 0,
            dropped_writes: 0,
            archived_flows: 0,
            terminal_flows: 0,
            artifact_bytes: 0,
            last_commit_ms: None,
            pending_commits: 0,
            mode: DurabilityMode::Disabled,
            activity_cuts: 0,
            fine_cuts: 0,
            minute_cuts: 0,
            coarse_cuts: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CutLookup {
    Latest,
    AtOrBefore(u64),
    AtOrAfter(u64),
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
        let mode = DurabilityMode::from_env();
        if !debug_ui_enabled || mode == DurabilityMode::Disabled {
            return Self::disabled();
        }
        let path = std::env::var_os(ENV_HISTORY_DB)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let Some(path) = path else {
            if mode == DurabilityMode::Required {
                panic!("{ENV_HISTORY_DB} is required when {ENV_DURABILITY}=required");
            }
            return Self::disabled();
        };
        if mode == DurabilityMode::Required && artifact_dir.is_none() {
            panic!("turn_capture_dir is required when {ENV_DURABILITY}=required");
        }
        if mode == DurabilityMode::Required {
            if !path.is_file() {
                panic!(
                    "required dashboard history database is missing: {}",
                    path.display()
                );
            }
            if !artifact_dir.as_deref().is_some_and(Path::is_dir) {
                panic!("required dashboard artifact directory is missing");
            }
        }
        match Self::open(path, artifact_dir, FINE_CUT_RETENTION_MS, mode) {
            Ok(history) => history,
            Err(error) => {
                if mode == DurabilityMode::Required {
                    panic!("required dashboard archive failed to initialize: {error}");
                }
                tracing::error!(%error, "dashboard history disabled: SQLite initialization failed");
                Self::disabled()
            }
        }
    }

    fn open(
        path: PathBuf,
        artifact_dir: Option<PathBuf>,
        retention_ms: i64,
        mode: DurabilityMode,
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
            if mode == DurabilityMode::Required {
                verify_artifact_storage(dir)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            }
            recover_orphan_capture_files(dir)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            recover_pending_artifacts(&connection, dir)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            index_existing_artifacts(&mut connection, dir)?;
        }
        let bootstrap_last_activity = latest_activity_from_connection(&connection)?;
        let (bootstrap_cursors, bootstrap_terminals) = bootstrap_archive_state(&connection)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }

        let (sender, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let expect_artifact = artifact_dir.is_some();
        std::thread::Builder::new()
            .name("dashboard-history".to_string())
            .spawn(move || writer_loop(connection, receiver, retention_ms, expect_artifact))
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
                failed_writes: AtomicU64::new(0),
                mode,
                bootstrap_last_activity,
                bootstrap_cursors,
                bootstrap_terminals: Arc::new(bootstrap_terminals),
            })),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn mode(&self) -> DurabilityMode {
        self.inner
            .as_ref()
            .map_or(DurabilityMode::Disabled, |inner| inner.mode)
    }

    pub fn is_required(&self) -> bool {
        self.mode() == DurabilityMode::Required
    }

    pub fn bootstrap_last_activity(&self) -> Option<crate::metrics::LastActivitySample> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.bootstrap_last_activity.clone())
    }

    pub fn bootstrap_cursors(&self) -> DomainCursors {
        self.inner
            .as_ref()
            .map_or_else(DomainCursors::default, |inner| inner.bootstrap_cursors)
    }

    pub fn bootstrap_terminal_summaries(&self) -> Arc<Vec<SnapshotFlowSummary>> {
        self.inner.as_ref().map_or_else(
            || Arc::new(Vec::new()),
            |inner| Arc::clone(&inner.bootstrap_terminals),
        )
    }

    pub async fn persist_cut(
        &self,
        snapshot: Arc<DashboardSnapshot>,
    ) -> Result<(), DashboardHistoryError> {
        if self.inner.is_none() {
            return Ok(());
        }
        let cut_id = match u64::try_from(snapshot.taken_at_ms) {
            Ok(value) => value,
            Err(_) => return Ok(()),
        };
        let Ok(snapshot_blob) = serde_cbor::to_vec(snapshot.as_ref()) else {
            return Err(DashboardHistoryError(format!(
                "failed to serialize dashboard history cut {cut_id}"
            )));
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
        self.send(Command::PersistCut {
            cut_id,
            taken_at_ms: cut_id as i64,
            monitor_seq: snapshot.cursors.monitor_seq.min(i64::MAX as u64) as i64,
            snapshot: snapshot_blob,
            summaries,
        })
        .await
    }

    pub async fn persist_monitor_update(
        &self,
        update: DebugUpdate,
    ) -> Result<(), DashboardHistoryError> {
        if self.inner.is_none() {
            return Ok(());
        }
        let Ok(blob) = serde_cbor::to_vec(&update) else {
            return Err(DashboardHistoryError(
                "failed to serialize dashboard monitor update".to_string(),
            ));
        };
        self.send(Command::PersistMonitor {
            sequence: update.sequence.min(i64::MAX as u64) as i64,
            update: blob,
        })
        .await
    }

    pub async fn index_artifact(
        &self,
        api_call_id: String,
        path: PathBuf,
    ) -> Result<(), DashboardHistoryError> {
        if self.inner.is_none() {
            return Ok(());
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            return Err(DashboardHistoryError(format!(
                "dashboard artifact does not exist: {}",
                path.display()
            )));
        };
        let modified_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        self.send(Command::IndexArtifact {
            api_call_id,
            path,
            bytes: metadata.len().min(i64::MAX as u64) as i64,
            modified_ms,
        })
        .await
    }

    pub async fn persist_flow_mutation(
        &self,
        mutation: &FlowMutation,
    ) -> Result<(), DashboardHistoryError> {
        let summary = SnapshotFlowSummary::from_record(&mutation.record);
        let observed_at_ms = summary_observed_at_ms(&summary);
        self.send(Command::PersistFlow {
            observed_at_ms,
            phase: mutation.phase,
            record_seq: mutation.seq.min(i64::MAX as u64) as i64,
            summary: Box::new(summary),
        })
        .await
    }

    pub async fn persist_flow_summary(
        &self,
        summary: SnapshotFlowSummary,
        phase: FlowMutationPhase,
        record_seq: u64,
    ) -> Result<(), DashboardHistoryError> {
        let observed_at_ms = summary_observed_at_ms(&summary);
        self.send(Command::PersistFlow {
            observed_at_ms,
            phase,
            record_seq: record_seq.min(i64::MAX as u64) as i64,
            summary: Box::new(summary),
        })
        .await
    }

    async fn send(&self, command: Command) -> Result<(), DashboardHistoryError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        if inner
            .sender
            .send(QueuedCommand {
                command,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            inner.failed_writes.fetch_add(1, Ordering::Relaxed);
            return Err(DashboardHistoryError(
                "dashboard history writer is unavailable".to_string(),
            ));
        }
        match ack_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                inner.failed_writes.fetch_add(1, Ordering::Relaxed);
                Err(DashboardHistoryError(error))
            }
            Err(_) => {
                inner.failed_writes.fetch_add(1, Ordering::Relaxed);
                Err(DashboardHistoryError(
                    "dashboard history writer stopped before acknowledging a write".to_string(),
                ))
            }
        }
    }

    pub async fn latest_cut(&self) -> Option<HistoricalCut> {
        self.load_cut(CutLookup::Latest).await
    }

    pub async fn cut_at_or_before(&self, at_ms: u64) -> Option<HistoricalCut> {
        self.load_cut(CutLookup::AtOrBefore(at_ms)).await
    }

    pub async fn cut_at_or_after(&self, at_ms: u64) -> Option<HistoricalCut> {
        let inner = self.inner.as_ref()?.clone();
        tokio::task::spawn_blocking(move || load_cut_sync(&inner.path, CutLookup::AtOrAfter(at_ms)))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
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

    /// Read a bounded scrubber projection without deserializing every retained snapshot.
    /// SQLite selects evenly spaced periodic rows while always returning range boundaries
    /// and every permanent activity anchor.
    pub async fn sampled_cuts_between(
        &self,
        from_ms: Option<u64>,
        to_ms: Option<u64>,
        limit: usize,
    ) -> HistoricalCutSelection {
        let Some(inner) = self.inner.clone() else {
            return HistoricalCutSelection::default();
        };
        tokio::task::spawn_blocking(move || {
            load_sampled_cuts_sync(&inner.path, from_ms, to_ms, limit)
        })
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

    /// Server-paged durable latest-flow population. The opaque cursor freezes the
    /// archive event watermark from page one, so concurrent arrivals cannot duplicate
    /// or skip rows on later pages.
    pub async fn latest_flow_page(
        &self,
        filter: DurableFlowFilter,
        cursor: Option<String>,
        limit: usize,
        offset: usize,
    ) -> Result<DurableFlowPage, DashboardHistoryError> {
        let Some(inner) = self.inner.clone() else {
            return Ok(DurableFlowPage::default());
        };
        tokio::task::spawn_blocking(move || {
            load_latest_flow_page_sync(&inner.path, filter, cursor, limit.clamp(1, 500), offset)
        })
        .await
        .map_err(|error| DashboardHistoryError(format!("durable flow query panicked: {error}")))?
        .map_err(|error| DashboardHistoryError(error.to_string()))
    }

    pub async fn latest_terminal_summary(&self) -> Option<SnapshotFlowSummary> {
        let inner = self.inner.clone()?;
        tokio::task::spawn_blocking(move || load_latest_terminal_summary_sync(&inner.path))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub async fn latest_terminal_matching(
        &self,
        filter: DurableFlowFilter,
    ) -> Option<SnapshotFlowSummary> {
        let inner = self.inner.clone()?;
        tokio::task::spawn_blocking(move || load_latest_terminal_matching_sync(&inner.path, filter))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    async fn load_cut(&self, lookup: CutLookup) -> Option<HistoricalCut> {
        let inner = self.inner.clone()?;
        tokio::task::spawn_blocking(move || load_cut_sync(&inner.path, lookup))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub async fn flow_rollup(
        &self,
        filter: DurableFlowFilter,
    ) -> Result<DurableFlowRollup, DashboardHistoryError> {
        let Some(inner) = self.inner.clone() else {
            return Ok(DurableFlowRollup::default());
        };
        tokio::task::spawn_blocking(move || load_flow_rollup_sync(&inner.path, filter))
            .await
            .map_err(|error| DashboardHistoryError(format!("flow rollup panicked: {error}")))?
            .map_err(|error| DashboardHistoryError(error.to_string()))
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
        let failed = inner.failed_writes.load(Ordering::Relaxed);
        let mode = inner.mode;
        tokio::task::spawn_blocking(move || metadata_sync(&inner.path, failed, mode))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn enabled_for_test(path: PathBuf, artifact_dir: Option<PathBuf>) -> Self {
        Self::open(
            path,
            artifact_dir,
            24 * 60 * 60 * 1000,
            DurabilityMode::BestEffort,
        )
        .expect("test history opens")
    }

    #[cfg(test)]
    pub(crate) fn required_for_test(path: PathBuf, artifact_dir: PathBuf) -> Self {
        Self::open(
            path,
            Some(artifact_dir),
            FINE_CUT_RETENTION_MS,
            DurabilityMode::Required,
        )
        .expect("required test history opens")
    }

    #[cfg(test)]
    fn enabled_for_test_with_retention(path: PathBuf, retention_ms: i64) -> Self {
        Self::open(path, None, retention_ms, DurabilityMode::BestEffort)
            .expect("test history opens")
    }
}

fn summary_observed_at_ms(summary: &SnapshotFlowSummary) -> i64 {
    let mut latest = summary.finished_ms.unwrap_or(summary.started_ms);
    for timestamp in [
        summary.phases.ingress_ms,
        summary.phases.normalization_done_ms,
        summary.phases.routing_decision_ms,
        summary.phases.first_content_delta_ms,
        summary.phases.stream_end_ms,
        summary.phases.finalize_ms,
    ]
    .into_iter()
    .flatten()
    {
        latest = latest.max(timestamp);
    }
    for attempt in &summary.attempts {
        latest = latest.max(attempt.end_ms);
    }
    latest.min(i64::MAX as u128) as i64
}

fn verify_artifact_storage(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let probe = dir.join(format!(".llmconduit-write-probe-{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    file.sync_all()?;
    std::fs::remove_file(probe)?;
    std::fs::File::open(dir)?.sync_all()
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
                Ok(update) => {
                    if let Err(error) = history.persist_monitor_update(update).await {
                        tracing::error!(%error, "failed to persist dashboard monitor update");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "dashboard history monitor subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

/// Persist every authoritative FlowStore mutation. The live broadcast stays the
/// low-latency transport, while this acknowledged subscriber makes SQLite the
/// durable ledger. Open and terminal mutations are also persisted directly by the
/// request path in required mode; the unique `(api_call_id, revision)` key makes the
/// duplicate delivery idempotent.
pub fn spawn_flow_history_task(
    history: DashboardHistory,
    mut receiver: tokio::sync::broadcast::Receiver<FlowMutation>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !history.is_enabled() {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(mutation) => {
                    if let Err(error) = history.persist_flow_mutation(&mutation).await {
                        tracing::error!(%error, "failed to persist dashboard flow mutation");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::error!(skipped, "dashboard history flow subscriber lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

fn initialize(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    // Dashboard durability is a correctness contract in required mode. FULL keeps a
    // committed flow durable across process and machine crashes; WAL retains reader
    // concurrency while the single writer serializes lifecycle commits.
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "busy_timeout", 5_000i64)?;
    let existing_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if existing_version > SQLITE_SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidQuery);
    }
    // Schema creation, v1 backfill, open-turn recovery, and the user-version bump are
    // one rollback-safe migration. Nested lifecycle writes use SQLite SAVEPOINTs, so a
    // malformed legacy row or disk failure can never leave a half-v2 database that an
    // older/newer process mistakes for a completed migration.
    connection.execute_batch("SAVEPOINT dashboard_schema_v2")?;
    let migration = (|| -> rusqlite::Result<()> {
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
         );
         CREATE TABLE IF NOT EXISTS flow_events (
            event_id INTEGER PRIMARY KEY AUTOINCREMENT,
            api_call_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            record_seq INTEGER NOT NULL,
            observed_at_ms INTEGER NOT NULL,
            phase TEXT NOT NULL,
            summary BLOB NOT NULL,
            UNIQUE(api_call_id, revision)
         );
         CREATE INDEX IF NOT EXISTS flow_events_asof_idx
            ON flow_events(observed_at_ms, event_id);
         CREATE INDEX IF NOT EXISTS flow_events_flow_idx
            ON flow_events(api_call_id, revision DESC);
         CREATE TABLE IF NOT EXISTS flows_latest (
            api_call_id TEXT PRIMARY KEY,
            event_id INTEGER NOT NULL,
            revision INTEGER NOT NULL,
            record_seq INTEGER NOT NULL,
            observed_at_ms INTEGER NOT NULL,
            started_ms INTEGER NOT NULL,
            finished_ms INTEGER,
            status TEXT NOT NULL,
            model_requested TEXT,
            model_served TEXT,
            upstream_target TEXT,
            client_label TEXT,
            response_id TEXT,
            uri TEXT NOT NULL DEFAULT '',
            search_text TEXT NOT NULL DEFAULT '',
            summary BLOB NOT NULL,
            FOREIGN KEY(event_id) REFERENCES flow_events(event_id)
         );
         CREATE INDEX IF NOT EXISTS flows_latest_started_idx
            ON flows_latest(started_ms DESC, api_call_id DESC);
         CREATE INDEX IF NOT EXISTS flows_latest_status_idx
            ON flows_latest(status, started_ms DESC);
         CREATE INDEX IF NOT EXISTS flows_latest_model_idx
            ON flows_latest(model_served, started_ms DESC);
         CREATE INDEX IF NOT EXISTS flows_latest_upstream_idx
            ON flows_latest(upstream_target, started_ms DESC);
         CREATE INDEX IF NOT EXISTS flows_latest_client_idx
            ON flows_latest(client_label, started_ms DESC);
         CREATE TABLE IF NOT EXISTS terminal_facts (
            api_call_id TEXT PRIMARY KEY,
            event_id INTEGER NOT NULL,
            finished_ms INTEGER NOT NULL,
            status TEXT NOT NULL,
            model_requested TEXT,
            model_served TEXT,
            upstream_target TEXT,
            client_label TEXT,
            client_source TEXT,
            endpoint TEXT NOT NULL DEFAULT '',
            elapsed_ms INTEGER,
            prompt_tokens INTEGER,
            completion_tokens INTEGER,
            total_tokens INTEGER,
            cached_tokens INTEGER,
            reasoning_tokens INTEGER,
            cost_usd REAL,
            cost_confidence TEXT NOT NULL,
            terminal_reason TEXT,
            failure_class TEXT,
            effective_route_limit INTEGER,
            cache_price_impact_usd REAL,
            search_text TEXT NOT NULL DEFAULT '',
            summary BLOB NOT NULL,
            FOREIGN KEY(event_id) REFERENCES flow_events(event_id)
         );
         CREATE INDEX IF NOT EXISTS terminal_facts_finished_idx
            ON terminal_facts(finished_ms DESC);
         CREATE INDEX IF NOT EXISTS terminal_facts_provider_idx
            ON terminal_facts(upstream_target, finished_ms DESC);
         CREATE TABLE IF NOT EXISTS attempt_facts (
            api_call_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            provider TEXT,
            model TEXT,
            start_ms INTEGER NOT NULL,
            end_ms INTEGER NOT NULL,
            duration_ms INTEGER,
            status TEXT NOT NULL,
            error_class TEXT,
            failover_reason TEXT,
            PRIMARY KEY(api_call_id, ordinal),
            FOREIGN KEY(api_call_id) REFERENCES terminal_facts(api_call_id)
               ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS attempt_facts_provider_idx
            ON attempt_facts(provider, end_ms DESC);
         CREATE TABLE IF NOT EXISTS terminal_theater (
            api_call_id TEXT PRIMARY KEY,
            response_id TEXT,
            terminal_at_ms INTEGER NOT NULL,
            projection BLOB NOT NULL
         );
         CREATE INDEX IF NOT EXISTS terminal_theater_latest_idx
            ON terminal_theater(terminal_at_ms DESC);
         CREATE TABLE IF NOT EXISTS artifact_manifests (
            api_call_id TEXT PRIMARY KEY,
            path TEXT NOT NULL,
            bytes INTEGER NOT NULL,
            sha256 TEXT,
            committed_at_ms INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS pending_turn_commits (
            api_call_id TEXT PRIMARY KEY,
            created_at_ms INTEGER NOT NULL,
            payload BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS archive_meta (
            key TEXT PRIMARY KEY,
            integer_value INTEGER,
            blob_value BLOB
         );",
        )?;
        ensure_column(
            connection,
            "dashboard_cuts",
            "resolution_ms",
            "INTEGER NOT NULL DEFAULT 5000",
        )?;
        ensure_column(
            connection,
            "dashboard_cuts",
            "cut_kind",
            "TEXT NOT NULL DEFAULT 'periodic'",
        )?;
        ensure_column(
            connection,
            "dashboard_cuts",
            "archive_event_id",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        for (table, column, declaration) in [
            ("flows_latest", "response_id", "TEXT"),
            ("flows_latest", "uri", "TEXT NOT NULL DEFAULT ''"),
            ("flows_latest", "search_text", "TEXT NOT NULL DEFAULT ''"),
            ("terminal_facts", "client_source", "TEXT"),
            ("terminal_facts", "endpoint", "TEXT NOT NULL DEFAULT ''"),
            ("terminal_facts", "failure_class", "TEXT"),
            ("terminal_facts", "effective_route_limit", "INTEGER"),
            ("terminal_facts", "cache_price_impact_usd", "REAL"),
            ("terminal_facts", "search_text", "TEXT NOT NULL DEFAULT ''"),
        ] {
            ensure_column(connection, table, column, declaration)?;
        }
        // Create indexes that reference additive columns only after `ensure_column`.
        // This keeps reopening an early v2 database transactional instead of failing
        // before the compatibility migration can add `search_text`.
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS flows_latest_search_idx
                ON flows_latest(search_text);",
        )?;
        if existing_version < 2 {
            backfill_v2(connection)?;
            backfill_legacy_cuts(connection)?;
            compact_periodic_cuts(connection, epoch_ms_i64(), FINE_CUT_RETENTION_MS)?;
        }
        backfill_search_text(connection)?;
        recover_open_flows(connection)?;
        connection.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
        Ok(())
    })();
    match migration {
        Ok(()) => connection.execute_batch("RELEASE dashboard_schema_v2"),
        Err(error) => {
            let _ = connection
                .execute_batch("ROLLBACK TO dashboard_schema_v2; RELEASE dashboard_schema_v2");
            Err(error)
        }
    }
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !names.iter().any(|name| name == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration};"
        ))?;
    }
    Ok(())
}

fn backfill_v2(connection: &mut Connection) -> rusqlite::Result<()> {
    let rows = {
        let mut statement = connection.prepare(
            "SELECT observed_at_ms, summary FROM flow_versions
             ORDER BY observed_at_ms ASC, api_call_id ASC, revision ASC",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (observed_at_ms, blob) in rows {
        let Ok(summary) = serde_cbor::from_slice::<SnapshotFlowSummary>(&blob) else {
            continue;
        };
        let phase = if summary.status == FlowStatus::Open {
            FlowMutationPhase::Open
        } else {
            FlowMutationPhase::Terminal
        };
        persist_flow_sync(connection, observed_at_ms, phase, 0, summary, false)?;
    }
    Ok(())
}

/// Recover the cut semantics that v1 stored inside each snapshot but did not project
/// into indexed columns. Activity-bearing cuts become permanent anchors, while every
/// cut receives the exact terminal-event watermark its timestamp could observe.
fn backfill_legacy_cuts(connection: &Connection) -> rusqlite::Result<()> {
    let rows = {
        let mut statement = connection.prepare(
            "SELECT cut_id, taken_at_ms, snapshot FROM dashboard_cuts ORDER BY taken_at_ms ASC",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut latest_activity_anchor = None::<i64>;
    for (cut_id, taken_at_ms, blob) in rows {
        let Ok(snapshot) = serde_cbor::from_slice::<DashboardSnapshot>(&blob) else {
            continue;
        };
        let instantaneous_activity = snapshot.instant.active_streams_now > 0
            || snapshot.instant.accepted_requests > 0
            || snapshot.instant.terminal_requests > 0;
        let last_activity_ms = snapshot
            .last_activity
            .as_ref()
            .map(|sample| sample.at_ms.min(i64::MAX as u128) as i64);
        let has_new_activity = instantaneous_activity
            || last_activity_ms.is_some_and(|activity| {
                latest_activity_anchor.is_none_or(|anchor| activity > anchor)
            });
        if has_new_activity {
            connection.execute(
                "UPDATE dashboard_cuts SET cut_kind = 'activity', resolution_ms = 0
                 WHERE cut_id = ?1",
                [cut_id],
            )?;
            // Match normal cut insertion: the anchor comparison is against the cut's
            // timestamp so later idle copies of the same `last_activity` stay periodic.
            latest_activity_anchor = Some(taken_at_ms);
        }
    }
    connection.execute(
        "UPDATE dashboard_cuts
         SET archive_event_id = COALESCE((
             SELECT MAX(event_id) FROM flow_events
             WHERE observed_at_ms <= dashboard_cuts.taken_at_ms
         ), 0)",
        [],
    )?;
    Ok(())
}

fn recover_open_flows(connection: &mut Connection) -> rusqlite::Result<()> {
    let rows = {
        let mut statement = connection.prepare(
            "SELECT observed_at_ms, record_seq, summary FROM flows_latest WHERE status = 'open'",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (observed_at_ms, record_seq, blob) in rows {
        let Ok(mut summary) = serde_cbor::from_slice::<SnapshotFlowSummary>(&blob) else {
            continue;
        };
        let finished_ms = observed_at_ms.max(0) as u128;
        summary.revision = summary.revision.saturating_add(1);
        summary.status = FlowStatus::Cancelled;
        summary.finished_ms = Some(finished_ms);
        summary.elapsed_ms = Some(finished_ms.saturating_sub(summary.started_ms));
        summary.terminal_reason = Some("gateway_restart".to_string());
        if summary.phases.finalize_ms.is_none() {
            summary.phases.finalize_ms = Some(finished_ms);
            summary.phases.finalize_offset_ms = summary.elapsed_ms;
        }
        persist_flow_sync(
            connection,
            observed_at_ms,
            FlowMutationPhase::Terminal,
            record_seq,
            summary,
            false,
        )?;
    }
    Ok(())
}

fn backfill_search_text(connection: &Connection) -> rusqlite::Result<()> {
    let rows = {
        let mut statement = connection
            .prepare("SELECT api_call_id, summary FROM flows_latest WHERE search_text = ''")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (api_call_id, blob) in rows {
        let Ok(summary) = serde_cbor::from_slice::<SnapshotFlowSummary>(&blob) else {
            continue;
        };
        let search_text = summary_search_text(&summary);
        connection.execute(
            "UPDATE flows_latest SET search_text = ?1 WHERE api_call_id = ?2",
            params![search_text, api_call_id],
        )?;
        connection.execute(
            "UPDATE terminal_facts SET search_text = ?1 WHERE api_call_id = ?2",
            params![search_text, api_call_id],
        )?;
    }
    Ok(())
}

fn writer_loop(
    mut connection: Connection,
    mut receiver: mpsc::Receiver<QueuedCommand>,
    fine_retention_ms: i64,
    expect_artifact: bool,
) {
    while let Some(queued) = receiver.blocking_recv() {
        let result = match queued.command {
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
                fine_retention_ms,
            ),
            Command::PersistMonitor { sequence, update } => connection
                .execute(
                    "INSERT OR REPLACE INTO monitor_updates(sequence, update_blob) VALUES (?1, ?2)",
                    params![sequence, update],
                )
                .map(|_| ()),
            Command::PersistFlow {
                observed_at_ms,
                phase,
                record_seq,
                summary,
            } => persist_flow_sync(
                &mut connection,
                observed_at_ms,
                phase,
                record_seq,
                *summary,
                expect_artifact,
            ),
            Command::IndexArtifact {
                api_call_id,
                path,
                bytes,
                modified_ms,
            } => index_artifact_sync(&mut connection, &api_call_id, &path, bytes, modified_ms),
        };
        let ack = match result {
            Ok(()) => Ok(()),
            Err(error) => {
                tracing::error!(%error, "dashboard history writer command failed");
                Err(error.to_string())
            }
        };
        if queued.ack.send(ack).is_err() {
            tracing::debug!("dashboard history write acknowledgement receiver dropped");
        }
    }
}

fn persist_flow_sync(
    connection: &mut Connection,
    observed_at_ms: i64,
    phase: FlowMutationPhase,
    record_seq: i64,
    summary: SnapshotFlowSummary,
    expect_artifact: bool,
) -> rusqlite::Result<()> {
    let blob = serde_cbor::to_vec(&summary)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let revision = summary.revision.min(i64::MAX as u64) as i64;
    let started_ms = summary.started_ms.min(i64::MAX as u128) as i64;
    let finished_ms = summary
        .finished_ms
        .map(|value| value.min(i64::MAX as u128) as i64);
    let client_source = summary.client_source.as_ref().map(serde_enum_key);
    let failure_class = summary
        .attempts
        .iter()
        .rev()
        .find_map(|attempt| attempt.error_class.as_ref().map(serde_enum_key))
        .unwrap_or_else(|| bounded_terminal_reason(summary.terminal_reason.as_deref()));
    let search_text = summary_search_text(&summary);
    // SAVEPOINT composes with the transactional v1→v2 migration while retaining
    // atomic lifecycle writes during normal operation.
    let transaction = connection.savepoint()?;
    transaction.execute(
        "INSERT OR IGNORE INTO flow_versions(api_call_id, revision, observed_at_ms, summary)
         VALUES (?1, ?2, ?3, ?4)",
        params![summary.api_call_id, revision, observed_at_ms, blob],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO flow_events(
             api_call_id, revision, record_seq, observed_at_ms, phase, summary
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            summary.api_call_id,
            revision,
            record_seq,
            observed_at_ms,
            flow_phase_key(phase),
            blob
        ],
    )?;
    let event_id: i64 = transaction.query_row(
        "SELECT event_id FROM flow_events WHERE api_call_id = ?1 AND revision = ?2",
        params![summary.api_call_id, revision],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO flows_latest(
             api_call_id, event_id, revision, record_seq, observed_at_ms, started_ms,
             finished_ms, status, model_requested, model_served, upstream_target,
             client_label, response_id, uri, search_text, summary
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(api_call_id) DO UPDATE SET
             event_id = excluded.event_id,
             revision = excluded.revision,
             record_seq = excluded.record_seq,
             observed_at_ms = excluded.observed_at_ms,
             started_ms = excluded.started_ms,
             finished_ms = excluded.finished_ms,
             status = excluded.status,
             model_requested = excluded.model_requested,
             model_served = excluded.model_served,
             upstream_target = excluded.upstream_target,
             client_label = excluded.client_label,
             response_id = excluded.response_id,
             uri = excluded.uri,
             search_text = excluded.search_text,
             summary = excluded.summary
         WHERE excluded.revision >= flows_latest.revision",
        params![
            summary.api_call_id,
            event_id,
            revision,
            record_seq,
            observed_at_ms,
            started_ms,
            finished_ms,
            flow_status_key(summary.status),
            summary.model_requested,
            summary.model_served,
            summary.upstream_target,
            summary.client_label,
            summary.response_id,
            summary.uri,
            search_text,
            blob,
        ],
    )?;

    if phase == FlowMutationPhase::Terminal || summary.status != FlowStatus::Open {
        let usage = summary.usage;
        transaction.execute(
            "INSERT INTO terminal_facts(
                 api_call_id, event_id, finished_ms, status, model_requested,
                 model_served, upstream_target, client_label, client_source, endpoint, elapsed_ms,
                 prompt_tokens, completion_tokens, total_tokens, cached_tokens,
                 reasoning_tokens, cost_usd, cost_confidence, terminal_reason,
                 failure_class, effective_route_limit, cache_price_impact_usd, summary
                 , search_text
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)
             ON CONFLICT(api_call_id) DO UPDATE SET
                 event_id = excluded.event_id,
                 finished_ms = excluded.finished_ms,
                 status = excluded.status,
                 model_requested = excluded.model_requested,
                 model_served = excluded.model_served,
                 upstream_target = excluded.upstream_target,
                 client_label = excluded.client_label,
                 client_source = excluded.client_source,
                 endpoint = excluded.endpoint,
                 elapsed_ms = excluded.elapsed_ms,
                 prompt_tokens = excluded.prompt_tokens,
                 completion_tokens = excluded.completion_tokens,
                 total_tokens = excluded.total_tokens,
                 cached_tokens = excluded.cached_tokens,
                 reasoning_tokens = excluded.reasoning_tokens,
                 cost_usd = excluded.cost_usd,
                 cost_confidence = excluded.cost_confidence,
                 terminal_reason = excluded.terminal_reason,
                 failure_class = excluded.failure_class,
                 effective_route_limit = excluded.effective_route_limit,
                 cache_price_impact_usd = excluded.cache_price_impact_usd,
                 summary = excluded.summary,
                 search_text = excluded.search_text",
            params![
                summary.api_call_id,
                event_id,
                finished_ms.unwrap_or(observed_at_ms),
                flow_status_key(summary.status),
                summary.model_requested,
                summary.model_served,
                summary.upstream_target,
                summary.client_label,
                client_source,
                summary.uri,
                summary
                    .elapsed_ms
                    .map(|value| value.min(i64::MAX as u128) as i64),
                usage.map(|value| value.prompt),
                usage.map(|value| value.completion),
                usage.map(|value| value.total),
                usage.and_then(|value| value.cached),
                usage.and_then(|value| value.reasoning),
                summary.terminal_cost_usd,
                cost_confidence_key(summary.terminal_cost_confidence),
                summary.terminal_reason,
                failure_class,
                summary.effective_route_limit,
                summary.cache_price_impact_usd,
                blob,
                search_text,
            ],
        )?;
        transaction.execute(
            "DELETE FROM attempt_facts WHERE api_call_id = ?1",
            [&summary.api_call_id],
        )?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO attempt_facts(
                     api_call_id, ordinal, provider, model, start_ms, end_ms,
                     duration_ms, status, error_class, failover_reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for (ordinal, attempt) in summary.attempts.iter().enumerate() {
                statement.execute(params![
                    summary.api_call_id,
                    ordinal.min(i64::MAX as usize) as i64,
                    attempt.provider,
                    attempt.model,
                    attempt.start_ms.min(i64::MAX as u128) as i64,
                    attempt.end_ms.min(i64::MAX as u128) as i64,
                    attempt
                        .duration_ms
                        .map(|value| value.min(i64::MAX as u128) as i64),
                    serde_enum_key(&attempt.status),
                    attempt.error_class.as_ref().map(serde_enum_key),
                    attempt.failover_reason.as_ref().map(serde_enum_key),
                ])?;
            }
        }
        transaction.execute(
            "INSERT INTO terminal_theater(api_call_id, response_id, terminal_at_ms, projection)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(api_call_id) DO UPDATE SET
                response_id = excluded.response_id,
                terminal_at_ms = excluded.terminal_at_ms,
                projection = excluded.projection",
            params![
                summary.api_call_id,
                summary.response_id,
                finished_ms.unwrap_or(observed_at_ms),
                blob,
            ],
        )?;
        if expect_artifact {
            transaction.execute(
                "INSERT INTO pending_turn_commits(api_call_id, created_at_ms, payload)
                 SELECT ?1, ?2, ?3
                 WHERE NOT EXISTS (
                    SELECT 1 FROM artifact_manifests WHERE api_call_id = ?1
                 )
                 ON CONFLICT(api_call_id) DO UPDATE SET
                    created_at_ms = excluded.created_at_ms,
                    payload = excluded.payload",
                params![summary.api_call_id, observed_at_ms, blob],
            )?;
        }
    }
    transaction.execute(
        "INSERT INTO archive_meta(key, integer_value) VALUES ('last_event_id', ?1)
         ON CONFLICT(key) DO UPDATE SET integer_value = MAX(integer_value, excluded.integer_value)",
        [event_id],
    )?;
    transaction.execute(
        "INSERT INTO archive_meta(key, integer_value) VALUES ('last_flow_seq', ?1)
         ON CONFLICT(key) DO UPDATE SET integer_value = MAX(integer_value, excluded.integer_value)",
        [record_seq],
    )?;
    transaction.execute(
        "INSERT INTO archive_meta(key, integer_value) VALUES ('last_commit_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET integer_value = excluded.integer_value",
        [epoch_ms_i64()],
    )?;
    transaction.commit()
}

fn index_artifact_sync(
    connection: &mut Connection,
    api_call_id: &str,
    path: &Path,
    bytes: i64,
    modified_ms: i64,
) -> rusqlite::Result<()> {
    use sha2::{Digest, Sha256};
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let checksum = format!("{:x}", hash.finalize());
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT OR REPLACE INTO artifacts(api_call_id, path, bytes, modified_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![api_call_id, path.to_string_lossy(), bytes, modified_ms],
    )?;
    transaction.execute(
        "INSERT INTO artifact_manifests(api_call_id, path, bytes, sha256, committed_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(api_call_id) DO UPDATE SET
            path = excluded.path,
            bytes = excluded.bytes,
            sha256 = excluded.sha256,
            committed_at_ms = excluded.committed_at_ms",
        params![
            api_call_id,
            path.to_string_lossy(),
            bytes,
            checksum,
            modified_ms,
        ],
    )?;
    transaction.execute(
        "DELETE FROM pending_turn_commits WHERE api_call_id = ?1",
        [api_call_id],
    )?;
    transaction.execute(
        "INSERT INTO archive_meta(key, integer_value) VALUES ('last_commit_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET integer_value = excluded.integer_value",
        [epoch_ms_i64()],
    )?;
    transaction.commit()
}

fn flow_status_key(status: FlowStatus) -> &'static str {
    match status {
        FlowStatus::Open => "open",
        FlowStatus::Completed => "completed",
        FlowStatus::Failed => "failed",
        FlowStatus::Cancelled => "cancelled",
    }
}

fn flow_phase_key(phase: FlowMutationPhase) -> &'static str {
    match phase {
        FlowMutationPhase::Open => "open",
        FlowMutationPhase::Progress => "progress",
        FlowMutationPhase::Terminal => "terminal",
    }
}

fn cost_confidence_key(confidence: crate::dashboard_flow::TerminalCostConfidence) -> &'static str {
    use crate::dashboard_flow::TerminalCostConfidence;
    match confidence {
        TerminalCostConfidence::Confident => "confident",
        TerminalCostConfidence::Estimated => "estimated",
        TerminalCostConfidence::Unavailable => "unavailable",
    }
}

fn serde_enum_key<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn epoch_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn persist_cut_sync(
    connection: &mut Connection,
    cut_id: u64,
    taken_at_ms: i64,
    monitor_seq: i64,
    snapshot: Vec<u8>,
    summaries: Vec<(String, i64, Vec<u8>)>,
    fine_retention_ms: i64,
) -> rusqlite::Result<()> {
    let decoded = serde_cbor::from_slice::<DashboardSnapshot>(&snapshot).ok();
    let instantaneous_activity = decoded.as_ref().is_some_and(|snapshot| {
        snapshot.instant.active_streams_now > 0
            || snapshot.instant.accepted_requests > 0
            || snapshot.instant.terminal_requests > 0
    });
    let last_activity_ms = decoded
        .as_ref()
        .and_then(|snapshot| snapshot.last_activity.as_ref())
        .map(|sample| sample.at_ms.min(i64::MAX as u128) as i64);
    let transaction = connection.transaction()?;
    let latest_activity_anchor: Option<i64> = transaction.query_row(
        "SELECT MAX(taken_at_ms) FROM dashboard_cuts WHERE cut_kind = 'activity'",
        [],
        |row| row.get(0),
    )?;
    let has_activity = instantaneous_activity
        || last_activity_ms
            .is_some_and(|activity| latest_activity_anchor.is_none_or(|anchor| activity > anchor));
    let archive_event_id: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(event_id), 0) FROM flow_events",
        [],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT OR REPLACE INTO dashboard_cuts(
             cut_id, taken_at_ms, monitor_seq, snapshot, resolution_ms, cut_kind,
             archive_event_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            cut_id.min(i64::MAX as u64) as i64,
            taken_at_ms,
            monitor_seq,
            snapshot,
            if has_activity { 0 } else { 5_000 },
            if has_activity { "activity" } else { "periodic" },
            archive_event_id,
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
    compact_periodic_cuts(&transaction, taken_at_ms, fine_retention_ms)?;
    transaction.execute(
        "INSERT INTO archive_meta(key, integer_value) VALUES ('last_commit_ms', ?1)
         ON CONFLICT(key) DO UPDATE SET integer_value = excluded.integer_value",
        [epoch_ms_i64()],
    )?;
    transaction.commit()
}

fn compact_periodic_cuts(
    transaction: &Connection,
    now_ms: i64,
    fine_retention_ms: i64,
) -> rusqlite::Result<()> {
    const THIRTY_DAYS_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
    const MINUTE_MS: i64 = 60 * 1_000;
    const FIFTEEN_MINUTES_MS: i64 = 15 * MINUTE_MS;
    let fine_cutoff = now_ms.saturating_sub(fine_retention_ms);
    let month_cutoff = now_ms.saturating_sub(THIRTY_DAYS_MS);

    // Activity anchors are never compacted. For idle presentation cuts, retain the
    // newest cut in each minute through day 30 and the newest cut in each 15-minute
    // bucket forever after that.
    transaction.execute(
        "DELETE FROM dashboard_cuts
         WHERE cut_kind = 'periodic'
           AND taken_at_ms < ?1
           AND taken_at_ms >= ?2
           AND cut_id NOT IN (
             SELECT MAX(cut_id) FROM dashboard_cuts
             WHERE cut_kind = 'periodic' AND taken_at_ms < ?1 AND taken_at_ms >= ?2
             GROUP BY taken_at_ms / ?3
           )",
        params![fine_cutoff, month_cutoff, MINUTE_MS],
    )?;
    transaction.execute(
        "UPDATE dashboard_cuts SET resolution_ms = ?1
         WHERE cut_kind = 'periodic' AND taken_at_ms < ?2 AND taken_at_ms >= ?3",
        params![MINUTE_MS, fine_cutoff, month_cutoff],
    )?;
    transaction.execute(
        "DELETE FROM dashboard_cuts
         WHERE cut_kind = 'periodic'
           AND taken_at_ms < ?1
           AND cut_id NOT IN (
             SELECT MAX(cut_id) FROM dashboard_cuts
             WHERE cut_kind = 'periodic' AND taken_at_ms < ?1
             GROUP BY taken_at_ms / ?2
           )",
        params![month_cutoff, FIFTEEN_MINUTES_MS],
    )?;
    transaction.execute(
        "UPDATE dashboard_cuts SET resolution_ms = ?1
         WHERE cut_kind = 'periodic' AND taken_at_ms < ?2",
        params![FIFTEEN_MINUTES_MS, month_cutoff],
    )?;
    Ok(())
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
            "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id FROM dashboard_cuts ORDER BY taken_at_ms DESC LIMIT 1",
            None,
        ),
        CutLookup::AtOrBefore(at) => (
            "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id FROM dashboard_cuts WHERE taken_at_ms <= ?1 ORDER BY taken_at_ms DESC LIMIT 1",
            Some(at.min(i64::MAX as u64) as i64),
        ),
        CutLookup::AtOrAfter(at) => (
            "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id FROM dashboard_cuts WHERE taken_at_ms >= ?1 ORDER BY taken_at_ms ASC LIMIT 1",
            Some(at.min(i64::MAX as u64) as i64),
        ),
        CutLookup::Id(id) => (
            "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id FROM dashboard_cuts WHERE cut_id = ?1 LIMIT 1",
            Some(id.min(i64::MAX as u64) as i64),
        ),
        CutLookup::Nearest(at) => {
            let at = at.min(i64::MAX as u64) as i64;
            let row: Option<(i64, Vec<u8>, i64, String, i64)> = connection
                .query_row(
                    "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id FROM dashboard_cuts
                     ORDER BY ABS(taken_at_ms - ?1), taken_at_ms ASC LIMIT 1",
                    [at],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .optional()?;
            return decode_cut(row);
        }
    };
    let row = if let Some(value) = value {
        connection
            .query_row(sql, [value], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .optional()?
    } else {
        connection
            .query_row(sql, [], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
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
        "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id FROM dashboard_cuts
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
                resolution_ms: row.get::<_, i64>(2)?.max(0) as u64,
                cut_kind: row.get(3)?,
                archive_event_id: row.get::<_, i64>(4)?.max(0) as u64,
            });
        }
    }
    Ok(cuts)
}

fn load_sampled_cuts_sync(
    path: &Path,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    limit: usize,
) -> rusqlite::Result<HistoricalCutSelection> {
    let connection = reader(path)?;
    let from = from_ms.unwrap_or(0).min(i64::MAX as u64) as i64;
    let to = to_ms.unwrap_or(i64::MAX as u64).min(i64::MAX as u64) as i64;
    let (count, oldest, newest): (i64, Option<i64>, Option<i64>) = connection.query_row(
        "SELECT COUNT(*), MIN(taken_at_ms), MAX(taken_at_ms)
         FROM dashboard_cuts WHERE taken_at_ms >= ?1 AND taken_at_ms <= ?2",
        params![from, to],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let (Some(oldest), Some(newest)) = (oldest, newest) else {
        return Ok(HistoricalCutSelection::default());
    };
    let mut selected = std::collections::BTreeMap::<i64, HistoricalCut>::new();
    {
        let mut statement = connection.prepare(
            "SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id
             FROM dashboard_cuts
             WHERE taken_at_ms >= ?1 AND taken_at_ms <= ?2
               AND (cut_kind = 'activity' OR taken_at_ms = ?3 OR taken_at_ms = ?4)
             ORDER BY taken_at_ms ASC",
        )?;
        let mut rows = statement.query(params![from, to, oldest, newest])?;
        while let Some(row) = rows.next()? {
            let decoded = decode_cut(Some((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            )))?;
            if let Some(cut) = decoded {
                selected.insert(cut.snapshot.taken_at_ms.min(i64::MAX as u128) as i64, cut);
            }
        }
    }
    let remaining = limit.saturating_sub(selected.len());
    if remaining > 0 {
        // Integer bucket transitions select at most `remaining` evenly spaced periodic
        // rows. Boundary rows are already mandatory above and excluded from this window.
        let mut statement = connection.prepare(
            "WITH periodic AS (
                 SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id,
                        taken_at_ms,
                        ROW_NUMBER() OVER (ORDER BY taken_at_ms ASC) AS rn,
                        COUNT(*) OVER () AS n
                 FROM dashboard_cuts
                 WHERE taken_at_ms >= ?1 AND taken_at_ms <= ?2
                   AND cut_kind != 'activity'
                   AND taken_at_ms != ?3 AND taken_at_ms != ?4
             )
             SELECT cut_id, snapshot, resolution_ms, cut_kind, archive_event_id
             FROM periodic
             WHERE ((rn - 1) * ?5) / n != (rn * ?5) / n
             ORDER BY taken_at_ms ASC",
        )?;
        let remaining = remaining.min(i64::MAX as usize) as i64;
        let mut rows = statement.query(params![from, to, oldest, newest, remaining])?;
        while let Some(row) = rows.next()? {
            let decoded = decode_cut(Some((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            )))?;
            if let Some(cut) = decoded {
                selected.insert(cut.snapshot.taken_at_ms.min(i64::MAX as u128) as i64, cut);
            }
        }
    }
    Ok(HistoricalCutSelection {
        cuts: selected.into_values().collect(),
        retained_cuts: count.max(0) as usize,
        oldest_at_ms: Some(oldest.max(0) as u128),
        newest_at_ms: Some(newest.max(0) as u128),
    })
}

fn decode_cut(
    row: Option<(i64, Vec<u8>, i64, String, i64)>,
) -> rusqlite::Result<Option<HistoricalCut>> {
    row.map(
        |(cut_id, blob, resolution_ms, cut_kind, archive_event_id)| {
            serde_cbor::from_slice::<DashboardSnapshot>(&blob)
                .map(|snapshot| HistoricalCut {
                    cut_id: cut_id.max(0) as u64,
                    snapshot: Arc::new(snapshot),
                    resolution_ms: resolution_ms.max(0) as u64,
                    cut_kind,
                    archive_event_id: archive_event_id.max(0) as u64,
                })
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        blob.len(),
                        rusqlite::types::Type::Blob,
                        Box::new(error),
                    )
                })
        },
    )
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

fn load_latest_flow_page_sync(
    path: &Path,
    filter: DurableFlowFilter,
    cursor: Option<String>,
    limit: usize,
    offset: usize,
) -> rusqlite::Result<DurableFlowPage> {
    use base64::Engine as _;
    let connection = reader(path)?;
    let cursor = cursor
        .map(|encoded| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| rusqlite::Error::InvalidQuery)
                .and_then(|bytes| {
                    serde_json::from_slice::<DurableCursor>(&bytes)
                        .map_err(|_| rusqlite::Error::InvalidQuery)
                })
        })
        .transpose()?;
    let as_of_event_id = if let Some(cursor) = &cursor {
        cursor.as_of_event_id
    } else {
        connection.query_row(
            "SELECT COALESCE(MAX(event_id), 0) FROM flow_events",
            [],
            |row| row.get(0),
        )?
    };
    let searches = durable_search_terms(filter.search.as_deref());
    let status = filter.status.map(flow_status_key);
    let model = filter
        .model
        .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
    let upstream = filter
        .upstream
        .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
    let client = filter.client.map(|value| value.trim().to_ascii_lowercase());
    let before_started = cursor.as_ref().map(|cursor| cursor.started_ms);
    let before_id = cursor.as_ref().map(|cursor| cursor.api_call_id.as_str());
    let where_clause = "event_id <= :as_of
         AND (:status IS NULL OR status = :status)
         AND (:model IS NULL OR lower(COALESCE(model_requested, '')) LIKE :model
              OR lower(COALESCE(model_served, '')) LIKE :model)
         AND (:upstream IS NULL OR lower(COALESCE(upstream_target, '')) LIKE :upstream
              OR EXISTS (
                 SELECT 1 FROM attempt_facts af
                 WHERE af.api_call_id = flows_latest.api_call_id
                   AND lower(COALESCE(af.provider, '')) LIKE :upstream
              ))
         AND (:client IS NULL OR lower(COALESCE(client_label, '')) = :client)
         AND (:search0 IS NULL OR instr(search_text, :search0) > 0)
         AND (:search1 IS NULL OR instr(search_text, :search1) > 0)
         AND (:search2 IS NULL OR instr(search_text, :search2) > 0)
         AND (:search3 IS NULL OR instr(search_text, :search3) > 0)
         AND (:search4 IS NULL OR instr(search_text, :search4) > 0)
         AND (:search5 IS NULL OR instr(search_text, :search5) > 0)
         AND (:search6 IS NULL OR instr(search_text, :search6) > 0)
         AND (:search7 IS NULL OR instr(search_text, :search7) > 0)
         AND (:before_started IS NULL OR started_ms < :before_started
              OR (started_ms = :before_started AND api_call_id < :before_id))";
    let total_sql = format!(
        "SELECT COUNT(*) FROM flows_latest WHERE {}",
        where_clause.replace(
            "AND (:before_started IS NULL OR started_ms < :before_started\n              OR (started_ms = :before_started AND api_call_id < :before_id))",
            "",
        )
    );
    let total: i64 = connection.query_row(
        &total_sql,
        rusqlite::named_params! {
            ":as_of": as_of_event_id,
            ":status": status,
            ":model": model,
            ":upstream": upstream,
            ":client": client,
            ":search0": searches[0],
            ":search1": searches[1],
            ":search2": searches[2],
            ":search3": searches[3],
            ":search4": searches[4],
            ":search5": searches[5],
            ":search6": searches[6],
            ":search7": searches[7],
        },
        |row| row.get(0),
    )?;
    let sql = format!(
        "SELECT summary, started_ms, api_call_id
         FROM flows_latest WHERE {where_clause}
         ORDER BY started_ms DESC, api_call_id DESC LIMIT :limit OFFSET :offset"
    );
    let fetch_limit = limit.saturating_add(1).min(501) as i64;
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::named_params! {
        ":as_of": as_of_event_id,
        ":status": status,
        ":model": model,
        ":upstream": upstream,
        ":client": client,
        ":search0": searches[0],
        ":search1": searches[1],
        ":search2": searches[2],
        ":search3": searches[3],
        ":search4": searches[4],
        ":search5": searches[5],
        ":search6": searches[6],
        ":search7": searches[7],
        ":before_started": before_started,
        ":before_id": before_id,
        ":limit": fetch_limit,
        ":offset": offset.min(i64::MAX as usize) as i64,
    })?;
    let mut decoded = Vec::<(SnapshotFlowSummary, i64, String)>::new();
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        if let Ok(summary) = serde_cbor::from_slice::<SnapshotFlowSummary>(&blob) {
            decoded.push((summary, row.get(1)?, row.get(2)?));
        }
    }
    let has_more = decoded.len() > limit;
    decoded.truncate(limit);
    let next_cursor = if has_more {
        decoded.last().and_then(|(_, started_ms, api_call_id)| {
            let cursor = DurableCursor {
                started_ms: *started_ms,
                api_call_id: api_call_id.clone(),
                as_of_event_id,
            };
            serde_json::to_vec(&cursor)
                .ok()
                .map(|bytes| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
        })
    } else {
        None
    };
    Ok(DurableFlowPage {
        summaries: decoded.into_iter().map(|(summary, _, _)| summary).collect(),
        total: total.max(0) as usize,
        next_cursor,
        as_of_event_id: as_of_event_id.max(0) as u64,
        generated_at_ms: epoch_ms_i64().max(0) as u128,
    })
}

fn load_latest_terminal_summary_sync(path: &Path) -> rusqlite::Result<Option<SnapshotFlowSummary>> {
    let connection = reader(path)?;
    let blob: Option<Vec<u8>> = connection
        .query_row(
            "SELECT summary FROM terminal_facts
             ORDER BY finished_ms DESC, api_call_id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
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

fn load_latest_terminal_matching_sync(
    path: &Path,
    filter: DurableFlowFilter,
) -> rusqlite::Result<Option<SnapshotFlowSummary>> {
    let connection = reader(path)?;
    let status = filter.status.map(flow_status_key);
    let model = filter
        .model
        .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
    let upstream = filter
        .upstream
        .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
    let client = filter.client.map(|value| value.trim().to_ascii_lowercase());
    let searches = durable_search_terms(filter.search.as_deref());
    let blob: Option<Vec<u8>> = connection
        .query_row(
            "SELECT summary FROM terminal_facts
             WHERE (:status IS NULL OR status = :status)
               AND (:model IS NULL OR lower(COALESCE(model_requested, '')) LIKE :model
                    OR lower(COALESCE(model_served, '')) LIKE :model)
               AND (:upstream IS NULL OR lower(COALESCE(upstream_target, '')) LIKE :upstream
                    OR EXISTS (
                       SELECT 1 FROM attempt_facts af
                       WHERE af.api_call_id = terminal_facts.api_call_id
                         AND lower(COALESCE(af.provider, '')) LIKE :upstream
                    ))
               AND (:client IS NULL OR lower(COALESCE(client_label, '')) = :client)
               AND (:search0 IS NULL OR instr(search_text, :search0) > 0)
               AND (:search1 IS NULL OR instr(search_text, :search1) > 0)
               AND (:search2 IS NULL OR instr(search_text, :search2) > 0)
               AND (:search3 IS NULL OR instr(search_text, :search3) > 0)
               AND (:search4 IS NULL OR instr(search_text, :search4) > 0)
               AND (:search5 IS NULL OR instr(search_text, :search5) > 0)
               AND (:search6 IS NULL OR instr(search_text, :search6) > 0)
               AND (:search7 IS NULL OR instr(search_text, :search7) > 0)
             ORDER BY finished_ms DESC, api_call_id DESC LIMIT 1",
            rusqlite::named_params! {
                ":status": status,
                ":model": model,
                ":upstream": upstream,
                ":client": client,
                ":search0": searches[0],
                ":search1": searches[1],
                ":search2": searches[2],
                ":search3": searches[3],
                ":search4": searches[4],
                ":search5": searches[5],
                ":search6": searches[6],
                ":search7": searches[7],
            },
            |row| row.get(0),
        )
        .optional()?;
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

fn load_flow_rollup_sync(
    path: &Path,
    filter: DurableFlowFilter,
) -> rusqlite::Result<DurableFlowRollup> {
    let connection = reader(path)?;
    let as_of_event_id: i64 = connection.query_row(
        "SELECT COALESCE(MAX(event_id), 0) FROM flow_events",
        [],
        |row| row.get(0),
    )?;
    let status = filter.status.map(flow_status_key);
    let model = filter
        .model
        .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
    let upstream = filter
        .upstream
        .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
    let client = filter.client.map(|value| value.trim().to_ascii_lowercase());
    let searches = durable_search_terms(filter.search.as_deref());
    let mut statement = connection.prepare(
        "SELECT summary FROM flows_latest
         WHERE event_id <= :as_of
           AND (:status IS NULL OR status = :status)
           AND (:model IS NULL OR lower(COALESCE(model_requested, '')) LIKE :model
                OR lower(COALESCE(model_served, '')) LIKE :model)
           AND (:upstream IS NULL OR lower(COALESCE(upstream_target, '')) LIKE :upstream
                OR EXISTS (
                   SELECT 1 FROM attempt_facts af
                   WHERE af.api_call_id = flows_latest.api_call_id
                     AND lower(COALESCE(af.provider, '')) LIKE :upstream
                ))
           AND (:client IS NULL OR lower(COALESCE(client_label, '')) = :client)
           AND (:search0 IS NULL OR instr(search_text, :search0) > 0)
           AND (:search1 IS NULL OR instr(search_text, :search1) > 0)
           AND (:search2 IS NULL OR instr(search_text, :search2) > 0)
           AND (:search3 IS NULL OR instr(search_text, :search3) > 0)
           AND (:search4 IS NULL OR instr(search_text, :search4) > 0)
           AND (:search5 IS NULL OR instr(search_text, :search5) > 0)
           AND (:search6 IS NULL OR instr(search_text, :search6) > 0)
           AND (:search7 IS NULL OR instr(search_text, :search7) > 0)",
    )?;
    let mut rows = statement.query(rusqlite::named_params! {
        ":as_of": as_of_event_id,
        ":status": status,
        ":model": model,
        ":upstream": upstream,
        ":client": client,
        ":search0": searches[0],
        ":search1": searches[1],
        ":search2": searches[2],
        ":search3": searches[3],
        ":search4": searches[4],
        ":search5": searches[5],
        ":search6": searches[6],
        ":search7": searches[7],
    })?;
    let mut summaries = Vec::new();
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        if let Ok(summary) = serde_cbor::from_slice::<SnapshotFlowSummary>(&blob) {
            summaries.push(summary);
        }
    }
    Ok(rollup_flow_summaries(
        summaries,
        as_of_event_id.max(0) as u64,
        epoch_ms_i64().max(0) as u128,
    ))
}

#[derive(Default)]
struct ClientRollupAccum {
    count: usize,
    failed: usize,
    source: Option<ClientSource>,
    cost_usd: f64,
    cost_confidence: Option<TerminalCostConfidence>,
    priced: usize,
    latency_ms: f64,
    timed: usize,
}

pub(crate) fn rollup_flow_summaries(
    summaries: impl IntoIterator<Item = SnapshotFlowSummary>,
    as_of_event_id: u64,
    generated_at_ms: u128,
) -> DurableFlowRollup {
    let mut models = std::collections::BTreeMap::<String, usize>::new();
    let mut upstreams = std::collections::BTreeMap::<String, usize>::new();
    let mut clients = std::collections::BTreeMap::<String, ClientRollupAccum>::new();
    let mut unattributed = 0usize;
    let mut statuses = std::collections::BTreeMap::<String, usize>::new();
    let mut failures = std::collections::BTreeMap::<(String, String, String), usize>::new();
    let mut group_totals = std::collections::BTreeMap::<(String, String), usize>::new();
    let mut context = DurableContextRollup::default();
    let mut total = 0usize;
    for summary in summaries {
        total = total.saturating_add(1);
        let mut flow_models = std::collections::BTreeSet::new();
        if let Some(value) = summary.model_requested.as_ref() {
            flow_models.insert(value.clone());
        }
        if let Some(value) = summary.model_served.as_ref() {
            flow_models.insert(value.clone());
        }
        for value in flow_models {
            *models.entry(value).or_default() += 1;
        }
        let mut flow_upstreams = std::collections::BTreeSet::new();
        if let Some(value) = summary.upstream_target.as_ref() {
            flow_upstreams.insert(value.clone());
        }
        for attempt in &summary.attempts {
            if let Some(value) = attempt.provider.as_ref() {
                flow_upstreams.insert(value.clone());
            }
        }
        for value in flow_upstreams {
            *upstreams.entry(value).or_default() += 1;
        }
        if let Some(value) = summary.client_label.as_ref() {
            let client = clients.entry(value.clone()).or_default();
            client.count = client.count.saturating_add(1);
            client.failed = client
                .failed
                .saturating_add(usize::from(summary.status == FlowStatus::Failed));
            if client_source_rank(summary.client_source) > client_source_rank(client.source) {
                client.source = summary.client_source;
            }
            if let Some(cost) = summary.terminal_cost_usd.filter(|value| value.is_finite()) {
                client.cost_usd += cost;
                client.priced = client.priced.saturating_add(1);
                client.cost_confidence = Some(match client.cost_confidence {
                    Some(current) => {
                        weakest_cost_confidence(current, summary.terminal_cost_confidence)
                    }
                    None => summary.terminal_cost_confidence,
                });
            }
            if let Some(elapsed) = summary.elapsed_ms {
                client.latency_ms += elapsed as f64;
                client.timed = client.timed.saturating_add(1);
            }
        } else {
            unattributed = unattributed.saturating_add(1);
        }
        *statuses
            .entry(flow_status_key(summary.status).to_string())
            .or_default() += 1;

        let provider = summary
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| attempt.provider.clone())
            .or_else(|| summary.upstream_target.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let model = summary
            .model_served
            .clone()
            .or(summary.model_requested.clone())
            .unwrap_or_else(|| "unknown".to_string());
        *group_totals
            .entry((provider.clone(), model.clone()))
            .or_default() += 1;
        if summary.status == FlowStatus::Failed {
            let reason = summary
                .attempts
                .iter()
                .rev()
                .find_map(|attempt| attempt.error_class.as_ref().map(serde_enum_key))
                .unwrap_or_else(|| bounded_terminal_reason(summary.terminal_reason.as_deref()));
            *failures.entry((provider, model, reason)).or_default() += 1;
        }
        if let (Some(usage), Some(limit)) = (summary.usage, summary.effective_route_limit)
            && limit > 0
        {
            let fraction = usage.prompt.max(0) as f64 / limit as f64;
            context.measurable = context.measurable.saturating_add(1);
            context.near_limit += usize::from(fraction >= 0.85);
            context.over_limit += usize::from(fraction >= 1.0);
            let pct = fraction * 100.0;
            context.peak_pct = Some(context.peak_pct.map_or(pct, |current| current.max(pct)));
        }
    }
    let mut clients = clients
        .into_iter()
        .map(|(key, client)| DurableClientRollup {
            key,
            count: client.count,
            failed: client.failed,
            source: client.source,
            cost_usd: (client.priced > 0).then_some(client.cost_usd),
            cost_confidence: client.cost_confidence.unwrap_or_default(),
            priced: client.priced,
            average_latency_ms: (client.timed > 0)
                .then_some(client.latency_ms / client.timed as f64),
            timed: client.timed,
        })
        .collect::<Vec<_>>();
    clients.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.key.cmp(&right.key))
    });
    clients.truncate(50);
    DurableFlowRollup {
        total,
        models: sorted_facets(models, usize::MAX),
        upstreams: sorted_facets(upstreams, usize::MAX),
        clients,
        unattributed,
        statuses: sorted_facets(statuses, usize::MAX),
        failures: failures
            .into_iter()
            .map(|((provider, model, reason), count)| DurableFailureCount {
                total: group_totals
                    .get(&(provider.clone(), model.clone()))
                    .copied()
                    .unwrap_or(count),
                provider,
                model,
                reason,
                count,
            })
            .collect(),
        context,
        as_of_event_id,
        generated_at_ms,
    }
}

fn client_source_rank(source: Option<ClientSource>) -> u8 {
    match source {
        Some(ClientSource::KeyHash | ClientSource::ConfiguredHeader) => 2,
        Some(ClientSource::UserAgent) => 1,
        None => 0,
    }
}

fn weakest_cost_confidence(
    left: TerminalCostConfidence,
    right: TerminalCostConfidence,
) -> TerminalCostConfidence {
    use TerminalCostConfidence::{Confident, Estimated, Unavailable};
    match (left, right) {
        (Unavailable, _) | (_, Unavailable) => Unavailable,
        (Estimated, _) | (_, Estimated) => Estimated,
        (Confident, Confident) => Confident,
    }
}

fn sorted_facets(
    values: std::collections::BTreeMap<String, usize>,
    limit: usize,
) -> Vec<DurableFacetCount> {
    let mut values = values
        .into_iter()
        .map(|(key, count)| DurableFacetCount { key, count })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.key.cmp(&right.key))
    });
    values.truncate(limit);
    values
}

fn bounded_terminal_reason(reason: Option<&str>) -> String {
    match reason
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("stop" | "response.completed") => "stop",
        Some("length" | "response.incomplete") => "length",
        Some("tool_calls") => "tool_calls",
        Some("content_filter") => "content_filter",
        Some("other") => "other",
        _ => "unclassified",
    }
    .to_string()
}

const DURABLE_SEARCH_MAX_CHARS: usize = 256;
const DURABLE_SEARCH_MAX_TERMS: usize = 8;

fn durable_search_terms(search: Option<&str>) -> [Option<String>; DURABLE_SEARCH_MAX_TERMS] {
    let terms = search
        .unwrap_or_default()
        .chars()
        .take(DURABLE_SEARCH_MAX_CHARS)
        .collect::<String>()
        .to_ascii_lowercase()
        .split_whitespace()
        .take(DURABLE_SEARCH_MAX_TERMS)
        .map(str::to_string)
        .collect::<Vec<_>>();
    std::array::from_fn(|index| terms.get(index).cloned())
}

fn summary_search_text(summary: &SnapshotFlowSummary) -> String {
    let status_aliases = match summary.status {
        FlowStatus::Open => "open running streaming live",
        FlowStatus::Completed => "completed complete success successful 2xx",
        FlowStatus::Failed => "failed failure error 5xx",
        FlowStatus::Cancelled => "cancelled canceled cancel 499",
    };
    let mut values = vec![
        summary.api_call_id.as_str(),
        summary.response_id.as_deref().unwrap_or_default(),
        summary.method.as_str(),
        summary.uri.as_str(),
        flow_status_key(summary.status),
        status_aliases,
        summary.model_requested.as_deref().unwrap_or_default(),
        summary.model_served.as_deref().unwrap_or_default(),
        summary.upstream_target.as_deref().unwrap_or_default(),
        summary.client_label.as_deref().unwrap_or_default(),
        summary.terminal_reason.as_deref().unwrap_or_default(),
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    if let Some(source) = summary.client_source.as_ref() {
        values.push(serde_enum_key(source));
    }
    if summary.model_requested.is_some()
        && summary.model_served.is_some()
        && summary.model_requested != summary.model_served
    {
        values.push("failover fallback".to_string());
    }
    for attempt in &summary.attempts {
        values.extend([
            attempt.provider.clone().unwrap_or_default(),
            attempt.model.clone().unwrap_or_default(),
            serde_enum_key(&attempt.status),
            attempt
                .error_class
                .as_ref()
                .map(serde_enum_key)
                .unwrap_or_default(),
            attempt
                .failover_reason
                .as_ref()
                .map(serde_enum_key)
                .unwrap_or_default(),
        ]);
    }
    values.join(" ").to_ascii_lowercase()
}

fn bootstrap_archive_state(
    connection: &Connection,
) -> rusqlite::Result<(DomainCursors, Vec<SnapshotFlowSummary>)> {
    let latest_blob: Option<Vec<u8>> = connection
        .query_row(
            "SELECT snapshot FROM dashboard_cuts ORDER BY taken_at_ms DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let mut cursors = latest_blob
        .as_deref()
        .and_then(|blob| serde_cbor::from_slice::<DashboardSnapshot>(blob).ok())
        .map_or_else(DomainCursors::default, |snapshot| snapshot.cursors);
    let flow_seq: i64 = connection.query_row(
        "SELECT MAX(
            COALESCE((SELECT MAX(record_seq) FROM flows_latest), 0),
            COALESCE((SELECT integer_value FROM archive_meta WHERE key = 'last_flow_seq'), 0)
         )",
        [],
        |row| row.get(0),
    )?;
    let monitor_seq: i64 = connection.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM monitor_updates",
        [],
        |row| row.get(0),
    )?;
    cursors.flow_seq = cursors.flow_seq.max(flow_seq.max(0) as u64);
    cursors.monitor_seq = cursors.monitor_seq.max(monitor_seq.max(0) as u64);

    let cutoff = epoch_ms_i64().saturating_sub(60 * 60 * 1_000);
    let mut statement = connection.prepare(
        "SELECT summary FROM terminal_facts WHERE finished_ms >= ?1 ORDER BY finished_ms ASC",
    )?;
    let mut rows = statement.query([cutoff])?;
    let mut terminals = Vec::new();
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        if let Ok(summary) = serde_cbor::from_slice(&blob) {
            terminals.push(summary);
        }
    }
    Ok((cursors, terminals))
}

fn latest_activity_from_connection(
    connection: &Connection,
) -> rusqlite::Result<Option<crate::metrics::LastActivitySample>> {
    let mut recovered = None;
    let mut statement = connection
        .prepare("SELECT snapshot FROM dashboard_cuts ORDER BY taken_at_ms DESC LIMIT 256")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let blob: Vec<u8> = row.get(0)?;
        if let Ok(snapshot) = serde_cbor::from_slice::<DashboardSnapshot>(&blob)
            && snapshot.last_activity.is_some()
        {
            recovered = snapshot.last_activity;
            break;
        }
    }
    let terminal_blob: Option<Vec<u8>> = connection
        .query_row(
            "SELECT summary FROM terminal_facts ORDER BY finished_ms DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let terminal = terminal_blob
        .as_deref()
        .and_then(|blob| serde_cbor::from_slice::<SnapshotFlowSummary>(blob).ok())
        .and_then(last_activity_from_terminal);
    Ok(match (recovered, terminal) {
        (Some(cut), Some(terminal)) if terminal.at_ms > cut.at_ms => Some(terminal),
        (Some(cut), _) => Some(cut),
        (None, terminal) => terminal,
    })
}

fn last_activity_from_terminal(
    summary: SnapshotFlowSummary,
) -> Option<crate::metrics::LastActivitySample> {
    use crate::metrics::{InstantMetricQuality, InstantMetricSample};
    let at_ms = summary.finished_ms?;
    let elapsed = summary.elapsed_ms.map(|value| value as f64);
    let usage = summary.usage;
    let mut instant = InstantMetricSample {
        interval_duration_ms: Some(1_000),
        ready: true,
        accepted_requests: 0,
        accepted_per_sec: Some(0.0),
        terminal_requests: 1,
        terminal_per_sec: Some(1.0),
        successes: u64::from(summary.status == FlowStatus::Completed),
        failures: u64::from(summary.status == FlowStatus::Failed),
        failure_pct: Some(if summary.status == FlowStatus::Failed {
            100.0
        } else {
            0.0
        }),
        cancellations: u64::from(summary.status == FlowStatus::Cancelled),
        cancellation_pct: Some(if summary.status == FlowStatus::Cancelled {
            100.0
        } else {
            0.0
        }),
        active_streams_now: 0,
        latency_samples: u64::from(elapsed.is_some()),
        p50_ms: elapsed,
        p95_ms: elapsed,
        p99_ms: elapsed,
        p50_quality: if elapsed.is_some() {
            InstantMetricQuality::Measured
        } else {
            InstantMetricQuality::Unavailable
        },
        p95_quality: if elapsed.is_some() {
            InstantMetricQuality::Measured
        } else {
            InstantMetricQuality::Unavailable
        },
        p99_quality: if elapsed.is_some() {
            InstantMetricQuality::Measured
        } else {
            InstantMetricQuality::Unavailable
        },
        usage_samples: u64::from(usage.is_some()),
        reported_tokens_per_sec: usage.map(|usage| usage.total as f64),
        priced_samples: u64::from(summary.terminal_cost_usd.is_some()),
        cost_per_min: summary.terminal_cost_usd.map(|cost| cost * 60.0),
        cost_confidence: summary.terminal_cost_confidence,
        ..InstantMetricSample::bootstrap(0)
    };
    // Keep the binding mutable-friendly for future recovered fields while preserving
    // the metrics layer's histogram error bound and quantile-method defaults.
    instant.active_streams_now = 0;
    Some(crate::metrics::LastActivitySample { at_ms, instant })
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

fn metadata_sync(
    path: &Path,
    failed_writes: u64,
    mode: DurabilityMode,
) -> rusqlite::Result<DurableHistoryMetadata> {
    let connection = reader(path)?;
    let (oldest, newest, count): (Option<i64>, Option<i64>, i64) = connection.query_row(
        "SELECT MIN(taken_at_ms), MAX(taken_at_ms), COUNT(*) FROM dashboard_cuts",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let archived_flows: i64 =
        connection.query_row("SELECT COUNT(*) FROM flows_latest", [], |row| row.get(0))?;
    let terminal_flows: i64 =
        connection.query_row("SELECT COUNT(*) FROM terminal_facts", [], |row| row.get(0))?;
    // The v1 compatibility table can reference artifacts already removed by the old
    // age rotator. Manifests are the v2 source of truth and index only files that were
    // actually recovered/published, so status must never claim bytes that no longer exist.
    let artifact_bytes: i64 = connection.query_row(
        "SELECT COALESCE(SUM(bytes), 0) FROM artifact_manifests",
        [],
        |row| row.get(0),
    )?;
    let last_commit_ms: Option<i64> = connection
        .query_row(
            "SELECT integer_value FROM archive_meta WHERE key = 'last_commit_ms'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let pending_commits: i64 =
        connection.query_row("SELECT COUNT(*) FROM pending_turn_commits", [], |row| {
            row.get(0)
        })?;
    let (activity_cuts, fine_cuts, minute_cuts, coarse_cuts): (i64, i64, i64, i64) =
        connection.query_row(
            "SELECT
                 COALESCE(SUM(CASE WHEN cut_kind = 'activity' THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN cut_kind != 'activity' AND resolution_ms <= 5000 THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN cut_kind != 'activity' AND resolution_ms = 60000 THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN cut_kind != 'activity' AND resolution_ms >= 900000 THEN 1 ELSE 0 END), 0)
             FROM dashboard_cuts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let database_bytes = std::fs::metadata(path)
        .map(|metadata| metadata.len().min(usize::MAX as u64) as usize)
        .unwrap_or(0);
    Ok(DurableHistoryMetadata {
        oldest_at_ms: oldest.map(|value| value.max(0) as u128),
        newest_at_ms: newest.map(|value| value.max(0) as u128),
        retained_cuts: count.max(0) as usize,
        database_bytes,
        dropped_writes: failed_writes,
        archived_flows: archived_flows.max(0) as usize,
        terminal_flows: terminal_flows.max(0) as usize,
        artifact_bytes: artifact_bytes.max(0) as usize,
        last_commit_ms: last_commit_ms.map(|value| value.max(0) as u128),
        pending_commits: pending_commits.max(0) as usize,
        mode,
        activity_cuts: activity_cuts.max(0) as usize,
        fine_cuts: fine_cuts.max(0) as usize,
        minute_cuts: minute_cuts.max(0) as usize,
        coarse_cuts: coarse_cuts.max(0) as usize,
    })
}

fn recover_orphan_capture_files(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // A crash after fsync but before rename leaves the complete artifact at
    // `<id>.json.tmp`. Validate it with a streaming deserializer and publish it.
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(api_call_id) = name.strip_suffix(".json.tmp") else {
            continue;
        };
        let mut file = std::fs::File::open(&path)?;
        let mut deserializer = serde_json::Deserializer::from_reader(&mut file);
        if serde::de::IgnoredAny::deserialize(&mut deserializer).is_ok() {
            std::fs::rename(&path, dir.join(format!("{api_call_id}.json")))?;
        } else {
            // A torn temp file is not a publishable artifact. Remove it only after
            // validating; a matching pending commit below will synthesize an explicit
            // partial manifest instead of letting corrupt crash residue linger forever.
            std::fs::remove_file(&path)?;
        }
    }

    let work_root = dir.join(".work");
    let Ok(work_entries) = std::fs::read_dir(&work_root) else {
        return Ok(());
    };
    for entry in work_entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let api_call_id = entry.file_name().to_string_lossy().to_string();
        if !api_call_id.starts_with("api_") {
            continue;
        }
        let final_path = dir.join(format!("{api_call_id}.json"));
        if final_path.is_file() {
            let _ = std::fs::remove_dir_all(entry.path());
            continue;
        }
        recover_work_dir(&entry.path(), &final_path, &api_call_id)?;
        std::fs::remove_dir_all(entry.path())?;
    }
    if let Ok(directory) = std::fs::File::open(dir) {
        directory.sync_all()?;
    }
    Ok(())
}

fn recover_work_dir(work_dir: &Path, final_path: &Path, api_call_id: &str) -> std::io::Result<()> {
    let mut section_paths = Vec::<(String, PathBuf)>::new();
    for name in [
        "inbound_request",
        "normalized_request",
        "upstream_request",
        "served_response",
    ] {
        let path = work_dir.join(name);
        if path.is_file() {
            section_paths.push((name.to_string(), path));
        }
    }
    let mut upstream = std::fs::read_dir(work_dir)?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (name == "upstream_response" || name.starts_with("upstream_response."))
                .then_some(entry.path())
        })
        .collect::<Vec<_>>();
    upstream.sort();
    if let Some(path) = upstream.pop() {
        section_paths.push(("upstream_response".to_string(), path));
    }

    let tmp = final_path.with_extension("json.tmp.recover");
    let file = std::fs::File::create(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(b"{\"api_call_id\":")?;
    serde_json::to_writer(&mut writer, api_call_id)?;
    writer.write_all(
        b",\"status\":\"cancelled\",\"terminal_reason\":\"gateway_restart\",\"sections\":{",
    )?;
    for (index, (name, path)) in section_paths.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut writer, name)?;
        let bytes = std::fs::metadata(path)?.len();
        write!(
            writer,
            ":{{\"bytes\":{bytes},\"partial\":true,\"encoding\":\"base64\",\"content\":\""
        )?;
        let mut input = std::fs::File::open(path)?;
        {
            let mut encoder = base64::write::EncoderWriter::new(
                &mut writer,
                &base64::engine::general_purpose::STANDARD,
            );
            std::io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
        }
        writer.write_all(b"\"}")?;
    }
    writer.write_all(b"}}")?;
    writer.flush()?;
    writer.into_inner()?.sync_all()?;
    std::fs::rename(&tmp, final_path)?;
    Ok(())
}

fn recover_pending_artifacts(connection: &Connection, dir: &Path) -> std::io::Result<()> {
    let mut statement = connection
        .prepare("SELECT api_call_id, payload FROM pending_turn_commits")
        .map_err(std::io::Error::other)?;
    let pending = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(std::io::Error::other)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?;
    for (api_call_id, payload) in pending {
        let path = dir.join(format!("{api_call_id}.json"));
        if path.is_file() {
            continue;
        }
        let tmp = dir.join(format!("{api_call_id}.json.tmp.recover"));
        let file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut writer = std::io::BufWriter::new(file);
        let summary = serde_cbor::from_slice::<SnapshotFlowSummary>(&payload).ok();
        let status = summary
            .as_ref()
            .map_or("cancelled", |summary| flow_status_key(summary.status));
        let reason = summary
            .as_ref()
            .and_then(|summary| summary.terminal_reason.as_deref())
            .unwrap_or("gateway_restart");
        serde_json::to_writer(
            &mut writer,
            &serde_json::json!({
                "api_call_id": api_call_id,
                "status": status,
                "terminal_reason": reason,
                "started_ms": summary.as_ref().map(|summary| summary.started_ms),
                "finished_ms": summary.as_ref().and_then(|summary| summary.finished_ms),
                "model_requested": summary.as_ref().and_then(|summary| summary.model_requested.as_deref()),
                "model_served": summary.as_ref().and_then(|summary| summary.model_served.as_deref()),
                "partial": true,
                "sections": {},
            }),
        )?;
        writer.flush()?;
        writer.into_inner()?.sync_all()?;
        std::fs::rename(tmp, path)?;
    }
    if let Ok(directory) = std::fs::File::open(dir) {
        directory.sync_all()?;
    }
    Ok(())
}

fn index_existing_artifacts(connection: &mut Connection, dir: &Path) -> rusqlite::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
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
        index_artifact_sync(
            connection,
            api_call_id,
            &path,
            metadata.len().min(i64::MAX as u64) as i64,
            modified_ms,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_flow::{FlowStatus, PhaseTimings, TerminalCostConfidence};
    use crate::metrics::{DomainCursors, MetricsView};
    use crate::upstream::ProviderHealthSnapshot;

    fn summary(id: &str, started_ms: u128, status: FlowStatus) -> SnapshotFlowSummary {
        SnapshotFlowSummary {
            revision: if status == FlowStatus::Open { 1 } else { 2 },
            api_call_id: id.to_string(),
            response_id: Some(id.replacen("api_", "resp_", 1)),
            method: "POST".to_string(),
            uri: "/v1/responses".to_string(),
            model_requested: Some("model-a".to_string()),
            model_served: Some("model-a".to_string()),
            upstream_target: Some("provider-a".to_string()),
            usage: (status != FlowStatus::Open).then_some(crate::dashboard_flow::FlowUsage {
                prompt: 10,
                completion: 5,
                total: 15,
                cached: Some(0),
                reasoning: Some(0),
            }),
            terminal_cost_usd: (status != FlowStatus::Open).then_some(0.01),
            terminal_cost_confidence: if status == FlowStatus::Open {
                TerminalCostConfidence::Unavailable
            } else {
                TerminalCostConfidence::Confident
            },
            cache_price_impact_usd: None,
            effective_route_limit: Some(8_192),
            status,
            started_ms,
            finished_ms: (status != FlowStatus::Open).then_some(started_ms + 100),
            elapsed_ms: (status != FlowStatus::Open).then_some(100),
            terminal_reason: (status != FlowStatus::Open).then(|| "response.completed".to_string()),
            phases: PhaseTimings {
                ingress_ms: Some(started_ms),
                ingress_offset_ms: Some(0),
                ..PhaseTimings::default()
            },
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
            client_label: Some("client-a".to_string()),
            client_source: Some(crate::dashboard_flow::ClientSource::KeyHash),
        }
    }

    #[tokio::test]
    async fn disabled_history_has_no_state() {
        let history = DashboardHistory::disabled();
        assert!(!history.is_enabled());
        history
            .persist_monitor_update(DebugUpdate {
                sequence: 1,
                messages: Vec::new(),
            })
            .await
            .expect("disabled history is a successful no-op");
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
            phases: PhaseTimings {
                ingress_ms: Some(1_000),
                ingress_offset_ms: Some(0),
                normalization_done_ms: Some(1_025),
                normalization_done_offset_ms: Some(25),
                routing_decision_ms: Some(1_050),
                routing_decision_offset_ms: Some(50),
                first_content_delta_ms: Some(1_500),
                first_content_delta_offset_ms: Some(500),
                stream_end_ms: Some(1_950),
                stream_end_offset_ms: Some(950),
                finalize_ms: Some(2_000),
                finalize_offset_ms: Some(1_000),
            },
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
                backend_metrics_seq: 8,
            },
            summaries: vec![summary],
            flow_summaries_truncated: false,
            metrics: MetricsView::default(),
            instant: crate::metrics::InstantMetricSample::default(),
            last_activity: None,
            engine_throughput: None,
            topology: Arc::new(ProviderHealthSnapshot::default()),
            backend_metrics: Arc::new(crate::backend_metrics::BackendMetricsSnapshot::default()),
        });
        serde_cbor::to_vec(cut.as_ref()).expect("snapshot serializes");
        history.persist_cut(cut).await.expect("persist cut");
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
        assert_eq!(loaded.snapshot.summaries[0].phases.ingress_ms, Some(1_000));
        assert_eq!(
            loaded.snapshot.summaries[0].phases.finalize_offset_ms,
            Some(1_000)
        );
        let flow = history
            .flow_summary_at("api_persisted", 5_000)
            .await
            .expect("flow version indexed");
        assert_eq!(flow.revision, 4);
        let historical_rows = history.flow_summaries_as_of(5_000).await;
        assert_eq!(historical_rows.len(), 1);
        assert_eq!(historical_rows[0].api_call_id, "api_persisted");
        assert_eq!(
            historical_rows[0].phases.first_content_delta_ms,
            Some(1_500)
        );

        // An unchanged long-running flow retains the same revision across cuts. Its first-observed
        // row must survive retention cleanup while it remains the newest version, otherwise it
        // disappears from a newer retained cut.
        let compact =
            DashboardHistory::enabled_for_test_with_retention(root.join("compact.sqlite3"), 100);
        for taken_at_ms in [1_000u128, 2_000] {
            compact
                .persist_cut(Arc::new(DashboardSnapshot {
                    taken_at_ms,
                    cursors: loaded.snapshot.cursors,
                    summaries: vec![flow.clone()],
                    flow_summaries_truncated: false,
                    metrics: MetricsView::default(),
                    instant: crate::metrics::InstantMetricSample::default(),
                    last_activity: None,
                    engine_throughput: None,
                    topology: Arc::new(ProviderHealthSnapshot::default()),
                    backend_metrics: Arc::new(
                        crate::backend_metrics::BackendMetricsSnapshot::default(),
                    ),
                }))
                .await
                .expect("persist compact cut");
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

    #[tokio::test]
    async fn keyset_pages_freeze_the_archive_while_new_flows_arrive() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-keyset-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let history = DashboardHistory::enabled_for_test(root.join("history.sqlite3"), None);
        for index in 0..16u64 {
            let mut flow = summary(
                &format!("api_{index:02}"),
                10_000 + u128::from(index),
                FlowStatus::Completed,
            );
            if index == 15 {
                flow.status = FlowStatus::Failed;
                flow.terminal_reason = Some("upstream failure".to_string());
                flow.attempts.push(crate::dashboard_flow::Attempt {
                    provider: Some("provider-z".to_string()),
                    model: Some("model-a".to_string()),
                    start_ms: 10_015,
                    end_ms: 10_115,
                    duration_ms: Some(100),
                    first_upstream_byte_ms: None,
                    first_upstream_byte_offset_ms: None,
                    status: crate::dashboard_flow::AttemptStatus::Failed,
                    error_class: Some(crate::dashboard_flow::AttemptErrorClass::Timeout),
                    failover_reason: Some(
                        crate::dashboard_flow::AttemptFailoverReason::ProviderFailed,
                    ),
                });
            }
            history
                .persist_flow_summary(flow, FlowMutationPhase::Terminal, index + 1)
                .await
                .expect("terminal persists");
        }
        let first = history
            .latest_flow_page(DurableFlowFilter::default(), None, 5, 0)
            .await
            .expect("first page");
        assert_eq!(first.total, 16);
        assert_eq!(first.summaries.len(), 5);
        let cursor = first.next_cursor.clone().expect("next cursor");

        history
            .persist_flow_summary(
                summary("api_new", 99_999, FlowStatus::Completed),
                FlowMutationPhase::Terminal,
                17,
            )
            .await
            .expect("new arrival persists");
        let second = history
            .latest_flow_page(DurableFlowFilter::default(), Some(cursor), 5, 0)
            .await
            .expect("second page");
        assert_eq!(second.total, 16, "cursor retains its original watermark");
        assert!(
            second
                .summaries
                .iter()
                .all(|flow| flow.api_call_id != "api_new")
        );
        let rollup = history
            .flow_rollup(DurableFlowFilter::default())
            .await
            .expect("rollup");
        assert_eq!(rollup.total, 17);
        assert_eq!(rollup.clients[0].count, 17);
        assert_eq!(rollup.context.measurable, 17);

        let search = DurableFlowFilter {
            search: Some("provider-z timeout 5xx".to_string()),
            ..DurableFlowFilter::default()
        };
        let search_page = history
            .latest_flow_page(search.clone(), None, 100, 0)
            .await
            .expect("multi-term attempt search");
        assert_eq!(search_page.total, 1);
        assert_eq!(search_page.summaries[0].api_call_id, "api_15");
        assert_eq!(
            history
                .latest_terminal_matching(search.clone())
                .await
                .expect("matching terminal")
                .api_call_id,
            "api_15"
        );
        assert_eq!(
            history
                .flow_rollup(search)
                .await
                .expect("matching rollup")
                .total,
            1
        );
        drop(history);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restart_cancels_open_flow_at_its_last_recorded_activity() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-recover-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let db = root.join("history.sqlite3");
        let history = DashboardHistory::enabled_for_test(db.clone(), None);
        let mut open = summary("api_open", 1_000, FlowStatus::Open);
        open.revision = 3;
        open.phases.routing_decision_ms = Some(1_750);
        open.phases.routing_decision_offset_ms = Some(750);
        history
            .persist_flow_summary(open, FlowMutationPhase::Progress, 9)
            .await
            .expect("open flow persists");
        drop(history);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let recovered = DashboardHistory::enabled_for_test(db, None);
        let flow = recovered
            .latest_flow_summary("api_open")
            .await
            .expect("recovered flow");
        assert_eq!(flow.status, FlowStatus::Cancelled);
        assert_eq!(flow.finished_ms, Some(1_750));
        assert_eq!(flow.terminal_reason.as_deref(), Some("gateway_restart"));
        assert!(recovered.bootstrap_cursors().flow_seq >= 9);
        drop(recovered);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn artifact_commit_records_checksum_and_clears_pending_turn() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-artifact-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let db = root.join("history.sqlite3");
        let history = DashboardHistory::enabled_for_test(db.clone(), Some(root.clone()));
        history
            .persist_flow_summary(
                summary("api_artifact", 2_000, FlowStatus::Completed),
                FlowMutationPhase::Terminal,
                1,
            )
            .await
            .expect("terminal persists");
        assert_eq!(history.metadata().await.pending_commits, 1);
        let artifact = root.join("api_artifact.json");
        std::fs::write(
            &artifact,
            br#"{"api_call_id":"api_artifact","sections":{"served_response":{"bytes":2,"partial":false,"encoding":"json","content":{}}}}"#,
        )
        .expect("artifact write");
        history
            .index_artifact("api_artifact".to_string(), artifact)
            .await
            .expect("artifact index");
        let metadata = history.metadata().await;
        assert_eq!(metadata.pending_commits, 0);
        assert!(metadata.artifact_bytes > 0);
        let connection = reader(&db).expect("reader");
        let checksum: String = connection
            .query_row(
                "SELECT sha256 FROM artifact_manifests WHERE api_call_id = 'api_artifact'",
                [],
                |row| row.get(0),
            )
            .expect("manifest checksum");
        assert_eq!(checksum.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("api_artifact.json"))
                .expect("artifact metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "indexed artifacts are owner-only");
        }
        drop(history);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn v1_migration_backfills_latest_terminal_and_artifact_index() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-migrate-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let db = root.join("history.sqlite3");
        let legacy = summary("api_legacy", 10_000, FlowStatus::Completed);
        let legacy_blob = serde_cbor::to_vec(&legacy).expect("legacy flow serializes");
        let legacy_instant = crate::metrics::InstantMetricSample {
            accepted_requests: 1,
            ..crate::metrics::InstantMetricSample::default()
        };
        let legacy_cut = DashboardSnapshot {
            taken_at_ms: 10_200,
            cursors: DomainCursors {
                flow_seq: 2,
                ..DomainCursors::default()
            },
            summaries: vec![legacy.clone()],
            flow_summaries_truncated: false,
            metrics: MetricsView::default(),
            instant: legacy_instant,
            last_activity: None,
            engine_throughput: None,
            topology: Arc::new(ProviderHealthSnapshot::default()),
            backend_metrics: Arc::new(crate::backend_metrics::BackendMetricsSnapshot::default()),
        };
        let legacy_cut_blob = serde_cbor::to_vec(&legacy_cut).expect("legacy cut serializes");
        {
            let connection = Connection::open(&db).expect("legacy database");
            connection
                .execute_batch(
                    "CREATE TABLE dashboard_cuts (
                        cut_id INTEGER PRIMARY KEY,
                        taken_at_ms INTEGER NOT NULL UNIQUE,
                        monitor_seq INTEGER NOT NULL,
                        snapshot BLOB NOT NULL
                     );
                     CREATE TABLE flow_versions (
                        api_call_id TEXT NOT NULL,
                        revision INTEGER NOT NULL,
                        observed_at_ms INTEGER NOT NULL,
                        summary BLOB NOT NULL,
                        PRIMARY KEY(api_call_id, revision)
                     );
                     CREATE TABLE monitor_updates (
                        sequence INTEGER PRIMARY KEY,
                        update_blob BLOB NOT NULL
                     );
                     CREATE TABLE artifacts (
                        api_call_id TEXT PRIMARY KEY,
                        path TEXT NOT NULL,
                        bytes INTEGER NOT NULL,
                        modified_ms INTEGER NOT NULL
                     );
                     PRAGMA user_version = 1;",
                )
                .expect("legacy schema");
            connection
                .execute(
                    "INSERT INTO flow_versions(api_call_id, revision, observed_at_ms, summary)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![legacy.api_call_id, legacy.revision, 10_100i64, legacy_blob],
                )
                .expect("legacy flow");
            connection
                .execute(
                    "INSERT INTO dashboard_cuts(cut_id, taken_at_ms, monitor_seq, snapshot)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![10_200i64, 10_200i64, 0i64, legacy_cut_blob],
                )
                .expect("legacy cut");
            // v1 could retain an index row after age rotation removed the file. V2
            // must expose that pre-migration payload gap honestly instead of counting
            // nonexistent bytes in durability status.
            connection
                .execute(
                    "INSERT INTO artifacts(api_call_id, path, bytes, modified_ms)
                     VALUES ('api_expired', '/missing/api_expired.json', 999999, 1)",
                    [],
                )
                .expect("expired legacy manifest");
        }
        let artifact = root.join("api_legacy.json");
        std::fs::write(
            &artifact,
            br#"{"api_call_id":"api_legacy","sections":{"served_response":{"bytes":2,"partial":false,"encoding":"utf8","content":"ok"}}}"#,
        )
        .expect("legacy artifact");

        let history = DashboardHistory::enabled_for_test(db.clone(), Some(root.clone()));
        let migrated = history
            .latest_flow_summary("api_legacy")
            .await
            .expect("latest flow backfilled");
        assert_eq!(migrated.status, FlowStatus::Completed);
        let metadata = history.metadata().await;
        assert_eq!(metadata.archived_flows, 1);
        assert_eq!(metadata.terminal_flows, 1);
        assert_eq!(
            metadata.artifact_bytes,
            std::fs::metadata(&artifact)
                .expect("artifact metadata")
                .len() as usize
        );
        let connection = Connection::open(&db).expect("migrated database");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("schema version");
        assert_eq!(version, SQLITE_SCHEMA_VERSION);
        let manifests: i64 = connection
            .query_row("SELECT COUNT(*) FROM artifact_manifests", [], |row| {
                row.get(0)
            })
            .expect("artifact manifests");
        assert_eq!(manifests, 1);
        let (cut_kind, resolution_ms, archive_event_id): (String, i64, i64) = connection
            .query_row(
                "SELECT cut_kind, resolution_ms, archive_event_id FROM dashboard_cuts
                 WHERE cut_id = 10200",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("legacy cut projections");
        assert_eq!(cut_kind, "activity");
        assert_eq!(resolution_ms, 0);
        assert_eq!(archive_event_id, 1);
        drop(history);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restart_finishes_artifact_publication_and_discards_torn_temp() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-pending-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let db = root.join("history.sqlite3");
        let history = DashboardHistory::enabled_for_test(db.clone(), Some(root.clone()));
        history
            .persist_flow_summary(
                summary("api_pending", 20_000, FlowStatus::Completed),
                FlowMutationPhase::Terminal,
                3,
            )
            .await
            .expect("pending terminal");
        assert_eq!(history.metadata().await.pending_commits, 1);
        drop(history);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        std::fs::write(root.join("api_pending.json.tmp"), b"{torn").expect("torn crash residue");

        let recovered = DashboardHistory::enabled_for_test(db, Some(root.clone()));
        let metadata = recovered.metadata().await;
        assert_eq!(metadata.pending_commits, 0);
        assert_eq!(metadata.terminal_flows, 1);
        assert!(!root.join("api_pending.json.tmp").exists());
        let artifact =
            std::fs::read(root.join("api_pending.json")).expect("partial artifact synthesized");
        let artifact: serde_json::Value =
            serde_json::from_slice(&artifact).expect("valid recovered artifact");
        assert_eq!(artifact["status"], "completed");
        assert_eq!(artifact["partial"], true);
        assert_eq!(artifact["terminal_reason"], "response.completed");
        drop(recovered);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn required_mode_rejects_missing_or_non_directory_artifact_storage() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-required-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let missing = root.join("missing-artifacts");
        assert!(
            DashboardHistory::open(
                root.join("missing.sqlite3"),
                Some(missing),
                FINE_CUT_RETENTION_MS,
                DurabilityMode::Required,
            )
            .is_err(),
            "required mode refuses missing artifact storage"
        );
        let file = root.join("not-a-directory");
        std::fs::write(&file, b"x").expect("sentinel file");
        assert!(
            DashboardHistory::open(
                root.join("file.sqlite3"),
                Some(file),
                FINE_CUT_RETENTION_MS,
                DurabilityMode::Required,
            )
            .is_err(),
            "required mode refuses a non-directory artifact path"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tier_compaction_keeps_activity_and_never_deletes_authoritative_facts() {
        let root =
            std::env::temp_dir().join(format!("llmconduit-history-tiers-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("temp root");
        let db = root.join("history.sqlite3");
        let history = DashboardHistory::open(
            db.clone(),
            Some(root.clone()),
            FINE_CUT_RETENTION_MS,
            DurabilityMode::BestEffort,
        )
        .expect("history opens");
        let mut terminal = summary("api_forever", 1_000, FlowStatus::Completed);
        terminal.attempts.push(crate::dashboard_flow::Attempt {
            provider: Some("provider-a".to_string()),
            model: Some("model-a".to_string()),
            start_ms: 1_010,
            end_ms: 1_090,
            duration_ms: Some(80),
            first_upstream_byte_ms: Some(1_020),
            first_upstream_byte_offset_ms: Some(10),
            status: crate::dashboard_flow::AttemptStatus::Served,
            error_class: None,
            failover_reason: None,
        });
        history
            .persist_flow_summary(terminal.clone(), FlowMutationPhase::Terminal, 1)
            .await
            .expect("terminal persists");
        let artifact = root.join("api_forever.json");
        std::fs::write(&artifact, br#"{"api_call_id":"api_forever","sections":{}}"#)
            .expect("artifact");
        history
            .index_artifact("api_forever".to_string(), artifact)
            .await
            .expect("artifact index");

        const DAY: u128 = 24 * 60 * 60 * 1_000;
        let now = 100 * DAY;
        let activity_at = now - 60 * DAY;
        let coarse = [now - 40 * DAY, now - 40 * DAY + 1_000];
        let minute = [now - 2 * DAY, now - 2 * DAY + 1_000];
        let recent = [now - 60 * 60 * 1_000, now - 60 * 60 * 1_000 + 5_000];
        for at in std::iter::once(activity_at)
            .chain(coarse)
            .chain(minute)
            .chain(recent)
            .chain(std::iter::once(now))
        {
            let mut instant = crate::metrics::InstantMetricSample::default();
            if at == activity_at {
                instant.accepted_requests = 1;
            }
            history
                .persist_cut(Arc::new(DashboardSnapshot {
                    taken_at_ms: at,
                    cursors: DomainCursors {
                        flow_seq: 1,
                        ..DomainCursors::default()
                    },
                    summaries: vec![terminal.clone()],
                    flow_summaries_truncated: false,
                    metrics: MetricsView::default(),
                    instant,
                    last_activity: None,
                    engine_throughput: None,
                    topology: Arc::new(ProviderHealthSnapshot::default()),
                    backend_metrics: Arc::new(
                        crate::backend_metrics::BackendMetricsSnapshot::default(),
                    ),
                }))
                .await
                .expect("cut persists");
        }
        let cuts = history.cuts_between(None, None).await;
        assert!(
            cuts.iter()
                .any(|cut| cut.cut_id == activity_at as u64 && cut.cut_kind == "activity")
        );
        assert!(!cuts.iter().any(|cut| cut.cut_id == coarse[0] as u64));
        assert!(
            cuts.iter()
                .any(|cut| cut.cut_id == coarse[1] as u64 && cut.resolution_ms == 900_000)
        );
        assert!(!cuts.iter().any(|cut| cut.cut_id == minute[0] as u64));
        assert!(
            cuts.iter()
                .any(|cut| cut.cut_id == minute[1] as u64 && cut.resolution_ms == 60_000)
        );
        assert!(
            recent
                .iter()
                .all(|at| cuts.iter().any(|cut| cut.cut_id == *at as u64))
        );
        let sampled = history.sampled_cuts_between(None, None, 4).await;
        assert_eq!(sampled.retained_cuts, cuts.len());
        assert_eq!(
            sampled.oldest_at_ms,
            cuts.first().map(|cut| cut.snapshot.taken_at_ms)
        );
        assert_eq!(
            sampled.newest_at_ms,
            cuts.last().map(|cut| cut.snapshot.taken_at_ms)
        );
        assert!(sampled.cuts.len() <= 4);
        assert!(
            sampled
                .cuts
                .iter()
                .any(|cut| cut.cut_id == activity_at as u64 && cut.cut_kind == "activity"),
            "bounded history projection always retains activity anchors"
        );

        let connection = reader(&db).expect("reader");
        for (table, expected) in [
            ("flows_latest", 1i64),
            ("terminal_facts", 1),
            ("attempt_facts", 1),
            ("terminal_theater", 1),
            ("artifact_manifests", 1),
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("authoritative count");
            assert_eq!(count, expected, "{table} is never compacted");
        }
        drop(history);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sixteen_concurrent_terminals_restart_with_consistent_totals_and_zero_active() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-history-concurrent-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let db = root.join("history.sqlite3");
        let history = DashboardHistory::enabled_for_test(db.clone(), None);
        let base = epoch_ms_i64().max(10_000) as u128 - 10_000;
        let writes = (0..16u64).map(|index| {
            let history = history.clone();
            async move {
                let mut terminal = summary(
                    &format!("api_concurrent_{index:02}"),
                    base + u128::from(index),
                    FlowStatus::Completed,
                );
                terminal.attempts.push(crate::dashboard_flow::Attempt {
                    provider: Some("provider-a".to_string()),
                    model: Some("model-a".to_string()),
                    start_ms: base + u128::from(index),
                    end_ms: base + u128::from(index) + 100,
                    duration_ms: Some(100),
                    first_upstream_byte_ms: Some(base + u128::from(index) + 10),
                    first_upstream_byte_offset_ms: Some(10),
                    status: crate::dashboard_flow::AttemptStatus::Served,
                    error_class: None,
                    failover_reason: None,
                });
                history
                    .persist_flow_summary(terminal, FlowMutationPhase::Terminal, index + 1)
                    .await
            }
        });
        for result in futures::future::join_all(writes).await {
            result.expect("concurrent terminal persists");
        }
        let page = history
            .latest_flow_page(DurableFlowFilter::default(), None, 100, 0)
            .await
            .expect("flow page");
        assert_eq!(page.total, 16);
        assert_eq!(page.summaries.len(), 16);
        let rollup = history
            .flow_rollup(DurableFlowFilter::default())
            .await
            .expect("rollup");
        assert_eq!(rollup.total, 16);
        assert_eq!(rollup.statuses[0].key, "completed");
        assert_eq!(rollup.statuses[0].count, 16);
        let connection = reader(&db).expect("reader");
        let (terminals, attempts, total_tokens, open): (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM terminal_facts),
                    (SELECT COUNT(*) FROM attempt_facts),
                    (SELECT COALESCE(SUM(total_tokens), 0) FROM terminal_facts),
                    (SELECT COUNT(*) FROM flows_latest WHERE status = 'open')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("archive totals");
        assert_eq!((terminals, attempts, total_tokens, open), (16, 16, 240, 0));
        drop(connection);
        drop(history);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let restarted = DashboardHistory::enabled_for_test(db, None);
        assert_eq!(restarted.bootstrap_terminal_summaries().len(), 16);
        assert!(restarted.bootstrap_cursors().flow_seq >= 16);
        let metrics = crate::metrics::MetricsLayer::new();
        metrics.hydrate_archive(
            restarted.bootstrap_cursors(),
            restarted.bootstrap_terminal_summaries().as_slice(),
        );
        let cut = metrics
            .publish_metrics_cut(
                &crate::dashboard_flow::DashboardFlowStore::new(),
                &crate::upstream::ProviderHealthPublisher::default(),
                0,
                false,
            )
            .expect("post-restart metric cut");
        assert_eq!(cut.instant.active_streams_now, 0);
        assert_eq!(cut.view.window_1h.accepted_requests, 16);
        let provider = cut
            .view
            .window_1h
            .provider_latency("provider-a")
            .expect("provider attempts rehydrated");
        assert_eq!(provider.samples, 16);
        assert_eq!(provider.served, 16);
        assert_eq!(provider.failed, 0);
        drop(restarted);
        let _ = std::fs::remove_dir_all(root);
    }
}
