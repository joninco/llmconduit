//! Bounded state for the OpenAI Responses `store` / `previous_response_id` contract.
//!
//! The default backend is an in-memory TTL-aware LRU.  Operators may opt into a
//! SQLite backing file for restart-safe continuity; SQLite work always runs on
//! Tokio's blocking pool and the same bounded memory store remains the hot cache.

use crate::models::responses::ResponseItem;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{RwLock, Semaphore};

const SQLITE_SCHEMA_VERSION: i64 = 4;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const MEMORY_STORE_BYTE_QUOTA: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredResponse {
    pub id: String,
    pub requested_model: String,
    pub served_model: String,
    pub history: Vec<ResponseItem>,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone)]
pub struct ResponseStoreError(pub String);

impl std::fmt::Display for ResponseStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ResponseStoreError {}

#[derive(Debug)]
struct MemoryInner {
    records: HashMap<String, StoredResponse>,
    record_sizes: HashMap<String, usize>,
    committed_bytes: usize,
    pending: HashMap<String, StoredResponse>,
    pending_sizes: HashMap<String, usize>,
    pending_bytes: usize,
    lru: VecDeque<String>,
    max_entries: usize,
    byte_quota: usize,
}

#[derive(Debug, Clone)]
struct MemoryStore {
    inner: Arc<RwLock<MemoryInner>>,
}

impl MemoryStore {
    fn new(max_entries: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(MemoryInner {
                records: HashMap::new(),
                record_sizes: HashMap::new(),
                committed_bytes: 0,
                pending: HashMap::new(),
                pending_sizes: HashMap::new(),
                pending_bytes: 0,
                lru: VecDeque::new(),
                max_entries: max_entries.max(1),
                byte_quota: MEMORY_STORE_BYTE_QUOTA,
            })),
        }
    }

    async fn prepare(&self, record: StoredResponse) -> Result<(), ResponseStoreError> {
        let size = serialized_size(&record)?;
        let mut inner = self.inner.write().await;
        prune_expired_memory(&mut inner);
        if size > inner.byte_quota {
            return Err(ResponseStoreError(format!(
                "response state is too large to store ({size} bytes; limit {})",
                inner.byte_quota
            )));
        }
        if !inner.pending.contains_key(&record.id) && inner.pending.len() >= inner.max_entries {
            return Err(ResponseStoreError(
                "too many response-state writes are pending".to_string(),
            ));
        }
        let replaced = inner.pending_sizes.get(&record.id).copied().unwrap_or(0);
        let pending_without_replaced = inner.pending_bytes.saturating_sub(replaced);
        if pending_without_replaced.saturating_add(size) > inner.byte_quota {
            return Err(ResponseStoreError(
                "pending response-state byte quota exceeded".to_string(),
            ));
        }
        inner.pending_bytes = pending_without_replaced.saturating_add(size);
        inner.pending_sizes.insert(record.id.clone(), size);
        inner.pending.insert(record.id.clone(), record);
        Ok(())
    }

    async fn publish(&self, id: &str) -> Result<(), ResponseStoreError> {
        let mut inner = self.inner.write().await;
        let record = inner.pending.remove(id).ok_or_else(|| {
            ResponseStoreError("prepared response state was not found".to_string())
        })?;
        let size = inner.pending_sizes.remove(id).unwrap_or(0);
        inner.pending_bytes = inner.pending_bytes.saturating_sub(size);
        insert_committed_memory(&mut inner, record, size);
        Ok(())
    }

    async fn insert_committed_cache(&self, record: StoredResponse) {
        let Ok(size) = serialized_size(&record) else {
            return;
        };
        let mut inner = self.inner.write().await;
        if size > inner.byte_quota {
            return;
        }
        prune_expired_memory(&mut inner);
        insert_committed_memory(&mut inner, record, size);
    }

    async fn get(&self, id: &str) -> Option<StoredResponse> {
        let mut inner = self.inner.write().await;
        prune_expired_memory(&mut inner);
        let record = inner.records.get(id).cloned()?;
        inner.lru.retain(|entry| entry != id);
        inner.lru.push_back(id.to_string());
        Some(record)
    }

    async fn discard(&self, id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(size) = inner.pending_sizes.remove(id) {
            inner.pending_bytes = inner.pending_bytes.saturating_sub(size);
        }
        inner.pending.remove(id);
        remove_committed_memory(&mut inner, id);
    }

    async fn find_item(&self, item_id: &str) -> Option<(StoredResponse, ResponseItem)> {
        let mut inner = self.inner.write().await;
        prune_expired_memory(&mut inner);
        let found = inner.records.values().find_map(|record| {
            record
                .history
                .iter()
                .find(|item| response_item_id(item) == Some(item_id))
                .cloned()
                .map(|item| (record.clone(), item))
        });
        if let Some((record, _)) = &found {
            inner.lru.retain(|entry| entry != &record.id);
            inner.lru.push_back(record.id.clone());
        }
        found
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.inner.read().await.records.len()
    }
}

fn insert_committed_memory(inner: &mut MemoryInner, record: StoredResponse, size: usize) {
    remove_committed_memory(inner, &record.id);
    inner.lru.push_back(record.id.clone());
    inner.committed_bytes = inner.committed_bytes.saturating_add(size);
    inner.record_sizes.insert(record.id.clone(), size);
    inner.records.insert(record.id.clone(), record);
    while inner.records.len() > inner.max_entries || inner.committed_bytes > inner.byte_quota {
        let Some(oldest) = inner.lru.pop_front() else {
            break;
        };
        if let Some(size) = inner.record_sizes.remove(&oldest) {
            inner.committed_bytes = inner.committed_bytes.saturating_sub(size);
        }
        inner.records.remove(&oldest);
    }
}

fn remove_committed_memory(inner: &mut MemoryInner, id: &str) {
    if let Some(size) = inner.record_sizes.remove(id) {
        inner.committed_bytes = inner.committed_bytes.saturating_sub(size);
    }
    inner.records.remove(id);
    inner.lru.retain(|entry| entry != id);
}

fn prune_expired_memory(inner: &mut MemoryInner) {
    let now = now_epoch();
    let expired = inner
        .records
        .iter()
        .filter_map(|(id, record)| (record.expires_at <= now).then_some(id.clone()))
        .collect::<Vec<_>>();
    for id in expired {
        remove_committed_memory(inner, &id);
    }
    let expired_pending = inner
        .pending
        .iter()
        .filter_map(|(id, record)| (record.expires_at <= now).then_some(id.clone()))
        .collect::<Vec<_>>();
    for id in expired_pending {
        if let Some(size) = inner.pending_sizes.remove(&id) {
            inner.pending_bytes = inner.pending_bytes.saturating_sub(size);
        }
        inner.pending.remove(&id);
    }
}

fn serialized_size<T: Serialize>(value: &T) -> Result<usize, ResponseStoreError> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| ResponseStoreError(format!("failed to size response state: {error}")))?;
    Ok(counter.0)
}

#[derive(Debug, Clone)]
struct SqliteStore {
    path: Arc<PathBuf>,
    max_entries: usize,
    gate: Arc<Semaphore>,
}

impl SqliteStore {
    fn new(path: PathBuf, max_entries: usize) -> Result<Self, ResponseStoreError> {
        let max_entries = max_entries.max(1);
        initialize_sqlite(&path, max_entries).map_err(|error| {
            ResponseStoreError(format!(
                "failed to initialize response store {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self {
            path: Arc::new(path),
            max_entries,
            // One bounded lane prevents concurrent read-as-write transactions
            // from filling Tokio's blocking pool. The acquired permit moves
            // into the blocking closure so cancellation cannot release the lane
            // while SQLite is still executing.
            gate: Arc::new(Semaphore::new(1)),
        })
    }

    async fn prepare(&self, record: StoredResponse) -> Result<(), ResponseStoreError> {
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| ResponseStoreError("response-store worker closed".to_string()))?;
        let path = Arc::clone(&self.path);
        let max_entries = self.max_entries;
        tokio::task::spawn_blocking(move || {
            // The blocking operation cannot be force-cancelled once started.
            // Keep ownership of the lane here so dropping the async waiter does
            // not allow another operation to overlap it.
            let _permit = permit;
            prepare_sqlite(&path, &record, max_entries)
        })
        .await
        .map_err(|error| ResponseStoreError(format!("response-store worker failed: {error}")))?
        .map_err(|error| ResponseStoreError(format!("failed to persist response: {error}")))
    }

    async fn publish(&self, id: &str) -> Result<(), ResponseStoreError> {
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| ResponseStoreError("response-store worker closed".to_string()))?;
        let path = Arc::clone(&self.path);
        let id = id.to_string();
        let max_entries = self.max_entries;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            publish_sqlite(&path, &id, max_entries)
        })
        .await
        .map_err(|error| ResponseStoreError(format!("response-store worker failed: {error}")))?
        .map_err(|error| ResponseStoreError(format!("failed to publish response: {error}")))
    }

    async fn get(&self, id: &str) -> Result<Option<StoredResponse>, ResponseStoreError> {
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| ResponseStoreError("response-store worker closed".to_string()))?;
        let path = Arc::clone(&self.path);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            get_sqlite(&path, &id)
        })
        .await
        .map_err(|error| ResponseStoreError(format!("response-store worker failed: {error}")))?
        .map_err(|error| ResponseStoreError(format!("failed to read response state: {error}")))
    }

    async fn delete(&self, id: &str) -> Result<(), ResponseStoreError> {
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| ResponseStoreError("response-store worker closed".to_string()))?;
        let path = Arc::clone(&self.path);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            delete_sqlite(&path, &id)
        })
        .await
        .map_err(|error| ResponseStoreError(format!("response-store worker failed: {error}")))?
        .map_err(|error| ResponseStoreError(format!("failed to delete response state: {error}")))
    }

    async fn find_item(
        &self,
        item_id: &str,
    ) -> Result<Option<(StoredResponse, ResponseItem)>, ResponseStoreError> {
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| ResponseStoreError("response-store worker closed".to_string()))?;
        let path = Arc::clone(&self.path);
        let item_id = item_id.to_string();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            find_item_sqlite(&path, &item_id)
        })
        .await
        .map_err(|error| ResponseStoreError(format!("response-store worker failed: {error}")))?
        .map_err(|error| ResponseStoreError(format!("failed to read stored item: {error}")))
    }
}

/// Cloneable response-state handle.  A SQLite-backed instance always retains a
/// bounded memory front cache; a memory-only instance performs no filesystem IO.
#[async_trait]
pub trait ResponseStore: Send + Sync {
    /// Persist a hidden record. Prepared state is never returned by `get` or
    /// `find_item`, so cancellation may stop waiting while a blocking SQLite
    /// write finishes without making an undelivered response referenceable.
    async fn prepare(
        &self,
        id: String,
        requested_model: String,
        served_model: String,
        history: Vec<ResponseItem>,
        created_at: i64,
    ) -> Result<(), ResponseStoreError>;
    /// Atomically make a prepared response referenceable immediately before
    /// its terminal event is emitted.
    async fn publish(&self, id: &str) -> Result<(), ResponseStoreError>;
    async fn get(&self, id: &str) -> Result<Option<StoredResponse>, ResponseStoreError>;
    async fn delete(&self, id: &str) -> Result<(), ResponseStoreError>;
    async fn find_item(&self, item_id: &str) -> Result<Option<ResponseItem>, ResponseStoreError>;
}

#[derive(Debug, Clone)]
pub struct ResponseStoreHandle {
    memory: MemoryStore,
    sqlite: Option<SqliteStore>,
    retention_seconds: i64,
}

impl ResponseStoreHandle {
    pub fn memory(max_entries: usize, retention_hours: u64) -> Self {
        Self {
            memory: MemoryStore::new(max_entries),
            sqlite: None,
            retention_seconds: retention_seconds(retention_hours),
        }
    }

    pub fn sqlite(
        path: PathBuf,
        max_entries: usize,
        retention_hours: u64,
    ) -> Result<Self, ResponseStoreError> {
        Ok(Self {
            memory: MemoryStore::new(max_entries),
            sqlite: Some(SqliteStore::new(path, max_entries)?),
            retention_seconds: retention_seconds(retention_hours),
        })
    }

    pub async fn prepare(
        &self,
        id: String,
        requested_model: String,
        served_model: String,
        history: Vec<ResponseItem>,
        created_at: i64,
    ) -> Result<(), ResponseStoreError> {
        let record = StoredResponse {
            id,
            requested_model,
            served_model,
            history,
            created_at,
            expires_at: now_epoch().saturating_add(self.retention_seconds),
        };
        // Prepared SQLite rows are durable but hidden (`committed = 0`). A
        // cancellation cleanup may therefore run after this blocking write
        // without any interval in which clients can reference the response.
        if let Some(sqlite) = &self.sqlite {
            sqlite.prepare(record).await?;
        } else {
            self.memory.prepare(record).await?;
        }
        Ok(())
    }

    pub async fn publish(&self, id: &str) -> Result<(), ResponseStoreError> {
        if let Some(sqlite) = &self.sqlite {
            sqlite.publish(id).await?;
            if let Some(record) = sqlite.get(id).await? {
                self.memory.insert_committed_cache(record).await;
            }
        } else {
            self.memory.publish(id).await?;
        }
        Ok(())
    }

    /// Convenience used by direct store tests and non-streaming callers: the
    /// public engine uses explicit prepare/publish around terminal delivery.
    pub async fn insert(
        &self,
        id: String,
        requested_model: String,
        served_model: String,
        history: Vec<ResponseItem>,
        created_at: i64,
    ) -> Result<(), ResponseStoreError> {
        self.prepare(
            id.clone(),
            requested_model,
            served_model,
            history,
            created_at,
        )
        .await?;
        self.publish(&id).await
    }

    pub async fn get(&self, id: &str) -> Result<Option<StoredResponse>, ResponseStoreError> {
        let Some(sqlite) = &self.sqlite else {
            return Ok(self.memory.get(id).await);
        };

        // SQLite is authoritative across handles and restarts. Always consult it
        // before returning a front-cache value so a rollback/delete performed by
        // another gateway handle cannot leave referenceable stale state here.
        let record = sqlite.get(id).await?;
        match record.clone() {
            Some(record) => self.memory.insert_committed_cache(record).await,
            None => self.memory.discard(id).await,
        }
        Ok(record)
    }

    /// Remove a stored response. This is the rollback seam for a caller that
    /// persisted immediately before a terminal event but then failed to publish
    /// that event. Durable state is removed first; a failed SQLite delete leaves
    /// the matching memory entry untouched and therefore cannot falsely claim a
    /// successful rollback.
    pub async fn delete(&self, id: &str) -> Result<(), ResponseStoreError> {
        if let Some(sqlite) = &self.sqlite {
            sqlite.delete(id).await?;
        }
        self.memory.discard(id).await;
        Ok(())
    }

    pub async fn find_item(
        &self,
        item_id: &str,
    ) -> Result<Option<ResponseItem>, ResponseStoreError> {
        let found = if let Some(sqlite) = &self.sqlite {
            sqlite.find_item(item_id).await?
        } else {
            self.memory.find_item(item_id).await
        };
        let Some((record, item)) = found else {
            return Ok(None);
        };
        self.memory.insert_committed_cache(record).await;
        Ok(Some(item))
    }

    #[cfg(test)]
    async fn memory_len(&self) -> usize {
        self.memory.len().await
    }
}

#[async_trait]
impl ResponseStore for ResponseStoreHandle {
    async fn prepare(
        &self,
        id: String,
        requested_model: String,
        served_model: String,
        history: Vec<ResponseItem>,
        created_at: i64,
    ) -> Result<(), ResponseStoreError> {
        ResponseStoreHandle::prepare(self, id, requested_model, served_model, history, created_at)
            .await
    }

    async fn publish(&self, id: &str) -> Result<(), ResponseStoreError> {
        ResponseStoreHandle::publish(self, id).await
    }

    async fn get(&self, id: &str) -> Result<Option<StoredResponse>, ResponseStoreError> {
        ResponseStoreHandle::get(self, id).await
    }

    async fn delete(&self, id: &str) -> Result<(), ResponseStoreError> {
        ResponseStoreHandle::delete(self, id).await
    }

    async fn find_item(&self, item_id: &str) -> Result<Option<ResponseItem>, ResponseStoreError> {
        ResponseStoreHandle::find_item(self, item_id).await
    }
}

fn retention_seconds(hours: u64) -> i64 {
    hours.max(1).saturating_mul(60 * 60).min(i64::MAX as u64) as i64
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn open_sqlite(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    Ok(connection)
}

fn initialize_sqlite(path: &Path, max_entries: usize) -> rusqlite::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_parent(parent)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    }
    prepare_sqlite_file(path)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let mut connection = open_sqlite(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS response_store_meta (
             schema_version INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS stored_responses (
             id TEXT PRIMARY KEY,
             payload TEXT NOT NULL,
             expires_at INTEGER NOT NULL,
             last_access INTEGER NOT NULL,
             committed INTEGER NOT NULL DEFAULT 1
         );
         CREATE INDEX IF NOT EXISTS stored_responses_expiry
             ON stored_responses(expires_at);
         CREATE INDEX IF NOT EXISTS stored_responses_lru
             ON stored_responses(last_access);
         CREATE TABLE IF NOT EXISTS stored_response_items (
             item_id TEXT NOT NULL,
             response_id TEXT NOT NULL,
             PRIMARY KEY(item_id, response_id)
         );
         CREATE INDEX IF NOT EXISTS stored_response_items_response
             ON stored_response_items(response_id);",
    )?;
    let version: Option<i64> = connection
        .query_row(
            "SELECT schema_version FROM response_store_meta LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version {
        None => {
            connection.execute(
                "INSERT INTO response_store_meta(schema_version) VALUES (?1)",
                [SQLITE_SCHEMA_VERSION],
            )?;
        }
        Some(1) | Some(2) => {
            migrate_sqlite_item_index(&mut connection)?;
            add_sqlite_committed_state(&connection)?;
        }
        Some(3) => add_sqlite_committed_state(&connection)?,
        Some(SQLITE_SCHEMA_VERSION) => {}
        Some(_) => return Err(rusqlite::Error::InvalidQuery),
    }
    let now = now_epoch();
    prune_sqlite(&connection, now, max_entries)?;
    prune_sqlite_pending(&connection, now, max_entries)?;
    drop(connection);
    restrict_file(path)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    Ok(())
}

fn prepare_sqlite(
    path: &Path,
    record: &StoredResponse,
    max_entries: usize,
) -> rusqlite::Result<()> {
    let mut connection = open_sqlite(path)?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(record)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let now = now_epoch();
    // Do not evict another in-flight hidden write to admit this one. Its owner
    // may be about to publish it; memory mode rejects the newcomer under the
    // same condition. Startup pruning still bounds orphaned pending rows after
    // a configured capacity reduction.
    prune_sqlite_pending(&transaction, now, max_entries)?;
    let pending_others: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM stored_responses WHERE committed = 0 AND id != ?1",
        [&record.id],
        |row| row.get(0),
    )?;
    if pending_others >= max_entries.max(1).min(i64::MAX as usize) as i64 {
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            ResponseStoreError("too many response-state writes are pending".to_string()),
        )));
    }
    let access = next_access_stamp(&transaction)?;
    transaction.execute(
        "INSERT INTO stored_responses(id, payload, expires_at, last_access, committed)
         VALUES (?1, ?2, ?3, ?4, 0)
         ON CONFLICT(id) DO UPDATE SET
             payload=excluded.payload,
             expires_at=excluded.expires_at,
             last_access=excluded.last_access,
             committed=0",
        params![record.id, payload, record.expires_at, access],
    )?;
    transaction.execute(
        "DELETE FROM stored_response_items WHERE response_id = ?1",
        [&record.id],
    )?;
    for item_id in record.history.iter().filter_map(response_item_id) {
        transaction.execute(
            "INSERT INTO stored_response_items(item_id, response_id)
             VALUES (?1, ?2)
             ON CONFLICT(item_id, response_id) DO NOTHING",
            params![item_id, record.id],
        )?;
    }
    transaction.commit()
}

fn publish_sqlite(path: &Path, id: &str, max_entries: usize) -> rusqlite::Result<()> {
    let mut connection = open_sqlite(path)?;
    let transaction = connection.transaction()?;
    let changed = transaction.execute(
        "UPDATE stored_responses SET committed = 1 WHERE id = ?1 AND committed = 0",
        [id],
    )?;
    if changed == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    prune_sqlite(&transaction, now_epoch(), max_entries)?;
    transaction.commit()
}

fn get_sqlite(path: &Path, id: &str) -> rusqlite::Result<Option<StoredResponse>> {
    let mut connection = open_sqlite(path)?;
    let transaction = connection.transaction()?;
    let now = now_epoch();
    transaction.execute("DELETE FROM stored_responses WHERE expires_at <= ?1", [now])?;
    let payload: Option<String> = transaction
        .query_row(
            "SELECT payload FROM stored_responses WHERE id = ?1 AND committed = 1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    if payload.is_some() {
        let access = next_access_stamp(&transaction)?;
        transaction.execute(
            "UPDATE stored_responses SET last_access = ?2 WHERE id = ?1",
            params![id, access],
        )?;
    }
    transaction.commit()?;
    payload
        .map(|payload| {
            serde_json::from_str(&payload).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    payload.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn delete_sqlite(path: &Path, id: &str) -> rusqlite::Result<()> {
    let mut connection = open_sqlite(path)?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM stored_response_items WHERE response_id = ?1",
        [id],
    )?;
    transaction.execute("DELETE FROM stored_responses WHERE id = ?1", [id])?;
    transaction.commit()
}

fn find_item_sqlite(
    path: &Path,
    item_id: &str,
) -> rusqlite::Result<Option<(StoredResponse, ResponseItem)>> {
    let mut connection = open_sqlite(path)?;
    let transaction = connection.transaction()?;
    let now = now_epoch();
    transaction.execute("DELETE FROM stored_responses WHERE expires_at <= ?1", [now])?;
    transaction.execute(
        "DELETE FROM stored_response_items
         WHERE response_id NOT IN (SELECT id FROM stored_responses)",
        [],
    )?;
    let row: Option<(String, String)> = transaction
        .query_row(
            "SELECT responses.id, responses.payload
             FROM stored_response_items AS items
             JOIN stored_responses AS responses ON responses.id = items.response_id
             WHERE items.item_id = ?1 AND responses.committed = 1
             ORDER BY responses.id ASC
             LIMIT 1",
            [item_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let found = row
        .map(|(response_id, payload)| {
            let record: StoredResponse = serde_json::from_str(&payload).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    payload.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            let item = record
                .history
                .iter()
                .find(|item| response_item_id(item) == Some(item_id))
                .cloned()
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let access = next_access_stamp(&transaction)?;
            transaction.execute(
                "UPDATE stored_responses SET last_access = ?2 WHERE id = ?1",
                params![response_id, access],
            )?;
            Ok::<(StoredResponse, ResponseItem), rusqlite::Error>((record, item))
        })
        .transpose()?;
    transaction.commit()?;
    Ok(found)
}

fn next_access_stamp(connection: &Connection) -> rusqlite::Result<i64> {
    let latest: Option<i64> =
        connection.query_row("SELECT MAX(last_access) FROM stored_responses", [], |row| {
            row.get(0)
        })?;
    Ok(latest.unwrap_or(0).saturating_add(1))
}

fn migrate_sqlite_item_index(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    // Stored response payloads are authoritative. Rebuild the derived index in
    // one transaction so an interrupted v1/v2 upgrade cannot expose a partially
    // migrated ownership table.
    transaction.execute_batch(
        "DROP TABLE IF EXISTS stored_response_items;
         CREATE TABLE stored_response_items (
             item_id TEXT NOT NULL,
             response_id TEXT NOT NULL,
             PRIMARY KEY(item_id, response_id)
         );
         CREATE INDEX stored_response_items_response
             ON stored_response_items(response_id);",
    )?;
    rebuild_sqlite_item_index(&transaction)?;
    transaction.commit()
}

fn add_sqlite_committed_state(connection: &Connection) -> rusqlite::Result<()> {
    let transaction = connection.unchecked_transaction()?;
    let has_column = {
        let mut statement = transaction.prepare("PRAGMA table_info(stored_responses)")?;
        let names = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = false;
        for name in names {
            if name? == "committed" {
                found = true;
                break;
            }
        }
        found
    };
    if !has_column {
        transaction.execute(
            "ALTER TABLE stored_responses ADD COLUMN committed INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    transaction.execute(
        "UPDATE response_store_meta SET schema_version = ?1",
        [SQLITE_SCHEMA_VERSION],
    )?;
    transaction.commit()
}

fn rebuild_sqlite_item_index(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM stored_response_items", [])?;
    let mut after = String::new();
    loop {
        let row: Option<(String, String)> = connection
            .query_row(
                "SELECT id, payload FROM stored_responses
                 WHERE id > ?1 ORDER BY id LIMIT 1",
                [&after],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((response_id, payload)) = row else {
            break;
        };
        let record: StoredResponse = serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                payload.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        for item_id in record.history.iter().filter_map(response_item_id) {
            connection.execute(
                "INSERT OR IGNORE INTO stored_response_items(item_id, response_id)
                 VALUES (?1, ?2)",
                params![item_id, response_id],
            )?;
        }
        after = response_id;
    }
    Ok(())
}

fn response_item_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::ItemReference { id }
        | ResponseItem::Reasoning { id, .. }
        | ResponseItem::ImageGenerationCall { id, .. } => Some(id),
        ResponseItem::Message { id, .. }
        | ResponseItem::FunctionCall { id, .. }
        | ResponseItem::CustomToolCall { id, .. }
        | ResponseItem::ToolSearchCall { id, .. }
        | ResponseItem::LocalShellCall { id, .. }
        | ResponseItem::WebSearchCall { id, .. } => id.as_deref(),
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. } => None,
    }
}

fn prune_sqlite(connection: &Connection, now: i64, max_entries: usize) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM stored_responses WHERE expires_at <= ?1", [now])?;
    connection.execute(
        "DELETE FROM stored_responses WHERE id IN (
             SELECT id FROM stored_responses WHERE committed = 1
             ORDER BY last_access ASC, id ASC
             LIMIT MAX((SELECT COUNT(*) FROM stored_responses WHERE committed = 1) - ?1, 0)
         )",
        [max_entries.max(1).min(i64::MAX as usize) as i64],
    )?;
    connection.execute(
        "DELETE FROM stored_response_items
         WHERE response_id NOT IN (SELECT id FROM stored_responses)",
        [],
    )?;
    Ok(())
}

fn prune_sqlite_pending(
    connection: &Connection,
    now: i64,
    max_entries: usize,
) -> rusqlite::Result<()> {
    connection.execute("DELETE FROM stored_responses WHERE expires_at <= ?1", [now])?;
    connection.execute(
        "DELETE FROM stored_responses WHERE id IN (
             SELECT id FROM stored_responses WHERE committed = 0
             ORDER BY last_access ASC, id ASC
             LIMIT MAX((SELECT COUNT(*) FROM stored_responses WHERE committed = 0) - ?1, 0)
         )",
        [max_entries.max(1).min(i64::MAX as usize) as i64],
    )?;
    connection.execute(
        "DELETE FROM stored_response_items
         WHERE response_id NOT IN (SELECT id FROM stored_responses)",
        [],
    )?;
    Ok(())
}

#[cfg(unix)]
fn prepare_sqlite_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "response-store database path must not be a symbolic link",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    restrict_file(path)
}

#[cfg(not(unix))]
fn prepare_sqlite_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    restrict_file(path)
}

#[cfg(unix)]
fn create_private_parent(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_parent(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::responses::{ContentItem, ResponseItem};

    fn history(text: &str) -> Vec<ResponseItem> {
        vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
        }]
    }

    fn history_with_id(id: &str, text: &str) -> Vec<ResponseItem> {
        vec![ResponseItem::Message {
            id: Some(id.to_string()),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            phase: None,
        }]
    }

    #[tokio::test]
    async fn memory_store_is_bounded_and_referenceable() {
        let store = ResponseStoreHandle::memory(2, 720);
        for index in 0..3 {
            store
                .insert(
                    format!("resp_{index}"),
                    "requested".to_string(),
                    "served".to_string(),
                    history(&format!("message {index}")),
                    now_epoch(),
                )
                .await
                .unwrap();
        }
        assert!(store.get("resp_0").await.unwrap().is_none());
        assert!(store.get("resp_2").await.unwrap().is_some());
        assert_eq!(store.memory_len().await, 2);
    }

    #[tokio::test]
    async fn prepared_memory_response_is_hidden_until_publish() {
        let store = ResponseStoreHandle::memory(2, 720);
        store
            .prepare(
                "resp_pending".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_pending", "not delivered yet"),
                now_epoch(),
            )
            .await
            .unwrap();
        assert!(store.get("resp_pending").await.unwrap().is_none());
        assert!(store.find_item("msg_pending").await.unwrap().is_none());
        store.publish("resp_pending").await.unwrap();
        assert!(store.get("resp_pending").await.unwrap().is_some());
        assert!(store.find_item("msg_pending").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn memory_store_enforces_a_payload_byte_quota() {
        let store = ResponseStoreHandle::memory(10, 720);
        store.memory.inner.write().await.byte_quota = 256;
        let error = store
            .prepare(
                "resp_large".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history(&"x".repeat(1024)),
                now_epoch(),
            )
            .await
            .expect_err("oversized response state must fail closed");
        assert!(error.to_string().contains("too large"));
        assert!(store.get("resp_large").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_pending_replacement_preserves_record_and_byte_accounting() {
        let expires_at = now_epoch() + 3600;
        let old = StoredResponse {
            id: "resp_replace".to_string(),
            requested_model: "requested".to_string(),
            served_model: "served".to_string(),
            history: history("old"),
            created_at: now_epoch(),
            expires_at,
        };
        let other = StoredResponse {
            id: "resp_other".to_string(),
            requested_model: "requested".to_string(),
            served_model: "served".to_string(),
            history: history("other"),
            created_at: now_epoch(),
            expires_at,
        };
        let replacement = StoredResponse {
            history: history(&"replacement".repeat(512)),
            ..old.clone()
        };
        let old_size = serialized_size(&old).unwrap();
        let other_size = serialized_size(&other).unwrap();
        let replacement_size = serialized_size(&replacement).unwrap();
        assert!(old_size + other_size < replacement_size);

        let memory = MemoryStore::new(3);
        memory.inner.write().await.byte_quota = replacement_size;
        memory.prepare(old.clone()).await.unwrap();
        memory.prepare(other.clone()).await.unwrap();
        memory
            .prepare(replacement)
            .await
            .expect_err("a replacement must include all other pending bytes");

        {
            let inner = memory.inner.read().await;
            assert_eq!(inner.pending.get(&old.id), Some(&old));
            assert_eq!(inner.pending_sizes.get(&old.id), Some(&old_size));
            assert_eq!(inner.pending_sizes.get(&other.id), Some(&other_size));
            assert_eq!(inner.pending_bytes, old_size + other_size);
        }

        memory.publish(&old.id).await.unwrap();
        let inner = memory.inner.read().await;
        assert_eq!(inner.pending_bytes, other_size);
        assert_eq!(inner.committed_bytes, old_size);
        assert_eq!(inner.records.get(&old.id), Some(&old));
    }

    #[tokio::test]
    async fn delete_makes_memory_response_unreferenceable() {
        let store = ResponseStoreHandle::memory(2, 720);
        store
            .insert(
                "resp_delete".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history("delete me"),
                now_epoch(),
            )
            .await
            .unwrap();
        store.delete("resp_delete").await.unwrap();
        assert!(store.get("resp_delete").await.unwrap().is_none());
        assert_eq!(store.memory_len().await, 0);
    }

    #[tokio::test]
    async fn sqlite_store_survives_handle_recreation() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-store-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let first = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        first
            .insert(
                "resp_saved".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history("persist me"),
                now_epoch(),
            )
            .await
            .unwrap();
        drop(first);

        let second = ResponseStoreHandle::sqlite(path, 10, 720).unwrap();
        let restored = second.get("resp_saved").await.unwrap().unwrap();
        assert_eq!(restored.history, history("persist me"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn prepared_sqlite_response_is_hidden_across_restart_until_publish() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-pending-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let first = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        first
            .prepare(
                "resp_pending".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_pending", "pending"),
                now_epoch(),
            )
            .await
            .unwrap();
        assert!(first.get("resp_pending").await.unwrap().is_none());
        drop(first);

        let restarted = ResponseStoreHandle::sqlite(path, 10, 720).unwrap();
        assert!(restarted.get("resp_pending").await.unwrap().is_none());
        assert!(restarted.find_item("msg_pending").await.unwrap().is_none());
        restarted.publish("resp_pending").await.unwrap();
        assert!(restarted.get("resp_pending").await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_startup_prunes_pending_rows_to_the_configured_bound() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-pending-prune-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let first = ResponseStoreHandle::sqlite(path.clone(), 3, 720).unwrap();
        for id in ["resp_a", "resp_b", "resp_c"] {
            first
                .prepare(
                    id.to_string(),
                    "requested".to_string(),
                    "served".to_string(),
                    history(id),
                    now_epoch(),
                )
                .await
                .unwrap();
        }
        drop(first);

        let restarted = ResponseStoreHandle::sqlite(path.clone(), 1, 720).unwrap();
        drop(restarted);
        let connection = Connection::open(&path).unwrap();
        let pending = connection
            .prepare("SELECT id FROM stored_responses WHERE committed = 0 ORDER BY last_access, id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(pending, vec!["resp_c"]);
        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_pending_capacity_rejects_new_write_without_evicting_inflight_state() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-pending-capacity-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path, 1, 720).unwrap();
        store
            .prepare(
                "resp_first".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history("first"),
                now_epoch(),
            )
            .await
            .unwrap();
        let error = store
            .prepare(
                "resp_second".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history("second"),
                now_epoch(),
            )
            .await
            .expect_err("a new write must not evict an in-flight prepared response");
        assert!(error.to_string().contains("too many response-state writes"));

        store.publish("resp_first").await.unwrap();
        assert!(store.get("resp_first").await.unwrap().is_some());
        assert!(store.get("resp_second").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_busy_lock_is_bounded() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-busy-bound-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let result = tokio::time::timeout(
            SQLITE_BUSY_TIMEOUT + Duration::from_secs(2),
            store.get("resp_blocked"),
        )
        .await
        .expect("SQLite busy timeout must bound a locked operation");
        assert!(result.is_err(), "the external writer lock must be reported");

        blocker.execute_batch("ROLLBACK").unwrap();
        assert!(store.get("resp_unblocked").await.unwrap().is_none());
        drop(blocker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_sqlite_waiter_does_not_release_an_active_worker_lane() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-cancelled-worker-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        let sqlite = store.sqlite.as_ref().unwrap().clone();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let worker_store = store.clone();
        let worker = tokio::spawn(async move { worker_store.get("resp_blocked").await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while sqlite.gate.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the SQLite worker must acquire its lane");
        tokio::time::sleep(Duration::from_millis(25)).await;
        worker.abort();
        let _ = worker.await;
        assert_eq!(
            sqlite.gate.available_permits(),
            0,
            "the blocking closure, not the cancelled waiter, owns the permit"
        );

        blocker.execute_batch("ROLLBACK").unwrap();
        tokio::time::timeout(SQLITE_BUSY_TIMEOUT + Duration::from_secs(1), async {
            while sqlite.gate.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the worker lane must be released when SQLite actually returns");
        assert!(store.get("resp_unblocked").await.unwrap().is_none());
        drop(blocker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_item_index_survives_restart() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-item-store-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let first = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        first
            .insert(
                "resp_item".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_stable", "persisted item"),
                now_epoch(),
            )
            .await
            .unwrap();
        drop(first);

        let second = ResponseStoreHandle::sqlite(path, 10, 720).unwrap();
        let item = second.find_item("msg_stable").await.unwrap().unwrap();
        assert_eq!(response_item_id(&item), Some("msg_stable"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_lru_uses_access_order_even_within_one_second() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-lru-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path, 2, 720).unwrap();
        for id in ["resp_a", "resp_z"] {
            store
                .insert(
                    id.to_string(),
                    "requested".to_string(),
                    "served".to_string(),
                    history(id),
                    now_epoch(),
                )
                .await
                .unwrap();
        }
        assert!(store.get("resp_a").await.unwrap().is_some());
        store
            .insert(
                "resp_m".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history("new"),
                now_epoch(),
            )
            .await
            .unwrap();
        assert!(store.get("resp_a").await.unwrap().is_some());
        assert!(store.get("resp_z").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_schema_v1_migrates_item_index() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-migrate-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let first = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        first
            .insert(
                "resp_old".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_old", "old schema item"),
                now_epoch(),
            )
            .await
            .unwrap();
        drop(first);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("UPDATE response_store_meta SET schema_version = 1", [])
            .unwrap();
        connection
            .execute("DELETE FROM stored_response_items", [])
            .unwrap();
        drop(connection);

        let migrated = ResponseStoreHandle::sqlite(path, 10, 720).unwrap();
        assert!(migrated.find_item("msg_old").await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_schema_v2_shared_item_survives_child_delete_and_restart() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-shared-item-migrate-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        store
            .insert(
                "resp_parent".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_shared", "parent output"),
                now_epoch(),
            )
            .await
            .unwrap();
        let mut child_history = history_with_id("msg_shared", "parent output");
        child_history.extend(history_with_id("msg_child", "child output"));
        store
            .insert(
                "resp_child".to_string(),
                "requested".to_string(),
                "served".to_string(),
                child_history,
                now_epoch(),
            )
            .await
            .unwrap();
        drop(store);

        // Recreate the v2 single-owner index as it would look after the child
        // overwrote the parent's ownership row. The v3 migration must rebuild
        // both owners from the authoritative response payloads.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "UPDATE response_store_meta SET schema_version = 2;
                 DROP TABLE stored_response_items;
                 CREATE TABLE stored_response_items (
                     item_id TEXT PRIMARY KEY,
                     response_id TEXT NOT NULL
                 );
                 CREATE INDEX stored_response_items_response
                     ON stored_response_items(response_id);
                 INSERT INTO stored_response_items(item_id, response_id)
                     VALUES ('msg_shared', 'resp_child');
                 INSERT INTO stored_response_items(item_id, response_id)
                     VALUES ('msg_child', 'resp_child');",
            )
            .unwrap();
        drop(connection);

        let migrated = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        let (owner, item) = migrated
            .sqlite
            .as_ref()
            .unwrap()
            .find_item("msg_shared")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            owner.id, "resp_child",
            "lowest response id wins deterministically"
        );
        assert_eq!(response_item_id(&item), Some("msg_shared"));

        migrated.delete("resp_child").await.unwrap();
        let (owner, _) = migrated
            .sqlite
            .as_ref()
            .unwrap()
            .find_item("msg_shared")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.id, "resp_parent");
        drop(migrated);

        let restarted = ResponseStoreHandle::sqlite(path, 10, 720).unwrap();
        assert!(restarted.get("resp_parent").await.unwrap().is_some());
        assert!(restarted.get("resp_child").await.unwrap().is_none());
        assert_eq!(
            restarted
                .find_item("msg_shared")
                .await
                .unwrap()
                .as_ref()
                .and_then(response_item_id),
            Some("msg_shared")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_shared_item_survives_child_eviction_and_restart() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-shared-item-evict-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path.clone(), 2, 720).unwrap();
        store
            .insert(
                "resp_parent".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_shared", "parent output"),
                now_epoch(),
            )
            .await
            .unwrap();
        let mut child_history = history_with_id("msg_shared", "parent output");
        child_history.extend(history_with_id("msg_child", "child output"));
        store
            .insert(
                "resp_child".to_string(),
                "requested".to_string(),
                "served".to_string(),
                child_history,
                now_epoch(),
            )
            .await
            .unwrap();

        // Touch the parent so the child is the LRU row removed by the next insert.
        assert!(store.get("resp_parent").await.unwrap().is_some());
        store
            .insert(
                "resp_new".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history_with_id("msg_new", "new output"),
                now_epoch(),
            )
            .await
            .unwrap();
        assert!(store.get("resp_child").await.unwrap().is_none());
        assert!(store.get("resp_parent").await.unwrap().is_some());
        drop(store);

        let restarted = ResponseStoreHandle::sqlite(path, 2, 720).unwrap();
        let (owner, item) = restarted
            .sqlite
            .as_ref()
            .unwrap()
            .find_item("msg_shared")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.id, "resp_parent");
        assert_eq!(response_item_id(&item), Some("msg_shared"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sqlite_delete_is_authoritative_across_front_caches() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-store-authoritative-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let first = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        let second = ResponseStoreHandle::sqlite(path, 10, 720).unwrap();
        first
            .insert(
                "resp_shared".to_string(),
                "requested".to_string(),
                "served".to_string(),
                history("shared"),
                now_epoch(),
            )
            .await
            .unwrap();
        assert!(first.get("resp_shared").await.unwrap().is_some());

        second.delete("resp_shared").await.unwrap();
        assert!(
            first.get("resp_shared").await.unwrap().is_none(),
            "SQLite, not a stale per-handle front cache, is authoritative"
        );
        assert_eq!(first.memory_len().await, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sqlite_startup_prunes_expired_and_over_capacity_rows() {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-store-prune-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(path.clone(), 10, 720).unwrap();
        drop(store);

        let connection = Connection::open(&path).unwrap();
        for (id, expires_at, last_access) in [
            ("expired", now_epoch() - 1, 1),
            ("old", now_epoch() + 3600, 2),
            ("new", now_epoch() + 3600, 3),
        ] {
            let record = StoredResponse {
                id: id.to_string(),
                requested_model: "requested".to_string(),
                served_model: "served".to_string(),
                history: history(id),
                created_at: now_epoch(),
                expires_at,
            };
            connection
                .execute(
                    "INSERT INTO stored_responses(id, payload, expires_at, last_access)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id,
                        serde_json::to_string(&record).unwrap(),
                        expires_at,
                        last_access
                    ],
                )
                .unwrap();
        }
        drop(connection);

        let store = ResponseStoreHandle::sqlite(path.clone(), 1, 720).unwrap();
        drop(store);
        let connection = Connection::open(path).unwrap();
        let ids = connection
            .prepare("SELECT id FROM stored_responses ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(ids, vec!["new"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_secures_new_files_without_chmodding_existing_parent() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "llmconduit-response-store-mode-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let existing_parent_path = root.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(existing_parent_path.clone(), 10, 720).unwrap();
        drop(store);
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755,
            "an existing operator-owned parent keeps its permissions"
        );
        assert_eq!(
            std::fs::metadata(&existing_parent_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the database itself is private"
        );

        let private_parent = root.join("new-private-parent");
        let private_path = private_parent.join("responses.sqlite3");
        let store = ResponseStoreHandle::sqlite(private_path.clone(), 10, 720).unwrap();
        drop(store);
        assert_eq!(
            std::fs::metadata(&private_parent)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&private_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(restrict_file(&root.join("missing.sqlite3")).is_err());

        let symlink_target = root.join("symlink-target.sqlite3");
        std::fs::write(&symlink_target, b"do not follow").unwrap();
        let symlink_path = root.join("symlink.sqlite3");
        symlink(&symlink_target, &symlink_path).unwrap();
        assert!(ResponseStoreHandle::sqlite(symlink_path, 10, 720).is_err());
        assert_eq!(std::fs::read(&symlink_target).unwrap(), b"do not follow");
        let _ = std::fs::remove_dir_all(root);
    }
}
