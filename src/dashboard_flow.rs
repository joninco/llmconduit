//! D1 — `DashboardFlowStore`: the authoritative per-flow record store and the
//! on-wire capture seam that feeds the transformation inspector + metrics.
//!
//! Architecture (DASHBOARD_PLAN §4.2/§4.7, Codex round-2/4/5 fixes):
//! - The store is `DashboardFlowStore { state: Mutex<DashboardFlowState> }` with a
//!   `MonitorHub`-style `new()`/`disabled()` split: when `--with-debug-ui` is off
//!   the store is `disabled()` and EVERY operation is a no-op, so the production
//!   hot path keeps zero overhead.
//! - `by_id` (the record map) and `order` (the LRU/insertion deque) are mutated
//!   together under the ONE `Mutex` — there is no exterior LRU that could drift.
//! - Records are `Arc<FlowRecord>` replaced copy-on-write on mutation: each
//!   mutator clones the inner record, edits the clone, and swaps the `Arc`. The
//!   `claim: Arc<AtomicU8>` is CLONED (shared) across COW copies so the atomic
//!   identity persists for D3's `OpenL0 → ClaimedL1 → Finalized` CAS guard. D1
//!   only ALLOCATES `claim` as `OpenL0`; D3 owns the transitions.
//! - Bodies are owned, capped `Arc<[u8]>` produced by a redacting STREAMING JSON
//!   serializer (`capture_body`): peak serializer memory is O(CAP), not O(body),
//!   and a `Bytes::slice` of the 256 MiB middleware buffer is NEVER retained (a
//!   slice keeps the whole backing allocation alive — AGENTS.md don't-rule). The
//!   serializer redacts secrets INLINE (sensitive keys, image URIs, over-long
//!   scalars), so no secret persists even in a preview.
//! - The live store enforces three caps under the same lock: the record count
//!   (512), a 30-minute TTL, and a total live summary-byte quota (64 MiB) that
//!   evicts OLDEST BODIES first (sets their body `Arc`s to `None`; the record
//!   stays as a body-free summary).
//! - Snapshots (`snapshot_summaries`) are BODY-FREE (`SnapshotFlowSummary`) — body
//!   retention on historical snapshots recreates a 135 GiB worst case (D5 owns the
//!   ring; D1 just exposes the body-free summary projection).

use crate::http::is_sensitive_payload_key;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Record cap: reuse the monitor's `REQUEST_EVENT_LIMIT` (512) so the dashboard
/// store does not invent a second retention dial (D1 constraint).
const FLOW_CAP: usize = crate::monitor::REQUEST_EVENT_LIMIT;
/// Per-record TTL: reuse the monitor's 30-minute `DEBUG_HISTORY_RETENTION_MS`.
const FLOW_TTL_MS: u128 = crate::monitor::DEBUG_HISTORY_RETENTION_MS;
/// Default total live summary-byte quota (bodies + retained scalar strings). When
/// exceeded, the OLDEST bodies are evicted first (body `Arc` → `None`) until the
/// store is back under quota; the record survives as a body-free summary.
const DEFAULT_SUMMARY_QUOTA_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on a single captured body (inbound/normalized/upstream). The streaming
/// serializer stops writing once it has emitted this many bytes, so peak retained
/// body memory is O(CAP) regardless of the 256 MiB inbound buffer size.
const BODY_CAP: usize = 128 * 1024;
/// Per-retained-scalar-string cap inside a captured body. A single huge JSON string
/// (e.g. a base64 blob) is truncated to this many bytes before it is retained.
const SCALAR_CAP: usize = 4 * 1024;
/// Recursion depth limit for the streaming JSON redactor — bounds the call stack on
/// adversarially nested input; deeper input falls back to the best-effort path.
const MAX_JSON_DEPTH: usize = 128;

/// CAS claim state: the flow is open and unclaimed by a telemetry writer (D3 L0).
pub const CLAIM_OPEN_L0: u8 = 0;
/// CAS claim state: a telemetry writer holds the claim (D3 L1).
pub const CLAIM_CLAIMED_L1: u8 = 1;
/// CAS claim state: the flow is finalized; no further telemetry writes (D3).
pub const CLAIM_FINALIZED: u8 = 2;

/// Lifecycle status of a flow. `Open` at creation; D3 moves it to a terminal
/// state. Serializes snake_case for the dashboard REST/WS surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    Open,
    Completed,
    Failed,
    Cancelled,
}

/// Token usage attached to a flow once the upstream response reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FlowUsage {
    pub prompt: i64,
    pub completion: i64,
    pub total: i64,
    pub cached: i64,
    pub reasoning: i64,
}

/// Request-extension newtype carrying the `api_call_id` minted by `log_api_call`
/// from the HTTP boundary down to the inference handlers + engine, so the engine
/// can `link(response_id, api_call_id)` without re-deriving the id.
#[derive(Debug, Clone)]
pub struct ApiCallId(pub String);

/// The authoritative live record for one inference flow. NOT `Serialize`: it holds
/// an `Arc<AtomicU8>` (the D3 claim) and an `Instant`, and carries the raw capped
/// body `Arc<[u8]>`s; the dashboard surface serializes the body-free
/// [`SnapshotFlowSummary`] projection instead. Cloned copy-on-write on every
/// mutation; the `claim` `Arc` is shared across copies so the atomic identity is
/// stable for D3.
#[derive(Debug, Clone)]
pub struct FlowRecord {
    /// D3 telemetry-guard CAS cell (`CLAIM_OPEN_L0` at creation). Shared across COW
    /// copies — D1 only allocates it; D3 owns the transitions.
    pub claim: Arc<AtomicU8>,
    pub api_call_id: String,
    pub response_id: Option<String>,
    pub method: String,
    pub uri: String,
    /// Header name/value pairs with sensitive VALUES already redacted.
    pub headers: Vec<(String, String)>,
    /// Capped + redacted inbound request body. `None` once evicted by the
    /// summary-byte quota (the record then survives as a body-free summary).
    pub inbound_body: Option<Arc<[u8]>>,
    /// Capped + redacted canonical/normalized body (set by D2).
    pub normalized: Option<Arc<[u8]>>,
    /// Capped + redacted upstream chat body (set by D2).
    pub upstream_body: Option<Arc<[u8]>>,
    pub model_requested: Option<String>,
    pub model_served: Option<String>,
    pub upstream_target: Option<String>,
    pub usage: Option<FlowUsage>,
    pub status: FlowStatus,
    pub started_at: Instant,
    pub started_ms: u128,
    pub finished_ms: Option<u128>,
    pub elapsed_ms: Option<u128>,
    pub terminal_reason: Option<String>,
}

impl FlowRecord {
    /// Bytes this record contributes to the live summary-byte quota: the three
    /// captured bodies plus the retained (already-capped) header strings. Used to
    /// maintain `live_summary_bytes` incrementally and to drive body eviction.
    fn summary_bytes(&self) -> usize {
        let body = |b: &Option<Arc<[u8]>>| b.as_ref().map(|b| b.len()).unwrap_or(0);
        let headers: usize = self
            .headers
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum();
        body(&self.inbound_body) + body(&self.normalized) + body(&self.upstream_body) + headers
    }

    /// Total bytes held by the three body `Arc`s only (the eviction target).
    fn body_bytes(&self) -> usize {
        let body = |b: &Option<Arc<[u8]>>| b.as_ref().map(|b| b.len()).unwrap_or(0);
        body(&self.inbound_body) + body(&self.normalized) + body(&self.upstream_body)
    }

    /// Whether the record still retains any body (eviction candidate).
    fn has_body(&self) -> bool {
        self.inbound_body.is_some() || self.normalized.is_some() || self.upstream_body.is_some()
    }
}

/// Body-free projection of a [`FlowRecord`] for the dashboard REST/WS surface and
/// the D5 snapshot ring. Carries every field EXCEPT the three body `Arc<[u8]>`s and
/// the non-serializable `claim`/`started_at` — body retention on snapshots is
/// forbidden (135 GiB worst case; AGENTS.md don't-rule).
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotFlowSummary {
    pub api_call_id: String,
    pub response_id: Option<String>,
    pub method: String,
    pub uri: String,
    pub model_requested: Option<String>,
    pub model_served: Option<String>,
    pub upstream_target: Option<String>,
    pub usage: Option<FlowUsage>,
    pub status: FlowStatus,
    pub started_ms: u128,
    pub finished_ms: Option<u128>,
    pub elapsed_ms: Option<u128>,
    pub terminal_reason: Option<String>,
}

impl SnapshotFlowSummary {
    fn from_record(record: &FlowRecord) -> Self {
        Self {
            api_call_id: record.api_call_id.clone(),
            response_id: record.response_id.clone(),
            method: record.method.clone(),
            uri: record.uri.clone(),
            model_requested: record.model_requested.clone(),
            model_served: record.model_served.clone(),
            upstream_target: record.upstream_target.clone(),
            usage: record.usage,
            status: record.status,
            started_ms: record.started_ms,
            finished_ms: record.finished_ms,
            elapsed_ms: record.elapsed_ms,
            terminal_reason: record.terminal_reason.clone(),
        }
    }
}

/// Interior state of the store, guarded by the single `Mutex`. `by_id` and `order`
/// are ALWAYS mutated together (no exterior LRU). `link_index` maps a
/// `response_id` to its owning `api_call_id` so `detail` can join by either id.
/// `live_summary_bytes` is the running total used to enforce the byte quota.
#[derive(Debug, Default)]
struct DashboardFlowState {
    by_id: HashMap<String, Arc<FlowRecord>>,
    order: VecDeque<String>,
    link_index: HashMap<String, String>,
    live_summary_bytes: usize,
}

/// Authoritative store of per-flow records + the capture seam. Mirrors the
/// `MonitorHub::new()/disabled()` zero-overhead pattern: when `disabled()` every
/// operation early-returns and `is_enabled()` is `false`. `Clone` (the inner state
/// is behind `Arc<Mutex<_>>`, exactly like `MonitorHub`) so it threads into the
/// `#[derive(Clone)] Gateway` like the monitor does.
#[derive(Clone)]
pub struct DashboardFlowStore {
    enabled: bool,
    state: Arc<Mutex<DashboardFlowState>>,
    summary_quota_bytes: usize,
}

impl DashboardFlowStore {
    /// Enabled store (debug UI on). Uses the default 64 MiB summary-byte quota.
    pub fn new() -> Self {
        Self {
            enabled: true,
            state: Arc::new(Mutex::new(DashboardFlowState::default())),
            summary_quota_bytes: DEFAULT_SUMMARY_QUOTA_BYTES,
        }
    }

    /// No-op store (debug UI off). Every operation early-returns; production keeps
    /// zero overhead, mirroring `MonitorHub::disabled()`.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            state: Arc::new(Mutex::new(DashboardFlowState::default())),
            summary_quota_bytes: DEFAULT_SUMMARY_QUOTA_BYTES,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DashboardFlowState> {
        self.state
            .lock()
            .expect("dashboard flow state lock poisoned")
    }

    /// Open a new flow record keyed by `api_call_id`. No-op when disabled. The
    /// caller passes ALREADY-redacted headers and an ALREADY-capped inbound body
    /// (`capture_body`), so no secret-bearing or oversized data enters the store.
    pub fn open(
        &self,
        api_call_id: String,
        method: String,
        uri: String,
        headers_redacted: Vec<(String, String)>,
        inbound_body: Option<Arc<[u8]>>,
    ) {
        if !self.enabled {
            return;
        }
        let now = now_ms();
        let record = FlowRecord {
            claim: Arc::new(AtomicU8::new(CLAIM_OPEN_L0)),
            api_call_id: api_call_id.clone(),
            response_id: None,
            method,
            uri,
            headers: headers_redacted,
            inbound_body,
            normalized: None,
            upstream_body: None,
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            status: FlowStatus::Open,
            started_at: Instant::now(),
            started_ms: now,
            finished_ms: None,
            elapsed_ms: None,
            terminal_reason: None,
        };
        let mut state = self.lock();
        state.prune_expired(now);
        state.insert(api_call_id, Arc::new(record));
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Record the `response_id → api_call_id` mapping and stamp the flow's
    /// `response_id`. Fires once per flow (the engine calls it exactly once after
    /// minting `resp_{uuid}`). No-op when disabled or when the flow is unknown.
    pub fn link(&self, response_id: String, api_call_id: String) {
        if !self.enabled {
            return;
        }
        let mut state = self.lock();
        // Record the join index even if the record was already evicted, so a late
        // detail-by-response_id still resolves while the record lives.
        state
            .link_index
            .insert(response_id.clone(), api_call_id.clone());
        state.update(&api_call_id, |record| {
            if record.response_id.is_none() {
                record.response_id = Some(response_id.clone());
            }
        });
    }

    /// Attach the upstream target + served/requested model identity (D2).
    pub fn set_upstream(
        &self,
        api_call_id: &str,
        upstream_target: Option<String>,
        model_served: Option<String>,
        upstream_body: Option<Arc<[u8]>>,
    ) {
        if !self.enabled {
            return;
        }
        let mut state = self.lock();
        state.update(api_call_id, |record| {
            if upstream_target.is_some() {
                record.upstream_target = upstream_target.clone();
            }
            if model_served.is_some() {
                record.model_served = model_served.clone();
            }
            if upstream_body.is_some() {
                record.upstream_body = upstream_body.clone();
            }
        });
        state.recompute_summary_bytes();
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Attach the canonical/normalized body + requested model (D2).
    pub fn set_normalized(
        &self,
        api_call_id: &str,
        model_requested: Option<String>,
        normalized: Option<Arc<[u8]>>,
    ) {
        if !self.enabled {
            return;
        }
        let mut state = self.lock();
        state.update(api_call_id, |record| {
            if model_requested.is_some() {
                record.model_requested = model_requested.clone();
            }
            if normalized.is_some() {
                record.normalized = normalized.clone();
            }
        });
        state.recompute_summary_bytes();
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Mark a flow terminal (D3). Stamps `status`, `finished_ms`, `elapsed_ms`
    /// (from the record's `started_at`), and the terminal reason.
    pub fn finalize(&self, api_call_id: &str, status: FlowStatus, terminal_reason: Option<String>) {
        if !self.enabled {
            return;
        }
        let now = now_ms();
        let mut state = self.lock();
        state.update(api_call_id, |record| {
            record.status = status;
            record.finished_ms = Some(now);
            record.elapsed_ms = Some(record.started_at.elapsed().as_millis());
            if terminal_reason.is_some() {
                record.terminal_reason = terminal_reason.clone();
            }
        });
    }

    /// Attach token usage (D3).
    pub fn record_usage(&self, api_call_id: &str, usage: FlowUsage) {
        if !self.enabled {
            return;
        }
        let mut state = self.lock();
        state.update(api_call_id, |record| {
            record.usage = Some(usage);
        });
    }

    /// Live records, newest-first. Empty when disabled.
    pub fn list(&self) -> Vec<Arc<FlowRecord>> {
        if !self.enabled {
            return Vec::new();
        }
        let state = self.lock();
        state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.by_id.get(id).cloned())
            .collect()
    }

    /// Resolve a single record by `api_call_id` OR `response_id` (via the link
    /// index). `None` when disabled or unknown.
    pub fn detail(&self, id: &str) -> Option<Arc<FlowRecord>> {
        if !self.enabled {
            return None;
        }
        let state = self.lock();
        if let Some(record) = state.by_id.get(id) {
            return Some(Arc::clone(record));
        }
        // Join by response_id → api_call_id.
        let api_call_id = state.link_index.get(id)?;
        state.by_id.get(api_call_id).cloned()
    }

    /// Body-free snapshot summaries, newest-first. Empty when disabled.
    pub fn snapshot_summaries(&self) -> Vec<SnapshotFlowSummary> {
        if !self.enabled {
            return Vec::new();
        }
        let state = self.lock();
        state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.by_id.get(id))
            .map(|record| SnapshotFlowSummary::from_record(record))
            .collect()
    }
}

impl Default for DashboardFlowStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardFlowState {
    /// Insert (or replace) a record, keeping `by_id` + `order` in lockstep and the
    /// `live_summary_bytes` total correct.
    fn insert(&mut self, api_call_id: String, record: Arc<FlowRecord>) {
        if let Some(previous) = self.by_id.remove(&api_call_id) {
            self.live_summary_bytes = self
                .live_summary_bytes
                .saturating_sub(previous.summary_bytes());
            self.order.retain(|id| id != &api_call_id);
        }
        self.live_summary_bytes = self
            .live_summary_bytes
            .saturating_add(record.summary_bytes());
        self.order.push_back(api_call_id.clone());
        self.by_id.insert(api_call_id, record);
    }

    /// Copy-on-write mutate the record for `api_call_id`: clone the inner record
    /// (the `claim` `Arc` is shared, NOT deep-copied, so D3's atomic identity
    /// persists), apply `edit`, and swap the `Arc` back in. Adjusts
    /// `live_summary_bytes` by the byte delta. No-op when the record is absent.
    fn update(&mut self, api_call_id: &str, edit: impl FnOnce(&mut FlowRecord)) {
        let Some(existing) = self.by_id.get(api_call_id) else {
            return;
        };
        let before = existing.summary_bytes();
        let mut next = (**existing).clone();
        edit(&mut next);
        let after = next.summary_bytes();
        self.live_summary_bytes = self
            .live_summary_bytes
            .saturating_sub(before)
            .saturating_add(after);
        self.by_id.insert(api_call_id.to_string(), Arc::new(next));
    }

    /// Recompute the running summary-byte total from scratch. Called after a
    /// body-attaching mutation so the quota accounting cannot drift over the COW
    /// lifecycle.
    fn recompute_summary_bytes(&mut self) {
        self.live_summary_bytes = self
            .by_id
            .values()
            .map(|record| record.summary_bytes())
            .sum();
    }

    /// Drop records whose age exceeds the TTL. Returns the removed ids' summary
    /// bytes to the running total.
    fn prune_expired(&mut self, now: u128) {
        let cutoff = now.saturating_sub(FLOW_TTL_MS);
        let expired: Vec<String> = self
            .order
            .iter()
            .filter(|id| {
                self.by_id
                    .get(*id)
                    .is_some_and(|record| record.started_ms < cutoff)
            })
            .cloned()
            .collect();
        for id in expired {
            self.remove(&id);
        }
    }

    /// Enforce the record-count cap (evict oldest WHOLE records) then the
    /// summary-byte quota (evict OLDEST BODIES first — set body `Arc`s to `None`;
    /// the record survives as a body-free summary).
    fn enforce_caps(&mut self, quota_bytes: usize) {
        while self.order.len() > FLOW_CAP {
            let Some(oldest) = self.order.front().cloned() else {
                break;
            };
            self.remove(&oldest);
        }
        self.enforce_summary_quota(quota_bytes);
    }

    /// Walk records oldest-first dropping their bodies until the running
    /// summary-byte total is back under quota. Only bodies are shed; the record
    /// (and its body-free summary) stays.
    fn enforce_summary_quota(&mut self, quota_bytes: usize) {
        if self.live_summary_bytes <= quota_bytes {
            return;
        }
        let ids: Vec<String> = self.order.iter().cloned().collect();
        for id in ids {
            if self.live_summary_bytes <= quota_bytes {
                break;
            }
            let Some(existing) = self.by_id.get(&id) else {
                continue;
            };
            if !existing.has_body() {
                continue;
            }
            let freed = existing.body_bytes();
            let mut next = (**existing).clone();
            next.inbound_body = None;
            next.normalized = None;
            next.upstream_body = None;
            self.by_id.insert(id, Arc::new(next));
            self.live_summary_bytes = self.live_summary_bytes.saturating_sub(freed);
        }
    }

    /// Remove a record (and its link-index back-references), keeping all three
    /// structures + the byte total consistent.
    fn remove(&mut self, api_call_id: &str) {
        if let Some(record) = self.by_id.remove(api_call_id) {
            self.live_summary_bytes = self
                .live_summary_bytes
                .saturating_sub(record.summary_bytes());
            if let Some(response_id) = &record.response_id {
                self.link_index.remove(response_id);
            }
        }
        self.order.retain(|id| id != api_call_id);
        // Drop any dangling link-index entries that pointed at this id (e.g. a
        // link recorded before the record was evicted).
        self.link_index.retain(|_, owner| owner != api_call_id);
    }
}

// ---------------------------------------------------------------------------
// Capped + redacting STREAMING body serializer.
//
// `capture_body` turns a raw request/response byte slice into an owned
// `Arc<[u8]>` of at most BODY_CAP bytes, redacting secrets INLINE:
// - sensitive object keys (`api_key`/`authorization`/… via the shared
//   `is_sensitive_payload_key`) → the entire value becomes `"[redacted]"` and the
//   real value is parse-skipped (never written);
// - string values → capped to SCALAR_CAP and run through `redact_image_uris`;
// - everything else copied through.
//
// The common path is a SINGLE forward-pass recursive-descent walk over the byte
// slice into a hard-capped writer — it never builds a full `serde_json::Value`
// (that would be O(body)), so peak allocation is O(CAP) even for a 10 MiB body.
// Only the rare fallback (malformed/non-UTF8/too-deep) materializes a `Value`.
// ---------------------------------------------------------------------------

/// Capped + redacting capture of a request/response body. Returns an owned
/// `Arc<[u8]>` of at most [`BODY_CAP`] bytes with secrets redacted inline. Never
/// retains a slice of `raw` (copies into a fresh buffer).
pub fn capture_body(raw: &[u8]) -> Arc<[u8]> {
    // Common path: a single streaming pass over the bytes into a hard-capped
    // writer. `CappedWriter` pre-reserves only `raw.len().min(BODY_CAP)` and stops
    // writing at BODY_CAP, so peak allocation is O(CAP), not O(body).
    let mut writer = CappedWriter::new(raw.len());
    if let Ok(text) = std::str::from_utf8(raw) {
        let mut parser = JsonRedactor::new(text);
        parser.skip_ws();
        if parser.redact_value(&mut writer, 0, false) && parser.at_trailing_end() {
            return writer.into_arc();
        }
    }
    // Fallback (RARE): malformed / non-JSON / non-UTF8 / too-deep. Best-effort
    // redaction via the existing `Value`-based path, then truncate to BODY_CAP. The
    // 10 MiB cap test must hit the streaming path above, so this stays the rare
    // case.
    capture_body_fallback(raw)
}

/// Best-effort fallback redaction for bodies the streaming pass could not handle.
fn capture_body_fallback(raw: &[u8]) -> Arc<[u8]> {
    if let Ok(mut value) = serde_json::from_slice::<Value>(raw) {
        redact_value_secrets(&mut value);
        crate::redaction::redact_image_uris_in_value(&mut value);
        if let Ok(serialized) = serde_json::to_vec(&value) {
            return truncate_to_cap(serialized);
        }
    }
    // Not even JSON: redact image URIs over a BODY_CAP-bounded lossy prefix.
    let prefix_len = raw.len().min(BODY_CAP);
    let lossy = String::from_utf8_lossy(&raw[..prefix_len]);
    let redacted = crate::redaction::redact_image_uris(&lossy);
    truncate_to_cap(redacted.into_bytes())
}

/// Value-tree secret redactor mirroring `http::redact_payload_secrets` (sensitive
/// key → `"[redacted]"`), used only on the fallback path.
fn redact_value_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if is_sensitive_payload_key(key) {
                    *inner = Value::String("[redacted]".to_string());
                } else {
                    redact_value_secrets(inner);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value_secrets(item);
            }
        }
        _ => {}
    }
}

/// Truncate an owned byte buffer to [`BODY_CAP`] on a UTF-8 char boundary (so a
/// best-effort preview never splits a multibyte sequence) and freeze it.
fn truncate_to_cap(mut bytes: Vec<u8>) -> Arc<[u8]> {
    if bytes.len() > BODY_CAP {
        let mut end = BODY_CAP;
        while end > 0 && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        bytes.truncate(end);
    }
    Arc::from(bytes.into_boxed_slice())
}

/// A `Vec<u8>` that accepts writes only until it reaches [`BODY_CAP`]; once full it
/// silently drops further writes and records `full = true`. Pre-reserves only
/// `min(hint, BODY_CAP)` so peak allocation is bounded by the cap.
struct CappedWriter {
    buf: Vec<u8>,
    full: bool,
}

impl CappedWriter {
    fn new(size_hint: usize) -> Self {
        Self {
            buf: Vec::with_capacity(size_hint.min(BODY_CAP)),
            full: false,
        }
    }

    /// Append `bytes`, stopping at the cap. Returns `false` once the writer is
    /// full so the caller can stop producing output.
    fn write(&mut self, bytes: &[u8]) -> bool {
        if self.full {
            return false;
        }
        let remaining = BODY_CAP - self.buf.len();
        if bytes.len() <= remaining {
            self.buf.extend_from_slice(bytes);
            if self.buf.len() == BODY_CAP {
                self.full = true;
            }
            true
        } else {
            self.buf.extend_from_slice(&bytes[..remaining]);
            self.full = true;
            false
        }
    }

    fn write_byte(&mut self, byte: u8) -> bool {
        self.write(&[byte])
    }

    fn into_arc(self) -> Arc<[u8]> {
        Arc::from(self.buf.into_boxed_slice())
    }
}

/// Forward-pass recursive-descent JSON redactor over a `&str`. Walks the value at
/// the cursor writing a redacted copy into a [`CappedWriter`], never building a
/// `Value`. Returns `false` on malformed input or depth overflow so the caller can
/// fall back.
struct JsonRedactor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonRedactor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// After parsing the top-level value, only whitespace may remain.
    fn at_trailing_end(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.bytes.len()
    }

    /// Redact one JSON value at the cursor into `out`. When `sensitive` is set the
    /// value belongs to a sensitive key, so it is replaced wholesale with
    /// `"[redacted]"` and parse-skipped. Returns `false` on malformed input or
    /// when the depth limit is exceeded.
    fn redact_value(&mut self, out: &mut CappedWriter, depth: usize, sensitive: bool) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        self.skip_ws();
        let Some(byte) = self.peek() else {
            return false;
        };
        if sensitive {
            // Replace the whole value with the redaction marker, and parse-skip the
            // real value so the cursor advances correctly.
            out.write(b"\"[redacted]\"");
            return self.skip_value(depth);
        }
        match byte {
            b'{' => self.redact_object(out, depth),
            b'[' => self.redact_array(out, depth),
            b'"' => self.redact_string(out),
            _ => self.copy_scalar(out),
        }
    }

    fn redact_object(&mut self, out: &mut CappedWriter, depth: usize) -> bool {
        // consume '{'
        self.pos += 1;
        out.write_byte(b'{');
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            out.write_byte(b'}');
            return true;
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return false;
            }
            // Capture the key text (unescaped lossily) to test sensitivity, while
            // writing the key string through verbatim-but-capped.
            let key = match self.read_key_string(out) {
                Some(key) => key,
                None => return false,
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                return false;
            }
            self.pos += 1;
            out.write_byte(b':');
            let sensitive = is_sensitive_payload_key(&key);
            if !self.redact_value(out, depth + 1, sensitive) {
                return false;
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    out.write_byte(b',');
                }
                Some(b'}') => {
                    self.pos += 1;
                    out.write_byte(b'}');
                    return true;
                }
                _ => return false,
            }
        }
    }

    fn redact_array(&mut self, out: &mut CappedWriter, depth: usize) -> bool {
        // consume '['
        self.pos += 1;
        out.write_byte(b'[');
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            out.write_byte(b']');
            return true;
        }
        loop {
            if !self.redact_value(out, depth + 1, false) {
                return false;
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    out.write_byte(b',');
                }
                Some(b']') => {
                    self.pos += 1;
                    out.write_byte(b']');
                    return true;
                }
                _ => return false,
            }
        }
    }

    /// Read a JSON string key, writing it through (capped) AND returning its
    /// decoded text for the sensitivity test. The cursor lands just past the
    /// closing quote. Keys are short in practice; the token slice is zero-copy and
    /// only the small decoded key allocates.
    fn read_key_string(&mut self, out: &mut CappedWriter) -> Option<String> {
        let range = self.scan_string_raw()?;
        let raw = self.token_str(range);
        out.write(raw.as_bytes());
        Some(decode_json_string(raw))
    }

    /// Redact a JSON string VALUE: cap its inner content to SCALAR_CAP and run
    /// `redact_image_uris` over it, then re-emit a quoted JSON string. The cursor
    /// lands just past the closing quote. The token is a zero-copy slice — for a
    /// 10 MiB value only the SCALAR_CAP-byte capped prefix is ever allocated.
    fn redact_string(&mut self, out: &mut CappedWriter) -> bool {
        let Some(range) = self.scan_string_raw() else {
            return false;
        };
        let raw = self.token_str(range);
        // `raw` includes the surrounding quotes. Strip them, cap the inner JSON
        // text on a boundary that does not split a `\uXXXX`/`\x` escape, redact
        // image URIs, and re-wrap. `cap_json_string_inner` slices BEFORE allocating,
        // so the huge inner string is never copied in full.
        let inner = &raw[1..raw.len() - 1];
        let capped = cap_json_string_inner(inner, SCALAR_CAP);
        let redacted = crate::redaction::redact_image_uris(&capped);
        out.write_byte(b'"');
        out.write(redacted.as_bytes());
        out.write_byte(b'"');
        true
    }

    /// Copy a JSON scalar (number / `true` / `false` / `null`) through verbatim.
    fn copy_scalar(&mut self, out: &mut CappedWriter) -> bool {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
                _ => self.pos += 1,
            }
        }
        if self.pos == start {
            return false;
        }
        out.write(&self.bytes[start..self.pos]);
        true
    }

    /// Parse-skip a JSON value WITHOUT emitting it (used for a sensitive value
    /// whose redaction marker was already written). Returns `false` on malformed
    /// input or depth overflow.
    fn skip_value(&mut self, depth: usize) -> bool {
        if depth > MAX_JSON_DEPTH {
            return false;
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return true;
                }
                loop {
                    self.skip_ws();
                    if self.scan_string_raw().is_none() {
                        return false;
                    }
                    self.skip_ws();
                    if self.peek() != Some(b':') {
                        return false;
                    }
                    self.pos += 1;
                    if !self.skip_value(depth + 1) {
                        return false;
                    }
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return true;
                        }
                        _ => return false,
                    }
                }
            }
            Some(b'[') => {
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return true;
                }
                loop {
                    if !self.skip_value(depth + 1) {
                        return false;
                    }
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return true;
                        }
                        _ => return false,
                    }
                }
            }
            Some(b'"') => self.scan_string_raw().is_some(),
            Some(_) => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    match self.bytes[self.pos] {
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
                        _ => self.pos += 1,
                    }
                }
                self.pos != start
            }
            None => false,
        }
    }

    /// Scan a JSON string token INCLUDING the surrounding quotes, honoring `\"`
    /// and `\\` escapes. Returns the `start..end` BYTE RANGE of the token (a
    /// zero-copy slice into `self.bytes`) or `None` if the string is unterminated.
    /// The cursor lands just past the closing quote. Returning a range — not an
    /// owned `String` — is what keeps `capture_body` O(CAP): a 10 MiB string VALUE
    /// is never copied in full; the caller slices and only the CAPPED prefix is
    /// allocated.
    fn scan_string_raw(&mut self) -> Option<(usize, usize)> {
        if self.peek() != Some(b'"') {
            return None;
        }
        let start = self.pos;
        self.pos += 1; // opening quote
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b'\\' => {
                    // Skip the escape and the escaped byte (UTF-8 continuation
                    // bytes after `\u` are ASCII hex, so byte-stepping is safe).
                    self.pos += 2;
                }
                b'"' => {
                    self.pos += 1;
                    return Some((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        None
    }

    /// The validated `&str` slice for a scanned string-token range. The bytes came
    /// from a `&str` and we split only on ASCII quote/backslash boundaries, so the
    /// slice is valid UTF-8 (the `unwrap_or` keeps the forward pass total).
    fn token_str(&self, range: (usize, usize)) -> &'a str {
        std::str::from_utf8(&self.bytes[range.0..range.1]).unwrap_or("")
    }
}

/// Cap the INNER text of a JSON string to `cap` bytes without splitting a UTF-8
/// sequence or a trailing JSON escape (`\`): if the cut would land right after a
/// lone backslash, back off one byte so the re-wrapped string stays valid.
fn cap_json_string_inner(inner: &str, cap: usize) -> String {
    if inner.len() <= cap {
        return inner.to_string();
    }
    let bytes = inner.as_bytes();
    let mut end = cap;
    // Back off to a UTF-8 char boundary.
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    // Avoid ending on a lone (unfinished) backslash escape: count the run of
    // trailing backslashes before `end`; if odd, the last one opens an escape, so
    // drop it.
    let mut backslashes = 0;
    let mut i = end;
    while i > 0 && bytes[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    if backslashes % 2 == 1 {
        end -= 1;
    }
    inner[..end].to_string()
}

/// Decode a raw JSON string token (quotes + escapes) into its text value, enough
/// to test key sensitivity. Only the escapes relevant to object keys are handled;
/// an unrecognized escape is passed through. Lossy by design (keys are ASCII in
/// practice).
fn decode_json_string(raw: &str) -> String {
    // `raw` is a scanned string token including its surrounding quotes.
    let inner = if raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        ""
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{0008}'),
                Some('f') => out.push('\u{000C}'),
                Some('u') => {
                    // Best-effort: consume 4 hex digits, decode a BMP scalar.
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16)
                        && let Some(decoded) = char::from_u32(code)
                    {
                        out.push(decoded);
                    }
                }
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Redact header VALUES whose normalized name is sensitive (reusing
/// `is_sensitive_payload_key`) to `"[redacted]"`; cap every other value to
/// [`SCALAR_CAP`] bytes (on a char boundary). Returns owned name/value pairs ready
/// for `FlowRecord.headers`. The header NAME is preserved (it is not a secret) so
/// the dashboard can show WHICH auth header was present without its value.
pub fn redact_headers(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_str().to_string();
            if is_sensitive_payload_key(&name) {
                return (name, "[redacted]".to_string());
            }
            let raw = value.to_str().unwrap_or("<non-utf8>");
            let capped = if raw.len() > SCALAR_CAP {
                let bytes = raw.as_bytes();
                let mut end = SCALAR_CAP;
                while end > 0 && (bytes[end] & 0xC0) == 0x80 {
                    end -= 1;
                }
                raw[..end].to_string()
            } else {
                raw.to_string()
            };
            (name, capped)
        })
        .collect()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::http::HeaderName;
    use axum::http::HeaderValue;
    fn arc(bytes: &[u8]) -> Arc<[u8]> {
        Arc::from(bytes.to_vec().into_boxed_slice())
    }

    #[test]
    fn disabled_store_is_a_no_op() {
        let store = DashboardFlowStore::disabled();
        assert!(!store.is_enabled());
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            Vec::new(),
            Some(arc(b"{}")),
        );
        store.link("resp_1".to_string(), "api_1".to_string());
        store.record_usage("api_1", FlowUsage::default());
        store.finalize("api_1", FlowStatus::Completed, None);
        assert!(store.list().is_empty(), "disabled store records nothing");
        assert!(store.detail("api_1").is_none());
        assert!(store.snapshot_summaries().is_empty());
    }

    #[test]
    fn open_then_list_and_detail_round_trip() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            vec![("content-type".to_string(), "application/json".to_string())],
            Some(arc(b"{\"model\":\"m\"}")),
        );
        let records = store.list();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].api_call_id, "api_1");
        assert_eq!(records[0].status, FlowStatus::Open);
        assert_eq!(
            records[0].claim.load(std::sync::atomic::Ordering::SeqCst),
            CLAIM_OPEN_L0,
            "claim allocated OpenL0"
        );
        // detail by api_call_id
        assert!(store.detail("api_1").is_some());
        // unknown id
        assert!(store.detail("nope").is_none());
    }

    #[test]
    fn list_is_newest_first() {
        let store = DashboardFlowStore::new();
        for i in 0..3 {
            store.open(
                format!("api_{i}"),
                "POST".to_string(),
                "/v1/responses".to_string(),
                Vec::new(),
                None,
            );
        }
        let ids: Vec<String> = store
            .list()
            .iter()
            .map(|record| record.api_call_id.clone())
            .collect();
        assert_eq!(ids, vec!["api_2", "api_1", "api_0"]);
    }

    #[test]
    fn link_fires_once_and_detail_joins_by_either_id() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            Vec::new(),
            None,
        );
        // Pre-link: detail by api_call_id resolves; by response_id does not yet.
        assert!(store.detail("api_1").is_some());
        assert!(store.detail("resp_1").is_none());

        store.link("resp_1".to_string(), "api_1".to_string());
        // A second link must NOT overwrite the response_id.
        store.link("resp_other".to_string(), "api_1".to_string());

        let record = store.detail("api_1").expect("record");
        assert_eq!(
            record.response_id.as_deref(),
            Some("resp_1"),
            "link fires once; the first response_id wins"
        );
        // detail now joins by response_id too.
        let by_resp = store.detail("resp_1").expect("join by response_id");
        assert_eq!(by_resp.api_call_id, "api_1");
    }

    #[test]
    fn concurrent_flows_link_correctly() {
        let store = Arc::new(DashboardFlowStore::new());
        let mut handles = Vec::new();
        for i in 0..16 {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                let api = format!("api_{i}");
                let resp = format!("resp_{i}");
                store.open(
                    api.clone(),
                    "POST".to_string(),
                    "/v1/responses".to_string(),
                    Vec::new(),
                    None,
                );
                store.link(resp, api);
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }
        for i in 0..16 {
            let record = store
                .detail(&format!("resp_{i}"))
                .expect("each response_id joins to its own flow");
            assert_eq!(record.api_call_id, format!("api_{i}"));
            assert_eq!(
                record.response_id.as_deref(),
                Some(format!("resp_{i}").as_str())
            );
        }
    }

    #[test]
    fn finalize_stamps_terminal_fields() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            Vec::new(),
            None,
        );
        store.record_usage(
            "api_1",
            FlowUsage {
                prompt: 10,
                completion: 5,
                total: 15,
                cached: 0,
                reasoning: 2,
            },
        );
        store.finalize(
            "api_1",
            FlowStatus::Completed,
            Some("response.completed".to_string()),
        );
        let record = store.detail("api_1").expect("record");
        assert_eq!(record.status, FlowStatus::Completed);
        assert!(record.finished_ms.is_some());
        assert!(record.elapsed_ms.is_some());
        assert_eq!(
            record.terminal_reason.as_deref(),
            Some("response.completed")
        );
        assert_eq!(record.usage.expect("usage").total, 15);
    }

    #[test]
    fn claim_arc_is_shared_across_cow_mutations() {
        // D3 relies on the claim atomic identity surviving COW updates. Capture the
        // Arc, mutate via finalize, and assert the SAME allocation is observed.
        let store = DashboardFlowStore::new();
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            Vec::new(),
            None,
        );
        let before = store.detail("api_1").expect("record").claim.clone();
        // A mutation that goes through the COW path.
        store.set_normalized("api_1", Some("model".to_string()), Some(arc(b"{}")));
        let after = store.detail("api_1").expect("record").claim.clone();
        assert!(
            Arc::ptr_eq(&before, &after),
            "claim Arc identity must persist across COW updates for D3"
        );
        // Writing through the original handle is visible via the new one.
        before.store(CLAIM_CLAIMED_L1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            after.load(std::sync::atomic::Ordering::SeqCst),
            CLAIM_CLAIMED_L1
        );
    }

    #[test]
    fn summary_quota_evicts_oldest_bodies_keeping_records() {
        // A tiny quota forces body eviction. Use a custom store with a small quota.
        let store = DashboardFlowStore {
            enabled: true,
            state: Arc::new(Mutex::new(DashboardFlowState::default())),
            summary_quota_bytes: 4 * 1024,
        };
        // Each body is ~2 KiB after capture; three of them exceed 4 KiB.
        let body = vec![b'a'; 2048];
        let json = {
            let mut v = Vec::new();
            v.extend_from_slice(b"{\"x\":\"");
            v.extend_from_slice(&body);
            v.extend_from_slice(b"\"}");
            v
        };
        for i in 0..3 {
            store.open(
                format!("api_{i}"),
                "POST".to_string(),
                "/v1/responses".to_string(),
                Vec::new(),
                Some(capture_body(&json)),
            );
        }
        // All three records still present.
        assert_eq!(store.list().len(), 3, "records survive body eviction");
        // The OLDEST body is gone; the record remains a body-free summary.
        let oldest = store.detail("api_0").expect("oldest record present");
        assert!(
            oldest.inbound_body.is_none(),
            "oldest body evicted under quota"
        );
        // The newest still has its body (quota satisfied before reaching it).
        let newest = store.detail("api_2").expect("newest record present");
        assert!(newest.inbound_body.is_some(), "newest body retained");
        // Snapshot summaries are body-free regardless.
        assert_eq!(store.snapshot_summaries().len(), 3);
    }

    #[test]
    fn record_cap_evicts_oldest_whole_records() {
        let store = DashboardFlowStore::new();
        // One past the cap → the oldest whole record is dropped.
        for i in 0..(FLOW_CAP + 1) {
            store.open(
                format!("api_{i}"),
                "POST".to_string(),
                "/v1/responses".to_string(),
                Vec::new(),
                None,
            );
        }
        assert_eq!(store.list().len(), FLOW_CAP);
        assert!(store.detail("api_0").is_none(), "oldest record evicted");
        assert!(store.detail(&format!("api_{FLOW_CAP}")).is_some());
    }

    // -------------------------------------------------------------------
    // Capped + redacting streaming serializer
    // -------------------------------------------------------------------

    #[test]
    fn capture_body_peak_allocation_is_bounded_for_10mib_body() {
        // THE crux acceptance criterion: serializing a 10 MiB body keeps PEAK heap
        // use O(CAP), not O(body) — the streaming pass never materializes the whole
        // body (no full `serde_json::Value`, no `Bytes::copy_from_slice`). Build the
        // 10 MiB input OUTSIDE the armed region so only `capture_body`'s own
        // allocations count. The shared allocator probe records the largest SINGLE
        // allocation `>= threshold` on this thread; we set the threshold ABOVE the
        // legitimate ceiling (output cap + one capped scalar + slack) but FAR below
        // the 10 MiB body, then assert nothing that large was allocated. A
        // body-materializing implementation would allocate a multi-MiB buffer and
        // trip the probe.
        const TEN_MIB: usize = 10 * 1024 * 1024;
        let big = "x".repeat(TEN_MIB);
        let json = format!("{{\"text\":\"{big}\"}}");
        let raw = json.into_bytes();

        // Legitimate ceiling for any single allocation on the streaming path.
        let ceiling = BODY_CAP + SCALAR_CAP + 64 * 1024;
        let (captured, peak) =
            crate::test_alloc_probe::peak_alloc_during(ceiling, || capture_body(&raw));

        assert!(
            captured.len() <= BODY_CAP,
            "captured body {} exceeds BODY_CAP {}",
            captured.len(),
            BODY_CAP
        );
        assert_eq!(
            peak, 0,
            "capture_body made a single allocation >= {ceiling} bytes (peak={peak}) for a \
             {TEN_MIB}-byte body — it must stream, not materialize the whole body"
        );
    }

    #[test]
    fn capture_body_caps_output_to_body_cap() {
        // A 10 MiB string body must serialize to <= BODY_CAP (output-size check;
        // the companion test asserts PEAK allocation is bounded).
        let big = "x".repeat(10 * 1024 * 1024);
        let json = format!("{{\"text\":\"{big}\"}}");
        let captured = capture_body(json.as_bytes());
        assert!(
            captured.len() <= BODY_CAP,
            "captured body {} exceeds BODY_CAP {}",
            captured.len(),
            BODY_CAP
        );
    }

    #[test]
    fn capture_body_redacts_sensitive_keys_inline() {
        let json = br#"{"model":"m","api_key":"sk-SECRETLEAK","nested":{"authorization":"Bearer TOKENLEAK"},"keep":"visible"}"#;
        let captured = capture_body(json);
        let text = String::from_utf8_lossy(&captured);
        assert!(!text.contains("SECRETLEAK"), "api_key value redacted");
        assert!(!text.contains("TOKENLEAK"), "nested authorization redacted");
        assert!(text.contains("[redacted]"));
        assert!(text.contains("visible"), "non-sensitive value preserved");
        assert!(text.contains("\"model\":\"m\""), "structure preserved");
    }

    #[test]
    fn capture_body_redacts_image_uris_in_strings() {
        let json =
            br#"{"content":"see data:image/png;base64,IMGLEAK and https://signed.x/i?sig=SIGLEAK"}"#;
        let captured = capture_body(json);
        let text = String::from_utf8_lossy(&captured);
        assert!(!text.contains("IMGLEAK"), "data: payload redacted");
        assert!(!text.contains("SIGLEAK"), "signed-url token redacted");
        assert!(text.contains("<redacted uri>"));
    }

    #[test]
    fn capture_body_caps_individual_scalar_strings() {
        // A single huge string is capped to ~SCALAR_CAP even though the overall
        // body is under BODY_CAP.
        let big = "y".repeat(64 * 1024);
        let json = format!("{{\"blob\":\"{big}\",\"after\":\"tail\"}}");
        let captured = capture_body(json.as_bytes());
        let text = String::from_utf8_lossy(&captured);
        // The blob value is truncated well under its original size.
        assert!(captured.len() < big.len(), "huge scalar truncated");
        // Fields after the capped scalar survive (the stream did not stop).
        assert!(
            text.contains("after"),
            "fields after a capped scalar survive"
        );
    }

    #[test]
    fn capture_body_handles_non_json_via_fallback() {
        let raw = b"this is not json: data:image/png;base64,RAWLEAK end";
        let captured = capture_body(raw);
        let text = String::from_utf8_lossy(&captured);
        assert!(!text.contains("RAWLEAK"), "non-json image uri redacted");
        assert!(text.contains("<redacted uri>"));
        assert!(captured.len() <= BODY_CAP);
    }

    #[test]
    fn capture_body_roundtrips_valid_json_structure() {
        let json = br#"{"a":1,"b":[true,false,null],"c":{"d":"e"}}"#;
        let captured = capture_body(json);
        // The streaming pass produced parseable JSON.
        let value: Value = serde_json::from_slice(&captured).expect("captured body is valid JSON");
        assert_eq!(value["a"], serde_json::json!(1));
        assert_eq!(value["b"], serde_json::json!([true, false, null]));
        assert_eq!(value["c"]["d"], serde_json::json!("e"));
    }

    #[test]
    fn redact_headers_redacts_sensitive_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer SECRETHEADER"),
        );
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("sk-HEADERKEY"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        let redacted = redact_headers(&headers);
        let dumped = format!("{redacted:?}");
        assert!(
            !dumped.contains("SECRETHEADER"),
            "authorization value redacted"
        );
        assert!(!dumped.contains("HEADERKEY"), "x-api-key value redacted");
        assert!(dumped.contains("[redacted]"));
        // The non-sensitive value survives; names are preserved.
        assert!(dumped.contains("application/json"));
        assert!(dumped.contains("authorization"));
    }

    #[test]
    fn secret_persistence_prevention_end_to_end() {
        // An inbound body with sensitive headers AND a top-level api_key field plus
        // an upstream body carrying api_key: NONE of the original secret values may
        // persist in the stored record.
        let store = DashboardFlowStore::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer AUTHSECRET"),
        );
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("XKEYSECRET"),
        );
        headers.insert(
            HeaderName::from_static("api-key"),
            HeaderValue::from_static("APIKEYSECRET"),
        );
        let inbound = br#"{"model":"m","api_key":"BODYKEYSECRET","messages":[]}"#;
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&headers),
            Some(capture_body(inbound)),
        );
        let upstream = br#"{"api_key":"UPSTREAMKEYSECRET","model":"m"}"#;
        store.set_upstream(
            "api_1",
            Some("https://upstream".to_string()),
            Some("m".to_string()),
            Some(capture_body(upstream)),
        );

        let record = store.detail("api_1").expect("record");
        let header_dump = format!("{:?}", record.headers);
        assert!(!header_dump.contains("AUTHSECRET"));
        assert!(!header_dump.contains("XKEYSECRET"));
        assert!(!header_dump.contains("APIKEYSECRET"));

        let inbound_text =
            String::from_utf8_lossy(record.inbound_body.as_ref().expect("inbound")).to_string();
        assert!(
            !inbound_text.contains("BODYKEYSECRET"),
            "inbound api_key redacted"
        );

        let upstream_text =
            String::from_utf8_lossy(record.upstream_body.as_ref().expect("upstream")).to_string();
        assert!(
            !upstream_text.contains("UPSTREAMKEYSECRET"),
            "upstream api_key redacted"
        );
    }

    #[test]
    fn snapshot_summaries_are_body_free() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            Vec::new(),
            Some(capture_body(b"{\"model\":\"m\"}")),
        );
        let summaries = store.snapshot_summaries();
        assert_eq!(summaries.len(), 1);
        // `SnapshotFlowSummary` has no body fields by construction; serializing it
        // must not surface any body bytes.
        let json = serde_json::to_string(&summaries[0]).expect("serialize summary");
        assert!(json.contains("api_call_id"));
        assert!(!json.contains("inbound_body"));
        assert!(
            !json.contains("\"model\":\"m\""),
            "no body content in summary"
        );
    }
}
