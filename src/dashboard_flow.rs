//! D1 — `DashboardFlowStore`: the authoritative per-flow record store and the
//! on-wire capture seam that feeds the transformation inspector + metrics.
//!
//! This module holds STORE LOGIC ONLY. The capped + redacting body serializer and
//! the single sensitive-key authority live in [`crate::redaction`] (D1 R1 #10), so
//! there is exactly one definition of "what is sensitive" and one O(CAP) capture
//! primitive shared with the inbound-trace logger.
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
//! - Bodies are owned, capped `Arc<[u8]>` produced by the redacting STREAMING JSON
//!   serializer in `redaction`: peak serializer memory is O(CAP), not O(body), and
//!   a `Bytes::slice` of the 256 MiB middleware buffer is NEVER retained (a slice
//!   keeps the whole backing allocation alive — AGENTS.md don't-rule). Secrets are
//!   redacted INLINE (sensitive keys, image URIs incl. `\uXXXX`-escaped forms,
//!   over-long scalars + keys), so no secret persists even in a preview.
//! - Every dynamic scalar string the record retains (ids, method, uri, models,
//!   upstream target, terminal reason) is CAPPED at `SCALAR_CAP` and COUNTED in
//!   `live_summary_bytes` (D1 R1 #5).
//! - The live store enforces three caps under the same lock AFTER EVERY mutation
//!   AND on every read: the record count (512), a 30-minute TTL, and a total live
//!   summary-byte quota (64 MiB) that evicts OLDEST BODIES first (sets their body
//!   `Arc`s to `None`; the record stays as a body-free summary).
//! - Snapshots (`snapshot_summaries`) are BODY-FREE (`SnapshotFlowSummary`) — body
//!   retention on historical snapshots recreates a 135 GiB worst case (D5 owns the
//!   ring; D1 just exposes the body-free summary projection).

use serde::Serialize;
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
/// Per-retained-scalar-string cap: bounds every dynamic string the record keeps
/// (body scalars, headers, ids, method, uri, models, target, terminal reason).
const SCALAR_CAP: usize = 4 * 1024;

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

/// A body that has ALREADY been through the capped + redacting capture primitive.
/// The ONLY way to mint one is [`capture_body`] (D1 R2 #2), so a caller (D2/D3)
/// cannot hand the store an unredacted/over-cap `Arc<[u8]>` — the type makes the
/// bypass impossible. Holds an `Arc<[u8]>` ≤ `BODY_CAP` with secrets redacted.
#[derive(Debug, Clone)]
pub struct CapturedBody(Arc<[u8]>);

impl CapturedBody {
    /// The redacted, capped bytes (cheap `Arc` clone).
    fn into_arc(self) -> Arc<[u8]> {
        self.0
    }

    /// Test-only view of the redacted bytes (the field is private so production
    /// callers cannot read raw body bytes back out of the newtype).
    #[cfg(test)]
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Headers that have ALREADY been through the capping + redacting header primitive.
/// The ONLY way to mint one is [`redact_headers`] (D1 R2 #2), so the store can never
/// be handed raw, secret-bearing header pairs.
#[derive(Debug, Clone)]
pub struct CapturedHeaders(Vec<(String, String)>);

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
    /// captured bodies, the retained (already-capped) header strings, AND every
    /// dynamic scalar string the record holds (D1 R1 #5 — none of these were
    /// counted before, so a flood of long model/uri/target strings could blow the
    /// quota silently).
    fn summary_bytes(&self) -> usize {
        let body = |b: &Option<Arc<[u8]>>| b.as_ref().map(|b| b.len()).unwrap_or(0);
        let opt = |s: &Option<String>| s.as_ref().map(|s| s.len()).unwrap_or(0);
        let headers: usize = self
            .headers
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum();
        body(&self.inbound_body)
            + body(&self.normalized)
            + body(&self.upstream_body)
            + headers
            + self.api_call_id.len()
            + opt(&self.response_id)
            + self.method.len()
            + self.uri.len()
            + opt(&self.model_requested)
            + opt(&self.model_served)
            + opt(&self.upstream_target)
            + opt(&self.terminal_reason)
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
#[derive(Clone, Debug)]
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

    /// Open a new flow record keyed by `api_call_id`. No-op when disabled. The body
    /// and headers are [`CapturedBody`]/[`CapturedHeaders`] — minted ONLY by the
    /// capture primitives, so they are provably redacted + capped (D1 R2 #2); every
    /// other dynamic scalar is `cap_scalar`-bounded here before storage.
    pub fn open(
        &self,
        api_call_id: String,
        method: String,
        uri: String,
        headers: CapturedHeaders,
        inbound_body: Option<CapturedBody>,
    ) {
        if !self.enabled {
            return;
        }
        let now = now_ms();
        let record = FlowRecord {
            claim: Arc::new(AtomicU8::new(CLAIM_OPEN_L0)),
            api_call_id: cap_scalar(api_call_id.clone()),
            response_id: None,
            method: cap_scalar(method),
            uri: cap_scalar(uri),
            headers: headers.0,
            inbound_body: inbound_body.map(CapturedBody::into_arc),
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
        state.insert(cap_scalar(api_call_id), Arc::new(record));
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Atomically bind a `response_id` to its flow's `api_call_id` (D1 R1 #8):
    /// fires exactly once per flow (first-link-wins). NO-OP when disabled, when the
    /// record is unknown, or when the record is already linked — so an unknown id
    /// can never leak a `link_index` entry and a repeat link can never alias.
    pub fn link(&self, response_id: String, api_call_id: String) {
        if !self.enabled {
            return;
        }
        let response_id = cap_scalar(response_id);
        let mut state = self.lock();
        state.prune_expired(now_ms());
        let Some(existing) = state.by_id.get(&api_call_id) else {
            // Unknown flow → no index entry, no record. (Avoids the leak where an
            // index entry outlives a never-existent record.)
            return;
        };
        if existing.response_id.is_some() {
            // Already linked: first-link-wins, idempotent.
            return;
        }
        state
            .link_index
            .insert(response_id.clone(), api_call_id.clone());
        state.update(&api_call_id, |record| {
            record.response_id = Some(response_id.clone());
        });
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Attach the upstream target + served model identity + upstream body (D2). The
    /// body is a [`CapturedBody`] (provably redacted + capped); scalars are
    /// `cap_scalar`-bounded. `id` may be the flow's `api_call_id` OR its
    /// `response_id` (the leaf only knows the latter): `update` joins by either via
    /// the link index, mirroring `detail`.
    pub fn set_upstream(
        &self,
        id: &str,
        upstream_target: Option<String>,
        model_served: Option<String>,
        upstream_body: Option<CapturedBody>,
    ) {
        if !self.enabled {
            return;
        }
        let upstream_target = upstream_target.map(cap_scalar);
        let model_served = model_served.map(cap_scalar);
        let upstream_body = upstream_body.map(CapturedBody::into_arc);
        let mut state = self.lock();
        state.prune_expired(now_ms());
        state.update(id, |record| {
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
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Attach the canonical/normalized body + requested model (D2). The body is a
    /// [`CapturedBody`] (provably redacted + capped); the model is `cap_scalar`-bounded.
    pub fn set_normalized(
        &self,
        api_call_id: &str,
        model_requested: Option<String>,
        normalized: Option<CapturedBody>,
    ) {
        if !self.enabled {
            return;
        }
        let model_requested = model_requested.map(cap_scalar);
        let normalized = normalized.map(CapturedBody::into_arc);
        let mut state = self.lock();
        state.prune_expired(now_ms());
        state.update(api_call_id, |record| {
            if model_requested.is_some() {
                record.model_requested = model_requested.clone();
            }
            if normalized.is_some() {
                record.normalized = normalized.clone();
            }
        });
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Mark a flow terminal (D3). Stamps `status`, `finished_ms`, `elapsed_ms`
    /// (`record.started_at.elapsed()` — a MONOTONIC `Instant` delta, never an
    /// epoch-ms subtraction), the terminal reason, and the serving-provider
    /// attribution read from D2's `ServingToken` at finalize time. The serving
    /// provider name (failover provider, else routing route) lands in
    /// `upstream_target` ONLY when the leaf did not already capture an upstream URL
    /// there — so the URL the leaf saw wins, and a path that never reached the leaf
    /// (pre-spawn early return, midstream cancel before first chunk) still records
    /// WHO would have served it. Idempotent at the store level too: a second
    /// finalize just re-stamps the same terminal fields (the D3 CAS guard is the
    /// real exactly-once authority).
    pub fn finalize(
        &self,
        api_call_id: &str,
        status: FlowStatus,
        terminal_reason: Option<String>,
        serving_provider: Option<String>,
    ) {
        if !self.enabled {
            return;
        }
        let terminal_reason = terminal_reason.map(cap_scalar);
        let serving_provider = serving_provider.map(cap_scalar);
        let now = now_ms();
        let mut state = self.lock();
        state.prune_expired(now);
        state.update(api_call_id, |record| {
            record.status = status;
            record.finished_ms = Some(now);
            record.elapsed_ms = Some(record.started_at.elapsed().as_millis());
            if terminal_reason.is_some() {
                record.terminal_reason = terminal_reason.clone();
            }
            // Don't clobber the leaf-captured upstream URL; only fill the slot when
            // it is still empty (the pre-leaf / pre-first-chunk paths).
            if record.upstream_target.is_none() && serving_provider.is_some() {
                record.upstream_target = serving_provider.clone();
            }
        });
        state.enforce_caps(self.summary_quota_bytes);
    }

    /// Attach token usage (D3). UPSERT semantics: the caller passes the running
    /// CUMULATIVE total for the flow, never an increment, so a multi-chunk turn
    /// whose usage is already cumulative does NOT double-count.
    pub fn record_usage(&self, api_call_id: &str, usage: FlowUsage) {
        if !self.enabled {
            return;
        }
        let mut state = self.lock();
        state.prune_expired(now_ms());
        state.update(api_call_id, |record| {
            record.usage = Some(usage);
        });
    }

    /// Mint the D3 **L0 middleware guard** for a freshly `open`ed flow (the
    /// `compare_exchange(OpenL0 → Finalized-Failed)` RAII fallback). Returns `None`
    /// when the store is disabled OR the record is unknown (so a disabled store
    /// stays a pure no-op). The middleware holds the guard across `next.run`: if the
    /// request never reaches the engine (an extractor/`Json` rejection, a panic in a
    /// layer above the handler) the record is still `OpenL0` at the guard's `Drop`,
    /// which CASes it to `Finalized` and stamps a `Failed("unhandled")` terminal —
    /// no orphan stuck `Open`. If the engine claimed it (`ClaimedL1`), the L0 `Drop`
    /// is inert: the CAS fails and L1 owns finalization. This is SEPARATE from
    /// [`open`](Self::open) (which stays infallible + guardless) so the store's many
    /// internal/test callers and the D2 leaf helper — which manage the lifecycle via
    /// the engine's L1 path, not `next.run` — are unaffected.
    pub fn middleware_guard(&self, api_call_id: &str) -> Option<MiddlewareGuard> {
        if !self.enabled {
            return None;
        }
        let claim = self.lock().by_id.get(api_call_id)?.claim.clone();
        Some(MiddlewareGuard {
            store: self.clone(),
            api_call_id: api_call_id.to_string(),
            claim,
        })
    }

    /// Mint the D3 **L1 engine guard** by CASing the record's claim
    /// `OpenL0 → ClaimedL1`. Returns `None` when the store is disabled, the record
    /// is unknown, or the CAS loses (already `ClaimedL1`/`Finalized`) — so only the
    /// FIRST engine guard for a flow owns finalization, and a disabled store stays a
    /// no-op. On success the engine owns finalization on EVERY exit path (the
    /// pre-spawn early returns, the spawned body, the terminal arms) via
    /// [`TelemetryGuard::finalize`]; its RAII `Drop` is the fallback that finalizes
    /// `Cancelled` if a path is missed (client hang-up / panic / cancel). The guard
    /// holds only `Arc`s + an `Instant` + an owned id, so it is `Send` and crosses
    /// the `tokio::spawn` in `stream_responses`.
    pub fn engine_guard(
        &self,
        api_call_id: &str,
        serving: std::sync::Arc<crate::upstream::ServingToken>,
    ) -> Option<TelemetryGuard> {
        if !self.enabled {
            return None;
        }
        let claim = self.lock().by_id.get(api_call_id)?.claim.clone();
        // CAS OpenL0 → ClaimedL1. Only the winner gets a guard.
        claim
            .compare_exchange(
                CLAIM_OPEN_L0,
                CLAIM_CLAIMED_L1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()?;
        Some(TelemetryGuard {
            store: self.clone(),
            api_call_id: api_call_id.to_string(),
            claim,
            serving,
            started: Instant::now(),
        })
    }

    /// Live records, newest-first. Empty when disabled. Prunes expired records
    /// first (D1 R1 #7 — TTL no longer depends on `open` traffic).
    pub fn list(&self) -> Vec<Arc<FlowRecord>> {
        if !self.enabled {
            return Vec::new();
        }
        let mut state = self.lock();
        state.prune_expired(now_ms());
        state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.by_id.get(id).cloned())
            .collect()
    }

    /// Resolve a single record by `api_call_id` OR `response_id` (via the link
    /// index). `None` when disabled or unknown. Prunes expired records first.
    pub fn detail(&self, id: &str) -> Option<Arc<FlowRecord>> {
        if !self.enabled {
            return None;
        }
        let mut state = self.lock();
        state.prune_expired(now_ms());
        if let Some(record) = state.by_id.get(id) {
            return Some(Arc::clone(record));
        }
        let api_call_id = state.link_index.get(id)?;
        state.by_id.get(api_call_id).cloned()
    }

    /// Body-free snapshot summaries, newest-first. Empty when disabled. Prunes
    /// expired records first.
    pub fn snapshot_summaries(&self) -> Vec<SnapshotFlowSummary> {
        if !self.enabled {
            return Vec::new();
        }
        let mut state = self.lock();
        state.prune_expired(now_ms());
        state
            .order
            .iter()
            .rev()
            .filter_map(|id| state.by_id.get(id))
            .map(|record| SnapshotFlowSummary::from_record(record))
            .collect()
    }

    /// Test-only: prune at a caller-supplied clock so the TTL is deterministically
    /// observable without sleeping (D1 R1 #7).
    #[cfg(test)]
    fn prune_at(&self, now_ms: u128) {
        let mut state = self.lock();
        state.prune_expired(now_ms);
    }

    /// Test-only: backdate a record's `started_ms` so a deterministic TTL test can
    /// age it past the retention window.
    #[cfg(test)]
    fn force_started_ms(&self, api_call_id: &str, started_ms: u128) {
        let mut state = self.lock();
        state.update(api_call_id, |record| record.started_ms = started_ms);
    }
}

impl Default for DashboardFlowStore {
    fn default() -> Self {
        Self::new()
    }
}

/// D3 **L0 (middleware) telemetry guard**: an RAII fallback that finalizes a flow
/// record IFF it is still `OpenL0` at `Drop`. Held by `log_api_call` across
/// `next.run`. It owns a cheap store handle (`Arc<Mutex<_>>` clone) + the record's
/// shared `claim` `Arc`, so it carries no borrow and is trivially `Send`. The CAS
/// (`OpenL0 → Finalized`) is the ONLY ownership-transfer mechanism — if the engine
/// claimed the record (`ClaimedL1`) the CAS here fails and L1 owns finalization;
/// the L0 `Drop` is then inert. This catches the narrow window where the middleware
/// opened a record but the request NEVER reached the engine (an extractor/`Json`
/// rejection, a layer panic): without L0 that record would be stuck `Open` forever.
#[must_use = "the L0 middleware guard finalizes an orphan flow on Drop; hold it across next.run"]
pub struct MiddlewareGuard {
    store: DashboardFlowStore,
    api_call_id: String,
    claim: Arc<AtomicU8>,
}

impl Drop for MiddlewareGuard {
    fn drop(&mut self) {
        // Finalize ONLY if still OpenL0 (the engine never claimed it). Race-free:
        // the CAS atomically transfers ownership; a concurrent L1 claim makes this
        // fail and L1 finalizes instead.
        if self
            .claim
            .compare_exchange(
                CLAIM_OPEN_L0,
                CLAIM_FINALIZED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            self.store.finalize(
                &self.api_call_id,
                FlowStatus::Failed,
                Some("unhandled".to_string()),
                None,
            );
        }
    }
}

/// D3 **L1 (engine) telemetry guard**: owns finalization of a flow once it has CASed
/// the record `OpenL0 → ClaimedL1` (via [`DashboardFlowStore::engine_guard`]). The
/// engine calls [`finalize`](Self::finalize) on EVERY deterministic exit path
/// (pre-spawn early returns, the spawned body's `Completed`/`Failed` arms) with the
/// terminal status; the RAII `Drop` is the fallback that finalizes `Cancelled` if a
/// path is missed (client hang-up mid-stream, panic, cancel). Finalization is
/// IDEMPOTENT: it CASes `ClaimedL1 → Finalized`, so the explicit call wins and the
/// `Drop` fallback then no-ops (and a double explicit call no-ops).
///
/// Holds only `Arc`s (`store` handle, shared `claim`, shared `serving`), an owned
/// `String` id, and a monotonic `Instant` — all `Send` — so the guard moves into the
/// `tokio::spawn` in `stream_responses` (the midstream-cancel test compiles + runs
/// across the spawn). The serving provider for attribution is read from the shared
/// `ServingToken` AT finalize time (not at construction), so a provider that the
/// failover/routing layer tags AFTER the guard is built is still recorded.
#[must_use = "the L1 engine guard finalizes the flow on Drop; bind it for the flow's lifetime"]
pub struct TelemetryGuard {
    store: DashboardFlowStore,
    api_call_id: String,
    claim: Arc<AtomicU8>,
    serving: Arc<crate::upstream::ServingToken>,
    started: Instant,
}

impl TelemetryGuard {
    /// The flow's `api_call_id` (so the engine can drive `record_usage` upserts on
    /// the SAME record the guard owns without re-threading the id separately).
    pub fn api_call_id(&self) -> &str {
        &self.api_call_id
    }

    /// Monotonic elapsed since the guard claimed the flow — the latency source the
    /// engine reports, never an epoch-ms subtraction. (`finalize` also stamps the
    /// store's own `started_at.elapsed()`; this accessor exists for callers/tests
    /// that want the guard-relative value.)
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Explicitly finalize the flow with `status` + `terminal_reason`, attributing
    /// the serving provider read from the shared `ServingToken` (failover provider,
    /// else routing route). IDEMPOTENT via `compare_exchange(ClaimedL1 → Finalized)`:
    /// the first finalize (explicit OR the `Drop` fallback) wins; later calls
    /// no-op, so the engine's explicit terminal status is never overwritten by the
    /// `Drop`'s `Cancelled` fallback.
    pub fn finalize(&self, status: FlowStatus, terminal_reason: Option<String>) {
        if self
            .claim
            .compare_exchange(
                CLAIM_CLAIMED_L1,
                CLAIM_FINALIZED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            let (route, provider) = self.serving.snapshot();
            // Prefer the actual serving provider; fall back to the route name.
            let serving = provider.or(route);
            self.store
                .finalize(&self.api_call_id, status, terminal_reason, serving);
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Fallback: a path that did not call `finalize` explicitly (client hang-up
        // mid-stream, panic, cancel) lands here while still `ClaimedL1`. Finalize
        // `Cancelled` — the conservative terminal for "the engine stopped owning
        // this flow without a clean Completed/Failed". If the engine already
        // finalized, the CAS in `finalize` already moved the claim to `Finalized`
        // and this no-ops.
        self.finalize(FlowStatus::Cancelled, Some("dropped".to_string()));
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

    /// Resolve any flow id — an `api_call_id` (direct `by_id` key) OR a
    /// `response_id` (via the link index) — to the owning `api_call_id`. Mirrors
    /// the dual-id join `detail` already performs so the mutators below can be
    /// driven by EITHER id (D2's leaf only knows the flow's `response_id`; D3 also
    /// keys by it). `None` when the id matches no live record.
    fn resolve_id(&self, id: &str) -> Option<String> {
        if self.by_id.contains_key(id) {
            return Some(id.to_string());
        }
        self.link_index.get(id).cloned()
    }

    /// Copy-on-write mutate the record for `id` (an `api_call_id` OR a linked
    /// `response_id`): clone the inner record (the `claim` `Arc` is shared, NOT
    /// deep-copied, so D3's atomic identity persists), apply `edit`, and swap the
    /// `Arc` back in. Adjusts `live_summary_bytes` by the byte delta. No-op when the
    /// id resolves to no record.
    fn update(&mut self, id: &str, edit: impl FnOnce(&mut FlowRecord)) {
        let Some(api_call_id) = self.resolve_id(id) else {
            return;
        };
        let Some(existing) = self.by_id.get(&api_call_id) else {
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
        self.by_id.insert(api_call_id, Arc::new(next));
    }

    /// Drop records whose age exceeds the TTL, keyed off `started_ms` vs a
    /// caller-supplied `now`.
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

    /// Bring the running summary-byte total back under quota. Phase 1: walk records
    /// oldest-first dropping their BODIES (the record survives as a body-free
    /// summary). Phase 2 (D1 R2 #3): if shedding every body is still not enough —
    /// because scalar/header-only records dominate the total — evict OLDEST WHOLE
    /// records until under quota, so the quota is a HARD bound the store cannot
    /// exceed regardless of body presence.
    fn enforce_summary_quota(&mut self, quota_bytes: usize) {
        if self.live_summary_bytes <= quota_bytes {
            return;
        }
        // Phase 1: shed bodies oldest-first.
        let ids: Vec<String> = self.order.iter().cloned().collect();
        for id in ids {
            if self.live_summary_bytes <= quota_bytes {
                return;
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
        // Phase 2: bodies are all gone but still over quota → evict whole records
        // oldest-first. (`remove` keeps `order`/`link_index`/byte-total consistent.)
        while self.live_summary_bytes > quota_bytes {
            let Some(oldest) = self.order.front().cloned() else {
                break;
            };
            self.remove(&oldest);
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
        // Drop any dangling link-index entries that pointed at this id.
        self.link_index.retain(|_, owner| owner != api_call_id);
    }
}

/// Capped + redacting capture of a request/response body → a [`CapturedBody`]
/// (`Arc<[u8]>` ≤ [`BODY_CAP`], secrets redacted inline) via the shared O(CAP)
/// primitive in [`crate::redaction`]. This is the ONLY constructor of
/// `CapturedBody`, so every body that reaches a store mutator is provably sanitized
/// (D1 R2 #2). Never retains a slice of `raw`.
pub fn capture_body(raw: &[u8]) -> CapturedBody {
    let bytes = crate::redaction::capture_capped_redacted(raw, BODY_CAP, SCALAR_CAP);
    CapturedBody(Arc::from(bytes.into_boxed_slice()))
}

/// Capture a `Serialize` value (the typed on-wire request) into a [`CapturedBody`]
/// WITHOUT a full O(body) `serde_json::to_vec` (D2 R1 #1). Serializes directly into
/// a writer bounded at `2 × BODY_CAP` and redacts the bounded bytes via the shared
/// O(CAP) primitive — peak heap is O(`BODY_CAP`), never O(body). Same `CapturedBody`
/// guarantees as [`capture_body`] (provably redacted + capped); the leaf uses THIS
/// so a 256 MiB request body is never serialized in full just to be capped.
pub fn capture_body_from_value<T: serde::Serialize>(value: &T) -> CapturedBody {
    let bytes = crate::redaction::capture_capped_redacted_value(value, BODY_CAP, SCALAR_CAP);
    CapturedBody(Arc::from(bytes.into_boxed_slice()))
}

/// Redact + cap request headers → a [`CapturedHeaders`] (sensitive names →
/// `"[redacted]"`; every other value image-URI-stripped then capped to
/// [`SCALAR_CAP]`; names capped too). The ONLY constructor of `CapturedHeaders`, so
/// the store can never be handed raw header pairs (D1 R2 #2).
pub fn redact_headers(headers: &axum::http::HeaderMap) -> CapturedHeaders {
    CapturedHeaders(crate::redaction::redact_headers_capped(headers, SCALAR_CAP))
}

/// Cap a single retained scalar string to [`SCALAR_CAP`] bytes on a UTF-8 char
/// boundary (D1 R1 #5 — every dynamic scalar the record keeps is bounded). When
/// over cap, the bounded prefix is COPIED into a FRESH `String` (D1 R2... R3 #2):
/// `String::truncate` keeps the ORIGINAL capacity, so an oversized input would
/// retain an unbounded backing allocation while the quota accounting only sees the
/// 4 KiB length. Reallocating makes retained capacity == bounded length.
fn cap_scalar(text: String) -> String {
    if text.len() <= SCALAR_CAP {
        return text;
    }
    let bytes = text.as_bytes();
    let mut end = SCALAR_CAP;
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    // `to_string()` on the slice allocates exactly `end` bytes — the original
    // (possibly huge) buffer is dropped, not retained behind a shrunk length.
    text[..end].to_string()
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

    /// Mint a `CapturedBody` from raw bytes via the real capture primitive (the only
    /// way to build one — that is the point of the newtype).
    fn cap(bytes: &[u8]) -> CapturedBody {
        capture_body(bytes)
    }

    /// An empty `CapturedHeaders` (minted via the real redactor).
    fn no_headers() -> CapturedHeaders {
        redact_headers(&HeaderMap::new())
    }

    fn open_simple(store: &DashboardFlowStore, api: &str) {
        store.open(
            api.to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            no_headers(),
            None,
        );
    }

    #[test]
    fn disabled_store_is_a_no_op() {
        let store = DashboardFlowStore::disabled();
        assert!(!store.is_enabled());
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            no_headers(),
            Some(cap(b"{}")),
        );
        store.link("resp_1".to_string(), "api_1".to_string());
        store.record_usage("api_1", FlowUsage::default());
        store.finalize("api_1", FlowStatus::Completed, None, None);
        assert!(
            store.middleware_guard("api_1").is_none(),
            "disabled store mints no L0 guard"
        );
        assert!(store.list().is_empty(), "disabled store records nothing");
        assert!(store.detail("api_1").is_none());
        assert!(store.snapshot_summaries().is_empty());
    }

    #[test]
    fn open_then_list_and_detail_round_trip() {
        let store = DashboardFlowStore::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&headers),
            Some(cap(b"{\"model\":\"m\"}")),
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
        assert!(store.detail("api_1").is_some());
        assert!(store.detail("nope").is_none());
    }

    #[test]
    fn list_is_newest_first() {
        let store = DashboardFlowStore::new();
        for i in 0..3 {
            open_simple(&store, &format!("api_{i}"));
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
        open_simple(&store, "api_1");
        assert!(store.detail("api_1").is_some());
        assert!(store.detail("resp_1").is_none());

        store.link("resp_1".to_string(), "api_1".to_string());
        // A second link must NOT overwrite the response_id (first-link-wins).
        store.link("resp_other".to_string(), "api_1".to_string());

        let record = store.detail("api_1").expect("record");
        assert_eq!(
            record.response_id.as_deref(),
            Some("resp_1"),
            "link fires once; the first response_id wins"
        );
        let by_resp = store.detail("resp_1").expect("join by response_id");
        assert_eq!(by_resp.api_call_id, "api_1");
        // The aliasing response_id was never indexed.
        assert!(
            store.detail("resp_other").is_none(),
            "second link did not create an alias"
        );
    }

    #[test]
    fn link_unknown_id_is_a_no_op_and_leaks_no_index() {
        let store = DashboardFlowStore::new();
        // No record opened for api_404.
        store.link("resp_x".to_string(), "api_404".to_string());
        assert!(store.detail("resp_x").is_none(), "no index entry leaked");
        assert!(store.detail("api_404").is_none());
        assert!(store.list().is_empty());
    }

    #[test]
    fn link_double_call_is_idempotent() {
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        store.link("resp_1".to_string(), "api_1".to_string());
        store.link("resp_1".to_string(), "api_1".to_string());
        let record = store.detail("api_1").expect("record");
        assert_eq!(record.response_id.as_deref(), Some("resp_1"));
        assert!(store.detail("resp_1").is_some());
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
                open_simple(&store, &api);
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
    fn mutators_resolve_by_response_id_via_link_index() {
        // D2 needs the id-keyed mutators (here `set_upstream`/`set_normalized`) to
        // accept the flow's `response_id` — the leaf only knows that — and join it to
        // the owning `api_call_id` via the link index, exactly like `detail`.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        store.link("resp_1".to_string(), "api_1".to_string());
        // Drive the mutators by RESPONSE_ID, not api_call_id.
        store.set_upstream(
            "resp_1",
            Some("https://upstream".to_string()),
            Some("served-m".to_string()),
            Some(cap(b"{\"on\":\"wire\"}")),
        );
        store.set_normalized("resp_1", Some("requested-m".to_string()), None);
        let record = store.detail("api_1").expect("record");
        assert_eq!(record.upstream_target.as_deref(), Some("https://upstream"));
        assert_eq!(record.model_served.as_deref(), Some("served-m"));
        assert_eq!(record.model_requested.as_deref(), Some("requested-m"));
        let body = record.upstream_body.as_ref().expect("upstream body set");
        assert_eq!(&**body, b"{\"on\":\"wire\"}");
        // An unknown id still no-ops (no panic, no record).
        store.set_upstream("resp_unknown", Some("x".to_string()), None, None);
        assert!(store.detail("resp_unknown").is_none());
    }

    #[test]
    fn finalize_stamps_terminal_fields() {
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
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
            None,
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

    use std::sync::atomic::Ordering::SeqCst;

    fn serving() -> Arc<crate::upstream::ServingToken> {
        Arc::new(crate::upstream::ServingToken::default())
    }

    #[test]
    fn l0_guard_finalizes_orphan_left_open() {
        // The L0 middleware guard's Drop finalizes a record that was opened but
        // NEVER claimed by the engine (extractor/JSON rejection) — no orphan Open.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        {
            let _guard = store.middleware_guard("api_1").expect("guard minted");
            // While held, the record is still Open (nothing claimed it).
            assert_eq!(store.detail("api_1").unwrap().status, FlowStatus::Open);
        } // Drop here.
        let record = store.detail("api_1").expect("record survives");
        assert_eq!(
            record.status,
            FlowStatus::Failed,
            "L0 Drop finalized the orphan Failed"
        );
        assert_eq!(record.terminal_reason.as_deref(), Some("unhandled"));
        assert_eq!(
            record.claim.load(SeqCst),
            CLAIM_FINALIZED,
            "claim CASed OpenL0 → Finalized"
        );
    }

    #[test]
    fn l1_claim_disarms_l0_guard() {
        // When the engine claims the record (ClaimedL1) the L0 Drop is inert: its
        // CAS(OpenL0 → Finalized) fails, so L1 owns finalization. No transition to
        // Failed-unhandled occurs.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        let l0 = store.middleware_guard("api_1").expect("L0 guard");
        let l1 = store
            .engine_guard("api_1", serving())
            .expect("L1 claim wins");
        assert_eq!(
            store.detail("api_1").unwrap().claim.load(SeqCst),
            CLAIM_CLAIMED_L1
        );
        // A SECOND engine guard cannot claim (CAS already lost OpenL0).
        assert!(
            store.engine_guard("api_1", serving()).is_none(),
            "second L1 claim loses the CAS"
        );
        drop(l0); // inert — record is ClaimedL1, not OpenL0.
        assert_eq!(
            store.detail("api_1").unwrap().status,
            FlowStatus::Open,
            "L0 Drop did NOT finalize a claimed record"
        );
        l1.finalize(
            FlowStatus::Completed,
            Some("response.completed".to_string()),
        );
        assert_eq!(store.detail("api_1").unwrap().status, FlowStatus::Completed);
    }

    #[test]
    fn l1_finalize_is_idempotent_and_drop_no_ops() {
        // Explicit finalize wins; the Drop fallback then no-ops (does NOT overwrite
        // Completed with the Cancelled fallback).
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        let guard = store.engine_guard("api_1", serving()).expect("claim");
        guard.finalize(
            FlowStatus::Completed,
            Some("response.completed".to_string()),
        );
        // A second explicit finalize no-ops (CAS already Finalized).
        guard.finalize(FlowStatus::Failed, Some("late".to_string()));
        assert_eq!(store.detail("api_1").unwrap().status, FlowStatus::Completed);
        drop(guard); // Drop fallback must NOT flip it to Cancelled.
        let record = store.detail("api_1").expect("record");
        assert_eq!(record.status, FlowStatus::Completed);
        assert_eq!(
            record.terminal_reason.as_deref(),
            Some("response.completed")
        );
        assert_eq!(record.claim.load(SeqCst), CLAIM_FINALIZED);
    }

    #[test]
    fn l1_drop_without_explicit_finalize_records_cancelled() {
        // A missed exit path (cancel/panic) → Drop finalizes Cancelled, never an
        // orphan stuck ClaimedL1.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        {
            let _guard = store.engine_guard("api_1", serving()).expect("claim");
            assert_eq!(store.detail("api_1").unwrap().status, FlowStatus::Open);
        } // Drop with no explicit finalize.
        let record = store.detail("api_1").expect("record");
        assert_eq!(record.status, FlowStatus::Cancelled);
        assert_eq!(record.terminal_reason.as_deref(), Some("dropped"));
        assert_eq!(record.claim.load(SeqCst), CLAIM_FINALIZED);
    }

    #[test]
    fn l1_finalize_attributes_serving_provider_when_no_leaf_target() {
        // The serving provider is read from the ServingToken AT finalize (so a
        // provider tagged after the guard is built is still recorded), and lands in
        // upstream_target only when the leaf left it empty.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        let token = serving();
        let guard = store
            .engine_guard("api_1", Arc::clone(&token))
            .expect("claim");
        // Provider tagged AFTER the guard exists (mirrors failover first-chunk).
        token.set_provider("backend-b");
        guard.finalize(FlowStatus::Completed, None);
        assert_eq!(
            store.detail("api_1").unwrap().upstream_target.as_deref(),
            Some("backend-b"),
            "serving provider recorded as the target when the leaf set none"
        );
    }

    #[test]
    fn l1_finalize_does_not_clobber_leaf_upstream_target() {
        // When the leaf already captured an upstream URL, the serving-provider
        // attribution must NOT overwrite it.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        store.set_upstream("api_1", Some("https://real-url".to_string()), None, None);
        let token = serving();
        token.set_provider("backend-b");
        let guard = store.engine_guard("api_1", token).expect("claim");
        guard.finalize(FlowStatus::Completed, None);
        assert_eq!(
            store.detail("api_1").unwrap().upstream_target.as_deref(),
            Some("https://real-url"),
            "leaf URL wins over serving-provider fallback"
        );
    }

    #[test]
    fn disabled_store_mints_no_guards() {
        let store = DashboardFlowStore::disabled();
        assert!(store.middleware_guard("api_1").is_none());
        assert!(store.engine_guard("api_1", serving()).is_none());
    }

    #[test]
    fn guards_are_send() {
        // The L1 guard must be Send to cross the tokio::spawn in stream_responses.
        fn assert_send<T: Send>() {}
        assert_send::<TelemetryGuard>();
        assert_send::<MiddlewareGuard>();
    }

    #[test]
    fn claim_arc_is_shared_across_cow_mutations() {
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_1");
        let before = store.detail("api_1").expect("record").claim.clone();
        store.set_normalized("api_1", Some("model".to_string()), Some(cap(b"{}")));
        let after = store.detail("api_1").expect("record").claim.clone();
        assert!(
            Arc::ptr_eq(&before, &after),
            "claim Arc identity must persist across COW updates for D3"
        );
        before.store(CLAIM_CLAIMED_L1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            after.load(std::sync::atomic::Ordering::SeqCst),
            CLAIM_CLAIMED_L1
        );
    }

    #[test]
    fn summary_quota_evicts_oldest_bodies_keeping_records() {
        let store = DashboardFlowStore {
            enabled: true,
            state: Arc::new(Mutex::new(DashboardFlowState::default())),
            summary_quota_bytes: 4 * 1024,
        };
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
                no_headers(),
                Some(cap(&json)),
            );
        }
        assert_eq!(store.list().len(), 3, "records survive body eviction");
        let oldest = store.detail("api_0").expect("oldest record present");
        assert!(
            oldest.inbound_body.is_none(),
            "oldest body evicted under quota"
        );
        let newest = store.detail("api_2").expect("newest record present");
        assert!(newest.inbound_body.is_some(), "newest body retained");
        assert_eq!(store.snapshot_summaries().len(), 3);
    }

    #[test]
    fn long_scalars_are_capped_counted_and_can_trigger_eviction() {
        // D1 R1 #5: long dynamic scalars (model/uri/target) must be capped to
        // SCALAR_CAP AND counted in `live_summary_bytes`, so their bytes can trip
        // the quota and evict an older body. A small quota makes the scalar bytes —
        // not bodies — the decisive contribution.
        // Quota 16 KiB: api_0's ~10 KiB body fits; api_1's ~12 KiB of CAPPED scalars
        // (3 x SCALAR_CAP) fit on their own; but api_0.body + api_1.scalars exceeds
        // the quota, so the counted scalars force api_0's BODY to be shed while api_1
        // (under-quota by itself) survives.
        let store = DashboardFlowStore {
            enabled: true,
            state: Arc::new(Mutex::new(DashboardFlowState::default())),
            summary_quota_bytes: 16 * 1024,
        };
        // api_0: a ~10 KiB body (JSON array of many short strings; none individually
        // capped, so the captured body is genuinely large), no large scalars.
        let array_body = format!("{{\"a\":[{}]}}", vec!["\"xx\""; 2000].join(","));
        store.open(
            "api_0".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            no_headers(),
            Some(cap(array_body.as_bytes())),
        );
        assert!(
            store.detail("api_0").unwrap().inbound_body.is_some(),
            "api_0 body retained under quota initially"
        );
        // api_1: oversized scalars (each capped to SCALAR_CAP, all counted), no body.
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/".to_string() + &"u".repeat(64 * 1024),
            no_headers(),
            None,
        );
        store.set_upstream(
            "api_1",
            Some("https://".to_string() + &"t".repeat(64 * 1024)),
            Some("m".repeat(64 * 1024)),
            None,
        );
        let record = store
            .detail("api_1")
            .expect("api_1 survives (under quota alone)");
        assert!(record.uri.len() <= SCALAR_CAP, "uri capped");
        assert!(
            record.upstream_target.as_ref().unwrap().len() <= SCALAR_CAP,
            "upstream_target capped"
        );
        assert!(
            record.model_served.as_ref().unwrap().len() <= SCALAR_CAP,
            "model_served capped"
        );
        // The capped-but-counted scalars pushed total over quota → api_0 body shed.
        let oldest = store.detail("api_0").expect("api_0 record still present");
        assert!(
            oldest.inbound_body.is_none(),
            "oldest body evicted once scalars counted toward quota"
        );
    }

    #[test]
    fn whole_records_evicted_when_body_eviction_insufficient() {
        // D1 R2 #3: when bodies are all gone (or absent) but the total still exceeds
        // quota — scalar/header-only records dominate — evict OLDEST WHOLE records
        // until under quota, so the quota is a HARD bound.
        let store = DashboardFlowStore {
            enabled: true,
            state: Arc::new(Mutex::new(DashboardFlowState::default())),
            // ~10 capped-scalar records fit; many more must force whole-record eviction.
            summary_quota_bytes: 64 * 1024,
        };
        let mut headers = HeaderMap::new();
        // A ~4 KiB header value (capped to SCALAR_CAP) so each record is header-heavy
        // with NO body to shed.
        headers.insert(
            HeaderName::from_static("x-big"),
            HeaderValue::from_str(&"h".repeat(8 * 1024)).unwrap(),
        );
        let captured_headers = redact_headers(&headers);
        for i in 0..40 {
            store.open(
                format!("api_{i}"),
                "POST".to_string(),
                "/v1/responses".to_string(),
                captured_headers.clone(),
                None, // no body → body eviction cannot help
            );
        }
        // The store stayed under quota by evicting oldest WHOLE records.
        let live = store.list();
        let total: usize = live
            .iter()
            .map(|r| {
                r.headers
                    .iter()
                    .map(|(n, v)| n.len() + v.len())
                    .sum::<usize>()
                    + r.uri.len()
            })
            .sum();
        assert!(
            total <= 64 * 1024,
            "store exceeded quota ({total} > 64 KiB) — whole-record eviction failed"
        );
        // Newest survive, oldest are gone.
        assert!(store.detail("api_39").is_some(), "newest record retained");
        assert!(store.detail("api_0").is_none(), "oldest record evicted");
        assert!(live.len() < 40, "some records were evicted");
    }

    #[test]
    fn cap_scalar_reallocates_to_bounded_capacity() {
        // D1 R3 #2: an oversized scalar must not retain its original (huge) backing
        // capacity behind a shrunk length — cap_scalar copies the bounded prefix into
        // a fresh String so capacity == bounded length.
        let huge = "x".repeat(1024 * 1024); // 1 MiB
        let capped = cap_scalar(huge);
        assert!(capped.len() <= SCALAR_CAP, "length bounded");
        assert!(
            capped.capacity() <= SCALAR_CAP + 4,
            "capacity bounded to ~SCALAR_CAP, not the 1 MiB original: cap={}",
            capped.capacity()
        );
        // A within-cap string is returned unchanged (no needless realloc).
        let small = "hello".to_string();
        assert_eq!(cap_scalar(small), "hello");
    }

    #[test]
    fn record_cap_evicts_oldest_whole_records() {
        let store = DashboardFlowStore::new();
        for i in 0..(FLOW_CAP + 1) {
            open_simple(&store, &format!("api_{i}"));
        }
        assert_eq!(store.list().len(), FLOW_CAP);
        assert!(store.detail("api_0").is_none(), "oldest record evicted");
        assert!(store.detail(&format!("api_{FLOW_CAP}")).is_some());
    }

    #[test]
    fn expired_records_are_pruned_on_read_deterministically() {
        // D1 R1 #7: TTL pruning runs on reads/mutations, not only `open`.
        let store = DashboardFlowStore::new();
        open_simple(&store, "api_old");
        // Backdate it well past the retention window.
        store.force_started_ms("api_old", 1);
        open_simple(&store, "api_new");
        // A read at a `now` past the TTL must drop the old record but keep the new.
        store.prune_at(FLOW_TTL_MS + 100);
        assert!(store.detail("api_old").is_none(), "expired record pruned");
        assert!(store.detail("api_new").is_some(), "fresh record retained");
        // And the public read path prunes too (no explicit prune_at needed): age
        // api_new and observe list() drop it at the wall clock.
        store.force_started_ms("api_new", 1);
        // `list()` prunes at `now_ms()`, which is far past `started_ms = 1 + TTL`.
        assert!(
            store.list().iter().all(|r| r.api_call_id != "api_new"),
            "read path prunes expired records"
        );
    }

    // -------------------------------------------------------------------
    // Capture seam (the heavy serializer lives in crate::redaction; these
    // confirm the dashboard wrapper + store integration).
    // -------------------------------------------------------------------

    #[test]
    fn capture_body_peak_allocation_is_bounded_for_10mib_body() {
        // THE crux acceptance criterion: serializing a 10 MiB body keeps PEAK LIVE
        // heap use O(CAP), not O(body) (D1 R2 #5 — peak-live also catches a path
        // doing many small unfreed allocations). Build the input OUTSIDE the armed
        // region so only `capture_body`'s own allocations count.
        const TEN_MIB: usize = 10 * 1024 * 1024;
        let big = "x".repeat(TEN_MIB);
        let json = format!("{{\"text\":\"{big}\"}}");
        let raw = json.into_bytes();
        let ceiling = BODY_CAP + SCALAR_CAP + 64 * 1024;
        let (captured, peak_live) =
            crate::test_alloc_probe::peak_live_alloc_during(|| capture_body(&raw));
        assert!(captured.as_bytes().len() <= BODY_CAP);
        assert!(
            peak_live <= ceiling,
            "capture_body held peak-live {peak_live} bytes (> {ceiling}) for a {TEN_MIB}-byte \
             body — it must stream, not materialize the whole body"
        );
    }

    #[test]
    fn capture_body_peak_bounded_for_10mib_single_key() {
        // D1 R1 #4a: a single 10 MiB OBJECT KEY must also stay O(CAP) (peak-live).
        const TEN_MIB: usize = 10 * 1024 * 1024;
        let big_key = "k".repeat(TEN_MIB);
        let json = format!("{{\"{big_key}\":\"v\"}}");
        let raw = json.into_bytes();
        let ceiling = BODY_CAP + SCALAR_CAP + 64 * 1024;
        let (captured, peak_live) =
            crate::test_alloc_probe::peak_live_alloc_during(|| capture_body(&raw));
        assert!(captured.as_bytes().len() <= BODY_CAP);
        assert!(
            peak_live <= ceiling,
            "huge key held peak-live {peak_live} bytes (> {ceiling}); must be O(CAP)"
        );
    }

    #[test]
    fn capture_body_peak_bounded_for_malformed_body() {
        // D1 R1 #4b + R2 #1a: the fallback must NOT materialize the body (no Value,
        // no retained lossy prefix) — a malformed 10 MiB body must stay O(CAP).
        const TEN_MIB: usize = 10 * 1024 * 1024;
        // Valid UTF-8 but NOT valid JSON (unterminated object) → fallback path.
        let mut raw = Vec::with_capacity(TEN_MIB + 16);
        raw.extend_from_slice(b"{\"a\":\"");
        raw.extend(std::iter::repeat_n(b'z', TEN_MIB));
        // no closing quote/brace
        let ceiling = BODY_CAP + SCALAR_CAP + 64 * 1024;
        let (captured, peak_live) =
            crate::test_alloc_probe::peak_live_alloc_during(|| capture_body(&raw));
        assert!(captured.as_bytes().len() <= BODY_CAP);
        assert!(
            peak_live <= ceiling,
            "malformed-body fallback held peak-live {peak_live} bytes (> {ceiling}); must be O(CAP)"
        );
    }

    #[test]
    fn capture_body_from_value_peak_allocation_is_bounded_for_10mib_prompt() {
        // D2 R1 #1 — THE crux: serializing a typed request whose prompt is 10 MiB
        // must keep PEAK LIVE heap O(CAP), NOT O(body). The old leaf did
        // `serde_json::to_vec` (a full ~10 MiB+ allocation, up to 256 MiB for a max
        // body) THEN capped — defeating the guarantee. The Serialize-direct writer is
        // bounded at 2×BODY_CAP, so peak stays well under O(body). Build the input
        // OUTSIDE the armed region so only the capture's own allocations count.
        use serde::Serialize;
        #[derive(Serialize)]
        struct Req {
            model: String,
            prompt: String,
        }
        const TEN_MIB: usize = 10 * 1024 * 1024;
        let req = Req {
            model: "served-model".to_string(),
            prompt: "x".repeat(TEN_MIB),
        };
        // Headroom: the bounded serialize writer holds ≤ 2×BODY_CAP, plus the capped
        // redacting pass's own ≤ BODY_CAP + SCALAR_CAP, plus slack. Crucially this is
        // a CONSTANT independent of the 10 MiB body, proving O(CAP) not O(body).
        let ceiling = 2 * BODY_CAP + BODY_CAP + SCALAR_CAP + 64 * 1024;
        let (captured, peak_live) =
            crate::test_alloc_probe::peak_live_alloc_during(|| capture_body_from_value(&req));
        assert!(captured.as_bytes().len() <= BODY_CAP);
        assert!(
            peak_live <= ceiling,
            "capture_body_from_value held peak-live {peak_live} bytes (> {ceiling}) for a \
             {TEN_MIB}-byte prompt — it must serialize into the bounded writer, NOT to_vec the \
             whole body"
        );
    }

    #[test]
    fn capture_body_from_value_redacts_secrets_and_round_trips_small_request() {
        // A within-bound typed request: secrets redacted, image URIs stripped, and
        // the on-wire field names preserved (valid JSON out). Serialize-direct path
        // must keep every redaction guarantee of the bytes path.
        let value = serde_json::json!({
            "model": "m",
            "api_key": "sk-VALUELEAK",
            "messages": [
                { "role": "user", "content": "see data:image/png;base64,IMGLEAK ok" }
            ]
        });
        let captured = capture_body_from_value(&value);
        let text = String::from_utf8_lossy(captured.as_bytes());
        assert!(!text.contains("VALUELEAK"), "api_key value redacted");
        assert!(!text.contains("IMGLEAK"), "data: payload redacted");
        assert!(text.contains("[redacted]"));
        assert!(text.contains("<redacted uri>"));
        let parsed: serde_json::Value =
            serde_json::from_slice(captured.as_bytes()).expect("captured value body is valid JSON");
        assert_eq!(parsed["model"], serde_json::json!("m"));
    }

    #[test]
    fn capture_body_from_value_overflow_routes_to_fixed_marker_no_leak() {
        // A request whose serialized form exceeds 2×BODY_CAP truncates mid-JSON in the
        // bounded writer → the strict parser bails → fixed marker. No raw/secret bytes
        // survive. Embed a secret PAST the 2×BODY_CAP bound to prove nothing leaks.
        let pad = "p".repeat(2 * BODY_CAP + 4096);
        let value = serde_json::json!({
            "filler": pad,
            "api_key": "sk-TAILLEAK",
        });
        let captured = capture_body_from_value(&value);
        let text = String::from_utf8_lossy(captured.as_bytes());
        assert!(
            text.starts_with("[redacted: unparseable body"),
            "overflowing body → fixed marker: {text}"
        );
        assert!(
            !text.contains("TAILLEAK"),
            "no secret survives the overflow path"
        );
        assert!(captured.as_bytes().len() <= BODY_CAP);
    }

    #[test]
    fn capture_body_redacts_sensitive_keys_inline() {
        let json = br#"{"model":"m","api_key":"sk-SECRETLEAK","nested":{"authorization":"Bearer TOKENLEAK"},"keep":"visible"}"#;
        let captured = capture_body(json);
        let text = String::from_utf8_lossy(captured.as_bytes());
        assert!(!text.contains("SECRETLEAK"), "api_key value redacted");
        assert!(!text.contains("TOKENLEAK"), "nested authorization redacted");
        assert!(text.contains("[redacted]"));
        assert!(text.contains("visible"), "non-sensitive value preserved");
    }

    #[test]
    fn capture_body_redacts_image_uris_in_strings() {
        let json =
            br#"{"content":"see data:image/png;base64,IMGLEAK and https://signed.x/i?sig=SIGLEAK"}"#;
        let captured = capture_body(json);
        let text = String::from_utf8_lossy(captured.as_bytes());
        assert!(!text.contains("IMGLEAK"), "data: payload redacted");
        assert!(!text.contains("SIGLEAK"), "signed-url token redacted");
        assert!(text.contains("<redacted uri>"));
    }

    #[test]
    fn capture_body_redacts_unicode_escaped_image_uris() {
        // D1 R1 #3: a JSON `\uXXXX`-escaped scheme must be DE-ESCAPED before
        // redaction. Build the body so it literally contains JSON unicode escapes
        // for the scheme letters (e.g. the four bytes `\`,`u`,`0`,`0`,`6`,`4` for
        // 'd'), so the stored string spells `data:`/`https:` with escapes;
        // de-escaping must expose them to `redact_image_uris`.
        let esc = |s: &str| -> String {
            s.chars()
                .map(|c| format!("\\u{:04x}", c as u32))
                .collect::<String>()
        };
        // Scheme letters are escaped; the rest is literal so the URI run is intact.
        let data_uri = format!("{}:image/png;base64,UNILEAK", esc("data"));
        let https_uri = format!("{}://h/p?sig=ESCSIGLEAK", esc("https"));
        let json = format!("{{\"a\":\"{data_uri} x\",\"b\":\"{https_uri} y\"}}");
        // Sanity: the body holds the ESCAPED (not literal) scheme.
        assert!(
            json.contains("\\u0064"),
            "test body uses \\u escapes for 'd'"
        );
        assert!(
            !json.contains("data:"),
            "test body has NO literal data: scheme"
        );

        let captured = capture_body(json.as_bytes());
        let text = String::from_utf8_lossy(captured.as_bytes());
        assert!(
            !text.contains("UNILEAK"),
            "unicode-escaped data: payload redacted: {text}"
        );
        assert!(
            !text.contains("ESCSIGLEAK"),
            "unicode-escaped https signed-url token redacted: {text}"
        );
        assert!(text.contains("<redacted uri>"));
    }

    #[test]
    fn capture_body_roundtrips_valid_json_structure() {
        let json = br#"{"a":1,"b":[true,false,null],"c":{"d":"e"}}"#;
        let captured = capture_body(json);
        let value: serde_json::Value =
            serde_json::from_slice(captured.as_bytes()).expect("captured body is valid JSON");
        assert_eq!(value["a"], serde_json::json!(1));
        assert_eq!(value["b"], serde_json::json!([true, false, null]));
        assert_eq!(value["c"]["d"], serde_json::json!("e"));
    }

    #[test]
    fn redact_headers_redacts_sensitive_and_uri_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer SECRETHEADER"),
        );
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("assistants=v2;token=BETASECRET"),
        );
        headers.insert(
            HeaderName::from_static("x-callback-url"),
            HeaderValue::from_static("https://cb.example.com/h?sig=HDRSIGLEAK"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        let redacted = redact_headers(&headers);
        let dumped = format!("{redacted:?}");
        assert!(!dumped.contains("SECRETHEADER"), "authorization redacted");
        // openai-beta is in the sensitive-key set now → value redacted.
        assert!(!dumped.contains("BETASECRET"), "openai-beta value redacted");
        // A URI-bearing header value is image-URI-redacted.
        assert!(
            !dumped.contains("HDRSIGLEAK"),
            "signed-url header value redacted"
        );
        assert!(dumped.contains("<redacted uri>") || dumped.contains("[redacted]"));
        assert!(dumped.contains("application/json"));
    }

    #[test]
    fn secret_persistence_prevention_end_to_end() {
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
            no_headers(),
            Some(cap(b"{\"model\":\"m\"}")),
        );
        let summaries = store.snapshot_summaries();
        assert_eq!(summaries.len(), 1);
        let json = serde_json::to_string(&summaries[0]).expect("serialize summary");
        assert!(json.contains("api_call_id"));
        assert!(!json.contains("inbound_body"));
        assert!(
            !json.contains("\"model\":\"m\""),
            "no body content in summary"
        );
    }

    #[test]
    fn malformed_body_with_api_key_leaks_nothing_into_the_store() {
        // D1 R2 #1a end-to-end: a MALFORMED (non-JSON) inbound body carrying an
        // `api_key` must not persist ANY of the secret in the stored record.
        let store = DashboardFlowStore::new();
        let malformed = b"oops not json api_key=PLAINTEXTLEAK data:image/png;base64,IMGLEAK";
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            no_headers(),
            Some(cap(malformed)),
        );
        let record = store.detail("api_1").expect("record");
        let body = record.inbound_body.as_ref().expect("inbound stored");
        let text = String::from_utf8_lossy(body);
        assert!(!text.contains("PLAINTEXTLEAK"), "no api_key persisted");
        assert!(!text.contains("IMGLEAK"), "no raw image bytes persisted");
        assert!(
            text.starts_with("[redacted: unparseable body"),
            "fixed marker stored for a malformed body: {text}"
        );
    }
}
