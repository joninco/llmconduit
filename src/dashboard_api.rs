//! D13 — the `/dashboard/api/*` REST surface: the capstone that makes Phase 0's
//! stores (D1 FlowStore, D4 topology, D5 metrics/snapshots, D6 kill) reachable by
//! the SPA. Every handler takes `State(Arc<Gateway>)`; the routes register ONLY in
//! the `--with-debug-ui` block (http.rs), behind D7a's session auth + `no_store`.
//!
//! ## Wire contract (FROZEN — `dashboard-frontend/src/api/types.ts`)
//! The JSON these handlers emit must match the SPA's runtime validators
//! byte-for-byte (field names, nesting, per-domain `seq` cursors). The cursor-
//! bearing reads (`/flows`, `/flows/:id`, `/metrics`, `/topology`, `/snapshot`)
//! each carry their OWN domain's sequence — never a single global watermark
//! (AGENTS.md per-domain `{domain, seq}` rule). `/catalog` is the lone BARE array
//! (a static-ish read, not a mutating domain).
//!
//! ## Shape reuse (REST == WS)
//! `/metrics` returns a [`crate::dashboard_ws::MetricsSnapshot`] and `/topology` a
//! [`crate::dashboard_ws::TopologySnapshot`] — the SAME structs the `/dashboard/ws`
//! initial snapshot ships, so the REST body and the WS snapshot body are identical
//! shapes (the SPA decodes both with one validator). The flow rows + detail add a
//! `cost` roll-up the body-free [`SnapshotFlowSummary`] does not carry, so this
//! module defines the cost-bearing [`FlowRow`]/[`FlowDetailBody`] projections.
//!
//! ## Rates + cost (D13's job, not D5's)
//! The WS `window_tile` ships RAW window counts in the rate fields and `0.0` cost
//! (it has no window-seconds or price table). D13's REST view divides by the true
//! window seconds and prices every bucket via [`crate::config::Config::price_for`],
//! so `reqs_per_sec`/`tokens_per_sec`/`cost_per_min`/`cost_per_sec` are real rates.
//! `active_streams` is the live count of OPEN flows (the metrics rings don't track
//! liveness; the FlowStore does).

use crate::dashboard_flow::Attempt;
use crate::dashboard_flow::ClientSource;
use crate::dashboard_flow::FlowRecord;
use crate::dashboard_flow::FlowStatus;
use crate::dashboard_flow::FlowUsage;
use crate::dashboard_flow::PhaseTimings;
#[cfg(test)]
use crate::dashboard_ws::MetricWindow;
use crate::dashboard_ws::MetricsSnapshot;
use crate::dashboard_ws::ModelPrice;
use crate::dashboard_ws::SeqCursors;
use crate::dashboard_ws::TopologyEdge;
use crate::dashboard_ws::TopologyNode;
use crate::dashboard_ws::TopologySnapshot;
use crate::engine::Gateway;
use crate::metrics::MetricsView;
use crate::metrics::OverviewAggregate;
use crate::metrics::OverviewDataQuality;
use crate::metrics::OverviewFilter;
use crate::metrics::SnapshotHistoryMetadata;
#[cfg(test)]
use crate::metrics::StatusClass;
use crate::metrics::WindowReport;
use crate::monitor::DebugSnapshot;
use crate::monitor::DebugWsMessage;
use crate::upstream::ProviderHealthSnapshot;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Flow row + detail DTOs (the cost-bearing projections of a FlowRecord)
// ---------------------------------------------------------------------------

/// One row in the flow table (`GET /dashboard/api/flows`) — the body-free
/// [`SnapshotFlowSummary`](crate::dashboard_flow::SnapshotFlowSummary) fields PLUS
/// the D13 `cost` roll-up (usage × the served model's price). Mirrors the frozen
/// `FlowSummary` (types.ts) exactly: the `Option` fields use `skip_serializing_if`
/// to match the frontend's optional-key validators, EXCEPT `usage` (serialized as
/// `null` when absent — the frontend accepts absent/null/usage) and `cost`
/// (`null`-not-absent when the served model has no configured price).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FlowRow {
    /// Per-flow optimistic-concurrency version from the authoritative FlowStore.
    pub revision: u64,
    pub api_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub method: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requested: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_served: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_target: Option<String>,
    pub usage: Option<FlowUsage>,
    /// Calculation-only corrected usage; raw provider values remain in `usage`.
    pub normalized_usage: Option<FlowUsage>,
    pub usage_anomaly_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_route_limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_price_impact_usd: Option<f64>,
    pub status: FlowStatus,
    pub started_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    /// Gap 04 — the STABLE, NON-SECRET client attribution label (key-hash `key-<hex>`
    /// display id / configured caller-id / User-Agent fallback), projected from the
    /// flow record/summary. `skip_serializing_if` so an unattributed flow OMITS the key
    /// (absent ⇒ renders `—`, never a fabricated id). Additive/optional: the frontend
    /// ignores it until the client-attribution UI (gap 15). The raw key is never here —
    /// only the one-way hash prefix ever existed as a label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_label: Option<String>,
    /// Gap 04 — the [`ClientSource`] the label was derived from (so the weak
    /// `user_agent` fallback is distinguishable from a key-hash / configured-id). `None`
    /// (absent) exactly when `client_label` is `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_source: Option<ClientSource>,
    /// USD cost of the flow (usage × the served model's [`ModelPrice`]). `null`
    /// when no price is configured for `model_served` — never a fabricated zero.
    pub cost: Option<f64>,
    /// Gap 07 — the [`CostConfidence`] of `cost`: `confident` (priced + every billed
    /// class has a known rate), `estimated` (a class falls back to the default `0.0`
    /// cached rate / cached unreported), or `unavailable` (unpriced ⇒ `cost: null`).
    /// Always present so the frontend can label an `estimated` figure as such and
    /// distinguish an `unavailable` cost from a measured `$0.00`.
    pub cost_confidence: CostConfidence,
    /// Gap 10b — the gap-02 per-phase wall-clock timestamps, FLATTENED onto the row as
    /// sibling scalar fields (`ingress_ms`/`first_content_delta_ms`/…), mirroring the
    /// Rust `#[serde(flatten)] PhaseTimings` on `SnapshotFlowSummary`. The list row
    /// surfaces TTFT (`first_content_delta_ms`) per spec 10; the full bundle is carried
    /// (each field is `skip_serializing_if = None`, so an unmeasured phase is ABSENT, never
    /// `0`) so the inspector + the gap-16 overview read the same shape off either the
    /// row or the detail. Scalar metadata only — body-free (AGENTS.md snapshots-are-body-
    /// free invariant holds; these are `u128` epochs, not bodies).
    #[serde(flatten)]
    pub phases: PhaseTimings,
    /// Gap 10b — the gap-03 per-attempt failover trace projected onto the row (spec 11's
    /// stepper reads the whole list; spec 10 reads the served attempt's
    /// `first_upstream_byte_ms`). Each [`Attempt`] is body-free scalar provenance + bounded
    /// taxonomic codes — never a raw upstream error body. `skip_serializing_if =
    /// Vec::is_empty` so a flow with no recorded attempt OMITS the key (the frontend's
    /// `attempts?` is absent), matching the body-free summary's wire shape.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    /// Gap 10b — the gap-03 flow-level wire time-to-first-byte (the served attempt's first
    /// on-wire chunk). Distinct from `first_content_delta_ms` (the first content delta to
    /// the CLIENT). `None` ⇒ absent ⇒ renders `—` downstream, NEVER `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_upstream_byte_ms: Option<u128>,
}

impl FlowRow {
    /// Build a row from a live [`FlowRecord`], pricing it via the gateway's price
    /// table keyed by the SERVED model (the backend that actually answered).
    pub(crate) fn from_record(record: &FlowRecord, _gateway: &Gateway) -> Self {
        let normalized = record.usage.map(crate::dashboard_flow::normalize_usage);
        let cost = record.terminal_cost_usd;
        let cost_confidence = record.terminal_cost_confidence.into();
        Self {
            revision: record.revision,
            api_call_id: record.api_call_id.clone(),
            response_id: record.response_id.clone(),
            method: record.method.clone(),
            uri: record.uri.clone(),
            model_requested: record.model_requested.clone(),
            model_served: record.model_served.clone(),
            upstream_target: record.upstream_target.clone(),
            usage: record.usage,
            normalized_usage: normalized.map(|value| value.usage),
            usage_anomaly_count: normalized.map_or(0, |value| value.anomaly_count),
            effective_route_limit: record.effective_route_limit,
            cache_price_impact_usd: record.cache_price_impact_usd,
            status: record.status,
            started_ms: record.started_ms,
            finished_ms: record.finished_ms,
            elapsed_ms: record.elapsed_ms,
            terminal_reason: record.terminal_reason.clone(),
            // Gap 04: thread the attribution (label + source) onto the row — body-free
            // scalar metadata; the raw key is never here (only the one-way hash prefix).
            client_label: record.client_label.clone(),
            client_source: record.client_source,
            cost,
            cost_confidence,
            // Gap 10b: project the gap-02 phases + gap-03 attempts/wire-TTFB from the live
            // record onto the row. `PhaseTimings` is `Copy`; the attempts vec is cloned
            // (body-free scalar provenance). No recompute — just thread the already-measured
            // spine fields through so the gap-10/11/16 surfaces light up against the row.
            phases: record.phases,
            attempts: record.attempts.clone(),
            first_upstream_byte_ms: record.first_upstream_byte_ms,
        }
    }

    /// Build a row from a body-free snapshot summary (the `/snapshot` summaries AND the
    /// initial `/dashboard/ws` snapshot message — the SPA's `isSnapshotFrame` requires the
    /// gap-07 `cost_confidence` on every row, so the WS snapshot must project through THIS,
    /// never serialize raw `SnapshotFlowSummary`s), pricing it the same way. The snapshot
    /// summary has no live `FlowRecord`, so this prices off its own `model_served` + `usage`.
    pub(crate) fn from_summary(
        summary: &crate::dashboard_flow::SnapshotFlowSummary,
        _gateway: &Gateway,
    ) -> Self {
        let normalized = summary.usage.map(crate::dashboard_flow::normalize_usage);
        let cost = summary.terminal_cost_usd;
        let cost_confidence = summary.terminal_cost_confidence.into();
        Self {
            revision: summary.revision,
            api_call_id: summary.api_call_id.clone(),
            response_id: summary.response_id.clone(),
            method: summary.method.clone(),
            uri: summary.uri.clone(),
            model_requested: summary.model_requested.clone(),
            model_served: summary.model_served.clone(),
            upstream_target: summary.upstream_target.clone(),
            usage: summary.usage,
            normalized_usage: normalized.map(|value| value.usage),
            usage_anomaly_count: normalized.map_or(0, |value| value.anomaly_count),
            effective_route_limit: summary.effective_route_limit,
            cache_price_impact_usd: summary.cache_price_impact_usd,
            status: summary.status,
            started_ms: summary.started_ms,
            finished_ms: summary.finished_ms,
            elapsed_ms: summary.elapsed_ms,
            terminal_reason: summary.terminal_reason.clone(),
            // Gap 04: same attribution projection from the body-free snapshot summary.
            client_label: summary.client_label.clone(),
            client_source: summary.client_source,
            cost,
            cost_confidence,
            // Gap 10b: the body-free `SnapshotFlowSummary` ALREADY carries the gap-02 phases
            // + gap-03 attempts/wire-TTFB (specs 02/03) — thread them straight onto the row
            // so a `/snapshot` cut's rows carry the same measured spine as the live `/flows`
            // rows. No recompute.
            phases: summary.phases,
            attempts: summary.attempts.clone(),
            first_upstream_byte_ms: summary.first_upstream_byte_ms,
        }
    }
}

/// `GET /dashboard/api/flows` — the paged flow list + total + the FlowStore
/// domain cursor. Matches the frozen `FlowsResponse`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FlowsResponse {
    pub flows: Vec<FlowRow>,
    /// Total rows AFTER filtering but BEFORE paging (so the SPA can page).
    pub total: usize,
    pub flow_seq: u64,
}

/// Query params for `GET /dashboard/api/flows`. All optional; `status`/`model`/
/// `upstream` filter, `page`/`limit` page (1-based page; absent ⇒ all rows).
#[derive(Debug, Default, Deserialize)]
pub struct FlowsQuery {
    pub cut_id: Option<u64>,
    pub status: Option<String>,
    pub model: Option<String>,
    pub upstream: Option<String>,
    pub page: Option<usize>,
    pub limit: Option<usize>,
}

/// One streamed delta replayed into the inspector (from the MonitorHub snapshot,
/// filtered by the flow's `response_id`). Mirrors the frozen `FlowDelta`:
/// `{sequence, kind, payload?, ts_ms?}`. `payload` is the heterogeneous delta body
/// (a segment text, an event summary, a status, …); the SPA narrows at the use
/// site. `sequence` is a per-flow ordinal (the replay order), NOT a domain cursor.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FlowDelta {
    pub sequence: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u128>,
}

/// Gap 05 — the captured upstream RESPONSE/ERROR body projected onto the live
/// [`FlowDetailBody`] (the `/dashboard/api/flows/:id` detail path only — NEVER the
/// body-free list rows or snapshot summaries). `body` is the redacted, capped bytes
/// parsed back to a JSON `Value` (or a string `Value` for a non-JSON / `[redacted:
/// unparseable body …]` marker, mirroring the other captured bodies); `truncated`
/// flags that the cap truncated the raw body, so the dashboard shows a PARTIAL body
/// honestly rather than presenting it as complete. Present ONLY when capture is armed
/// AND the turn produced an upstream error body (the live record's
/// [`upstream_response`](crate::dashboard_flow::FlowRecord::upstream_response) is
/// `Some`); absent otherwise (capture off / no body / evicted by the byte quota).
/// Derives `Deserialize` (alongside `Serialize`) so the new wire field round-trips in a
/// test (AGENTS.md: no new wire field without a deserialize-then-serialize proof) — the
/// enclosing [`FlowDetailBody`] stays serialize-only (it is only ever a response), so the
/// round-trip is pinned on THIS self-contained sub-DTO. Consumed by gap 14 (failure
/// taxonomy).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FlowUpstreamResponse {
    /// The redacted, capped upstream response/error body, parsed to JSON (or a string
    /// `Value` for a non-JSON / marker body). An EMPTY captured body parses to a string
    /// `""` — distinct from the whole field being ABSENT (capture off / no body).
    pub body: serde_json::Value,
    /// Whether the cap truncated the raw body (the retained bytes are a PREFIX). The
    /// dashboard must flag a truncated body rather than presenting it as complete.
    pub truncated: bool,
}

/// `GET /dashboard/api/flows/:id` — the 3-pane inspector body. Carries the summary
/// fields, the three captured on-wire bodies (inbound, normalized, upstream —
/// ABSENT, not error, when the summary-byte quota evicted them), the inbound
/// headers, the replayed deltas, usage, the terminal, and cost. Mirrors the frozen
/// `FlowDetail` (`:id == api_call_id`). The three bodies, headers, and deltas are
/// the additive detail fields over a [`FlowRow`].
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FlowDetailBody {
    pub detail_source: FlowDetailSource,
    pub flow_seq: u64,
    pub revision: u64,
    pub api_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// The captured INBOUND request body (parsed JSON). Absent when evicted by the
    /// D1 summary-byte quota; parsed back to a `Value` so the SPA renders the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbound_body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbound_headers: Option<BTreeMap<String, String>>,
    /// The captured CANONICAL/normalized body (D2), parsed. Absent when evicted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized: Option<serde_json::Value>,
    /// The captured UPSTREAM on-wire chat body (D2), parsed. Absent when evicted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_body: Option<serde_json::Value>,
    /// Gap 05 — the captured upstream RESPONSE/ERROR body (parsed) + its `truncated`
    /// flag, projected from the live record's
    /// [`upstream_response`](crate::dashboard_flow::FlowRecord::upstream_response).
    /// Present ONLY on this LIVE detail path (the diagnostic operator endpoint) when
    /// response capture is armed AND the turn produced an upstream error body; absent
    /// otherwise (capture off / no body / evicted by the byte quota). DELIBERATELY kept
    /// OFF the body-free [`FlowRow`] list rows and
    /// [`SnapshotFlowSummary`](crate::dashboard_flow::SnapshotFlowSummary) (the 135 GiB
    /// body-free-snapshot invariant). `skip_serializing_if` so an absent body OMITS the
    /// key. Consumed by gap 14 (failure taxonomy); the React app ignores it until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_response: Option<FlowUpstreamResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requested: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_served: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_target: Option<String>,
    pub usage: Option<FlowUsage>,
    pub normalized_usage: Option<FlowUsage>,
    pub usage_anomaly_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_route_limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_price_impact_usd: Option<f64>,
    pub status: FlowStatus,
    /// Monitor-domain watermark of the single transcript snapshot used to build
    /// `deltas`. [`FlowDelta::sequence`] remains a per-flow replay ordinal and is
    /// intentionally NOT comparable to live `DebugUpdate.sequence` values. The SPA
    /// appends only live monitor segments whose monitor sequence is strictly greater
    /// than this watermark, so the replay/live seam neither duplicates nor drops
    /// repeated or same-millisecond content.
    pub deltas_through_monitor_seq: u64,
    pub deltas: Vec<FlowDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    pub started_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    pub cost: Option<f64>,
    /// Gap 07 — the [`CostConfidence`] of `cost` (confident/estimated/unavailable),
    /// mirroring the flow-row tag so the inspector labels an `estimated` figure.
    pub cost_confidence: CostConfidence,
    /// Gap 10b — the gap-02 per-phase wall-clock timestamps, FLATTENED onto the detail
    /// body (mirrors the `#[serde(flatten)] PhaseTimings` on `SnapshotFlowSummary`). The
    /// inspector's gap-10 latency waterfall reads the FULL phase set here (ingress →
    /// normalization → routing → first_content_delta → stream_end → finalize). Each field
    /// is `skip_serializing_if = None`, so an unmeasured phase is ABSENT, never `0`
    /// (don't-lie-with-zeros). Scalar `u128` epochs — not bodies.
    #[serde(flatten)]
    pub phases: PhaseTimings,
    /// Gap 10b — the gap-03 per-attempt failover trace, projected onto the detail body
    /// (spec 11's inspector stepper reads the whole list; spec 10 reads the served
    /// attempt's `first_upstream_byte_ms` to enrich the upstream-wait segment). Each
    /// [`Attempt`] is body-free scalar provenance + bounded taxonomic codes — never a raw
    /// upstream error body. `skip_serializing_if = Vec::is_empty` so a flow with no recorded
    /// attempt OMITS the key (matches the frontend's optional `attempts?`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    /// Gap 10b — the gap-03 flow-level wire time-to-first-byte (the served attempt's first
    /// on-wire chunk). Distinct from `first_content_delta_ms` (first content delta to the
    /// CLIENT). `None` ⇒ absent ⇒ renders `—`, NEVER `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_upstream_byte_ms: Option<u128>,
    /// Every body section available from the durable per-turn artifact. This includes
    /// successful upstream/served responses that were intentionally never retained in
    /// the live FlowStore. Sections load lazily at the HTTP request, not into snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captured_sections: Vec<CapturedSection>,
}

#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowDetailSource {
    Live,
    Durable,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CapturedSection {
    pub name: String,
    pub bytes: u64,
    pub partial: bool,
    pub encoding: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FlowDetailQuery {
    pub cut_id: Option<u64>,
}

/// One catalog entry (`GET /dashboard/api/catalog` — a BARE array, no cursor).
/// `{id, context_limit}` where `context_limit` is the per-model max-context window
/// surfaced from the upstream `/v1/models` snapshot (gap 06).
///
/// NULLABLE end-to-end (gap 06 contract migration): `context_limit` is
/// `Option<i64>`, serialized as `null` when the upstream advertises no window —
/// distinct from a real `0`. Previously this DTO collapsed a missing window to a
/// non-null `0` (`unwrap_or(0)`), which lies-with-zeros: a `0` ceiling reads as
/// garbage/infinite utilization downstream (spec 09's context-window gauge).
/// `measured` when advertised; `unavailable`/`None` when the upstream omits it.
/// The frontend renders `—` on `null`, NEVER `0`. Derives `Deserialize` alongside
/// `Serialize` so the changed wire field round-trips in a test (AGENTS.md: no
/// changed wire field without a deserialize-then-serialize proof).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CatalogEntry {
    pub id: String,
    /// The per-model max-context window (tokens), or `null`/absent when the
    /// upstream advertises none. `skip_serializing_if` so an unavailable window
    /// OMITS the key rather than emitting `null` — either is honest (the frontend
    /// type is `number | null` with the field optional); both are distinct from a
    /// real `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<i64>,
}

/// `GET /dashboard/api/snapshot?at=<unix_ms>` — a body-free frozen cut. Mirrors
/// the frozen `SnapshotResponse`: the per-domain `cursors`, the cut instant, the
/// body-free flow summaries (priced), and the metrics/topology cuts reshaped into
/// their REST bodies (`null` when the cut is empty for that domain).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SnapshotResponse {
    /// Stable durable-cut identifier. Equal to the coordinated cut's epoch-ms stamp;
    /// absent only when no historical cut exists yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut_id: Option<u64>,
    pub cursors: SeqCursors,
    pub at_ms: u128,
    pub summaries: Vec<FlowRow>,
    pub metrics: Option<MetricsSnapshot>,
    pub topology: Option<TopologySnapshot>,
    /// Bounds and memory use of the retained historical-cut ring.
    pub history: SnapshotHistoryMetadata,
    /// Whether this selected cut dropped its oldest flow summaries to fit the quota.
    pub flow_summaries_truncated: bool,
    /// Persisted monitor messages through this cut. Empty for legacy in-memory-only
    /// cuts; durable cuts use them for historical flow timelines and Theater replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_messages: Vec<DebugWsMessage>,
}

/// Query param for `GET /dashboard/api/snapshot` — the wall-clock instant (unix
/// ms) to time-travel to. Absent ⇒ the latest cut. Typed `u64` (NOT `u128`): the
/// axum/serde QUERY deserializer does not support `u128`, and unix-ms fits `u64`
/// for ~580 million years; the handler widens it to the `u128` `snapshot_at` key.
#[derive(Debug, Default, Deserialize)]
pub struct SnapshotQuery {
    pub at: Option<u64>,
    pub cut_id: Option<u64>,
}

/// Exact server-side Overview window. Kept to the three MetricsLayer ring spans so a
/// live and historical request always selects the same retained population.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum OverviewWindow {
    #[default]
    M1,
    M5,
    H1,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverviewQuery {
    #[serde(default)]
    pub window: OverviewWindow,
    pub at: Option<u64>,
    pub cut_id: Option<u64>,
    pub status: Option<String>,
    pub model: Option<String>,
    pub upstream: Option<String>,
    pub client: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct OverviewScope {
    pub window: OverviewWindow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut_id: Option<u64>,
    pub requested_at_ms: Option<u128>,
    pub selected_at_ms: Option<u128>,
    pub status: Option<String>,
    pub model: Option<String>,
    pub upstream: Option<String>,
    pub client: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoricalCutQuery {
    pub cut_id: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HistoryQuery {
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HistoryPoint {
    pub cut_id: u64,
    pub at_ms: u128,
    pub cursors: SeqCursors,
    pub instant: crate::metrics::InstantMetricSample,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_throughput: Option<crate::backend_metrics::EngineThroughputSample>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HistoryResponse {
    pub oldest_at_ms: Option<u128>,
    pub newest_at_ms: Option<u128>,
    pub retained_cuts: usize,
    pub database_bytes: usize,
    pub dropped_writes: u64,
    pub points: Vec<HistoryPoint>,
}

/// `GET /dashboard/api/overview`: an immutable exact-window rollup. The aggregate is
/// flattened so the wire reads naturally (`totals`, `served_models`, `cost_series`, …)
/// while the calculation remains owned by MetricsLayer for both live and historical
/// cuts.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct OverviewResponse {
    pub generated_at_ms: u128,
    pub metrics_seq: u64,
    pub scope: OverviewScope,
    #[serde(flatten)]
    pub aggregate: OverviewAggregate,
}

// ---------------------------------------------------------------------------
// Cost + rate helpers (pure — unit-testable without the HTTP stack)
// ---------------------------------------------------------------------------

/// The USD cost of one flow's `usage` at `model`'s configured price (`None` when
/// the model has no price, so the row reports `cost: null`, never a fake zero).
///
/// Billing model (the standard prompt/cached/completion split): the `cached`
/// prompt tokens bill at the cache-read rate and the REMAINING prompt tokens at
/// the input rate, so `cached` is treated as a subset of `prompt` (clamped at 0 so
/// a transient `cached > prompt` never yields a negative charge). Reasoning tokens
/// are part of the completion the provider bills, so they are NOT charged
/// separately (the `total`/`completion` already account for them upstream).
///
/// The result is run through [`finite`] so a degenerate configured price (an
/// absurd magnitude that overflows to ±∞, or a serde-loaded NaN) can never poison
/// the JSON: `serde_json::to_vec` ERRORS on a non-finite float, which would 500 the
/// whole `/flows` (or snapshot) read. A non-finite cost collapses to `0.0` instead.
pub fn cost_for_usage(usage: FlowUsage, price: ModelPrice) -> f64 {
    let usage = crate::dashboard_flow::normalize_usage(usage).usage;
    // Gap 07: an UNREPORTED cached count (`None`) bills as 0 cached tokens — the whole
    // prompt then bills at the input rate (the confidence tier flags this as `estimated`
    // when no cached rate is configured; the dollar figure stays a best-effort number).
    let cached = usage.cached.unwrap_or(0).max(0) as f64;
    let prompt = usage.prompt.max(0) as f64;
    let completion = usage.completion.max(0) as f64;
    // Uncached prompt = prompt - cached (never negative).
    let uncached_prompt = (prompt - cached).max(0.0);
    finite(
        (uncached_prompt / 1000.0) * price.input_per_1k
            + (cached / 1000.0) * price.cached_per_1k
            + (completion / 1000.0) * price.output_per_1k,
    )
}

/// Gap 07 — the CONFIDENCE tier of a flow's `cost`, so an operator can tell a trusted
/// figure from a best-effort estimate from an honest gap. Emitted alongside `cost` on
/// every flow row + detail (and aggregated onto the metrics windows). Serializes
/// snake_case to mirror the data-quality vocabulary the frontend already uses
/// (`measured`/`derived`/`estimated`/`unavailable`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CostConfidence {
    /// The model is priced AND every billed token class has a known rate: the prompt
    /// (input) + completion (output) rates always exist when priced, and cached tokens
    /// either were reported as `0` (nothing billed at the cache rate) OR the model has
    /// a CONFIGURED cached rate. The dollar figure is trustworthy.
    Confident,
    /// The model is priced but a billed class would fall back to an APPROXIMATE rate:
    /// cached tokens were reported `> 0` (or are UNAVAILABLE) while the model has NO
    /// configured cached rate, so those tokens silently bill at the default `0.0`
    /// (an undercount). The figure is a best-effort ESTIMATE — surfaced as such.
    Estimated,
    /// No price is configured for the served model: there is NO cost (the row reports
    /// `cost: null`), so the figure is UNAVAILABLE — never a fabricated `0`. The
    /// DEFAULT: a window/flow with nothing priced makes no confident claim.
    #[default]
    Unavailable,
}

impl From<crate::dashboard_flow::TerminalCostConfidence> for CostConfidence {
    fn from(value: crate::dashboard_flow::TerminalCostConfidence) -> Self {
        match value {
            crate::dashboard_flow::TerminalCostConfidence::Confident => Self::Confident,
            crate::dashboard_flow::TerminalCostConfidence::Estimated => Self::Estimated,
            crate::dashboard_flow::TerminalCostConfidence::Unavailable => Self::Unavailable,
        }
    }
}

/// Gap 07 — classify a flow's cost confidence from its served model's price PRESENCE
/// + the cached-token report. The rules (spec 07 acceptance):
/// - unpriced model ⇒ [`CostConfidence::Unavailable`] (cost is `None`, never `0`).
/// - priced AND (`cached == Some(0)` OR `cached_price_configured`) ⇒ `Confident`
///   (a reported `cached = 0` bills nothing at the cache rate; a configured rate
///   prices it honestly).
/// - priced AND (`cached == Some(n>0)` OR `cached == None`) AND NOT
///   `cached_price_configured` ⇒ `Estimated` (those cached tokens would bill at the
///   default `0.0` — an undercount, so NOT a silently-`confident` total).
#[cfg(test)]
fn cost_confidence(price: Option<ModelPrice>, usage: Option<FlowUsage>) -> CostConfidence {
    let Some(price) = price else {
        return CostConfidence::Unavailable;
    };
    // A priced flow with no usage at all still has a (zero-token) cost; with no cached
    // tokens billed it is trivially confident.
    let cached = usage.and_then(|usage| usage.cached);
    match cached {
        // Reported zero cache tokens: nothing bills at the cache rate ⇒ confident
        // regardless of whether a cached rate is configured.
        Some(0) => CostConfidence::Confident,
        // Reported >0 cached, OR UNAVAILABLE (None) cached: confident ONLY if a cached
        // rate is configured; otherwise those tokens fall back to the default 0.0.
        Some(_) | None => {
            if price.cached_price_configured {
                CostConfidence::Confident
            } else {
                CostConfidence::Estimated
            }
        }
    }
}

/// A JSON-safe float: the value if finite, else `0.0`. `serde_json` REFUSES to
/// serialize NaN/±∞ (it errors), so every float that reaches a response body — cost
/// roll-ups, per-second rates — is passed through this so a degenerate input can
/// never turn a read into a 500. (The inputs are operator-configured prices, not
/// attacker data, but a typo'd 1e308 rate should degrade gracefully, not 500.)
fn finite(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

/// Canonical token throughput is prompt + completion. Cached and reasoning are
/// diagnostic subsets and must never be added a second time.
#[cfg(test)]
fn window_total_tokens(report: &WindowReport) -> i64 {
    report
        .buckets
        .values()
        .map(|counts| {
            counts
                .prompt_tokens
                .saturating_add(counts.completion_tokens)
        })
        .fold(0i64, i64::saturating_add)
}

/// Exact-then-case-insensitive price lookup over a raw price map, mirroring
/// [`crate::config::Config::price_for`] (used where only the map is in hand, e.g.
/// pricing a snapshot cut's metrics buckets).
#[cfg(test)]
fn price_lookup(prices: &HashMap<String, ModelPrice>, model: &str) -> Option<ModelPrice> {
    prices.get(model).copied().or_else(|| {
        prices
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(model))
            .map(|(_, price)| *price)
    })
}

/// Collapse one [`WindowReport`] into a flat REST [`MetricWindow`] tile over
/// `window_secs` seconds: TRUE per-second request/token rates, the error %, the
/// p50/p95/p99 latency, and the per-minute cost (this is D13's job — the WS
/// `window_tile` ships raw counts + `0.0` cost). `active_streams` is the live open-
/// flow count (passed in; the rings don't track liveness). An empty window reports
/// all-zero rates (finite — the contract requires finite numbers).
#[cfg(test)]
fn rest_window_tile(
    report: &WindowReport,
    window_secs: f64,
    active_streams: u64,
    _prices: &HashMap<String, ModelPrice>,
) -> MetricWindow {
    let percentiles = report.percentiles();
    let total = report.total_count();
    let count_status = |status| {
        report
            .buckets
            .iter()
            .filter(|(key, _)| key.status == status)
            .map(|(_, counts)| counts.count)
            .fold(0u64, u64::saturating_add)
    };
    let successes = count_status(StatusClass::Success);
    let failures = count_status(StatusClass::Error);
    let cancellations = count_status(StatusClass::Cancelled);
    let denominator = report.observed_seconds.max(1).min(window_secs as u64) as f64;
    let usage_samples = report.usage_sample_count();
    let priced_samples = report.terminal_priced_samples();
    let percentage = |count: u64| {
        if total == 0 {
            0.0
        } else {
            count as f64 / total as f64 * 100.0
        }
    };
    let latency_value = |minimum: u64, value: f64| (total >= minimum).then_some(finite(value));
    let latency_quality = if total == 0 {
        crate::dashboard_ws::MetricQuality::Unavailable
    } else if report.histogram.is_partial() {
        crate::dashboard_ws::MetricQuality::Partial
    } else {
        crate::dashboard_ws::MetricQuality::Measured
    };
    let cost_confidence = if priced_samples == 0 {
        CostConfidence::Unavailable
    } else if report.terminal_cost_is_estimated() {
        CostConfidence::Estimated
    } else {
        CostConfidence::Confident
    };
    MetricWindow {
        window_seconds: window_secs as u64,
        observed_seconds: report.observed_seconds.min(window_secs as u64),
        warm: report.observed_seconds >= window_secs as u64,
        accepted_requests: report.accepted_requests,
        accepted_per_sec: finite(report.accepted_requests as f64 / denominator),
        terminal_requests: total,
        terminal_per_sec: finite(total as f64 / denominator),
        successes,
        failures,
        failure_pct: finite(percentage(failures)),
        cancellations,
        cancellation_pct: finite(percentage(cancellations)),
        active_streams_now: active_streams,
        latency_samples: total,
        p50_ms: latency_value(2, percentiles.p50),
        p95_ms: latency_value(20, percentiles.p95),
        p99_ms: latency_value(100, percentiles.p99),
        quantile_method: crate::dashboard_ws::QuantileMethod::LogHistogramNearestRank,
        max_relative_error: crate::metrics::HISTOGRAM_MAX_RELATIVE_ERROR,
        latency_overflow_count: report.histogram.overflow_count(),
        latency_quality,
        usage_samples,
        reported_tokens_per_sec: (usage_samples > 0)
            .then_some(finite(window_total_tokens(report) as f64 / denominator)),
        usage_anomaly_count: report.usage_anomaly_count(),
        priced_samples,
        cost_per_min: (priced_samples > 0)
            .then_some(finite(report.terminal_cost_usd() / (denominator / 60.0))),
        cost_confidence,
    }
}

/// Build the `/metrics`-shaped [`MetricsSnapshot`] from one process-published
/// reset-on-publish interval, its retained idle sample, and the preferred engine
/// token-rate sample. Shared by REST, snapshots, and the WebSocket projection so
/// all three surfaces expose one byte-shape and one source-selection seam.
pub fn metrics_body(
    instant: &crate::metrics::InstantMetricSample,
    last_activity: Option<&crate::metrics::LastActivitySample>,
    engine_throughput: Option<&crate::backend_metrics::EngineThroughputSample>,
    generated_at_ms: u128,
    metrics_seq: u64,
) -> MetricsSnapshot {
    MetricsSnapshot {
        metrics_seq,
        generated_at_ms,
        instant: instant.clone(),
        last_activity: last_activity.cloned(),
        engine_throughput: engine_throughput.cloned(),
    }
}

/// Build the full `/topology`-shaped [`TopologySnapshot`] body from a D4
/// [`ProviderHealthSnapshot`] + the price table + the live `m1` metrics window
/// (for the edge rate roll-ups). Each provider becomes a node; one gateway→provider
/// edge carries that provider's per-second request/token/cost rates aggregated from
/// the m1 window keyed by `BucketKey.upstream`. Shared by `/topology` AND the
/// `/snapshot` topology reshape.
pub fn topology_body(
    snapshot: &ProviderHealthSnapshot,
    prices: &HashMap<String, ModelPrice>,
    window_1m: &WindowReport,
    backend_metrics: &crate::backend_metrics::BackendMetricsSnapshot,
) -> TopologySnapshot {
    // Gap 12: each node carries its per-provider latency/error metrics from the m1
    // window (aggregated off the evict-safe per-attempt trace), looked up by provider id
    // — absent when the provider had no in-window samples (don't-lie-with-zeros). Same
    // m1 window the edge rates below roll up, so the tiles + edges share one metrics cut.
    let nodes: Vec<TopologyNode> = snapshot
        .providers
        .iter()
        .map(|provider| {
            TopologyNode::from_health_with_metrics(provider, window_1m, backend_metrics)
        })
        .collect();
    let edges: Vec<TopologyEdge> = snapshot
        .providers
        .iter()
        .map(|provider| {
            let (attempts, terminals, tokens, cost) = upstream_edge_rates(&provider.id, window_1m);
            TopologyEdge {
                from: "gateway".to_string(),
                to: provider.id.clone(),
                attempts_per_sec: attempts,
                terminal_flows_per_sec: terminals,
                reported_tokens_per_sec: tokens,
                terminal_cost_per_sec: cost,
            }
        })
        .collect();
    TopologySnapshot {
        topology_seq: snapshot.version,
        nodes,
        edges,
        price_table: prices
            .iter()
            .map(|(model, price)| (model.clone(), *price))
            .collect(),
    }
}

/// The `(reqs_per_sec, tokens_per_sec, cost_per_sec)` rates for one upstream over
/// the `m1` window: every bucket whose `BucketKey.upstream` matches `upstream_id`,
/// summed and divided by the 60 s window. Cost prices each bucket by its OWN served
/// model. Used to enrich the gateway→provider topology edges.
fn upstream_edge_rates(
    upstream_id: &str,
    window_1m: &WindowReport,
) -> (f64, f64, Option<f64>, Option<f64>) {
    let mut reqs = 0u64;
    let mut tokens = 0i64;
    let mut cost = 0.0f64;
    let mut usage_samples = 0u64;
    let mut priced_samples = 0u64;
    for (key, counts) in &window_1m.buckets {
        if key.upstream != upstream_id {
            continue;
        }
        reqs = reqs.saturating_add(counts.count);
        tokens = tokens
            .saturating_add(counts.prompt_tokens)
            .saturating_add(counts.completion_tokens);
        usage_samples = usage_samples.saturating_add(counts.usage_samples);
        priced_samples = priced_samples.saturating_add(counts.priced_samples);
        cost += counts.terminal_cost_usd;
    }
    let denominator = window_1m.observed_seconds.clamp(1, 60) as f64;
    let attempts = window_1m
        .provider_latency(upstream_id)
        .map_or(0, |latency| latency.samples);
    (
        finite(attempts as f64 / denominator),
        finite(reqs as f64 / denominator),
        (usage_samples > 0).then_some(finite(tokens as f64 / denominator)),
        (priced_samples > 0).then_some(finite(cost / denominator)),
    )
}

// ---------------------------------------------------------------------------
// Delta replay (MonitorHub snapshot, filtered by response_id)
// ---------------------------------------------------------------------------

/// Replay the streamed deltas for a flow from the MonitorHub snapshot, filtered by
/// the flow's `response_id` (the monitor keys transcript messages by the engine's
/// response id, NOT the `api_call_id`). Returns an empty `Vec` when the flow has no
/// linked `response_id` yet (nothing to correlate). The returned watermark is still
/// the snapshot's `last_sequence`, so an empty replay cannot accidentally append
/// pre-snapshot live history. Each matching `SegmentAppend`/
/// `EventAppend`/`RequestStatus` becomes a [`FlowDelta`] in monitor order, with a
/// per-flow `sequence` ordinal. `RequestUpsert`/`Usage`/`RequestRemove`/`Hello`/
/// `SnapshotDone` are not per-token deltas (the row already carries usage/status),
/// so they are skipped — the inspector wants the segment/event timeline. Taking the
/// already-captured [`DebugSnapshot`] makes the replay and its monitor watermark one
/// indivisible read; the handler must never fetch them separately.
fn replay_deltas(response_id: Option<&str>, snapshot: DebugSnapshot) -> (Vec<FlowDelta>, u64) {
    let through_monitor_seq = snapshot.last_sequence;
    let Some(response_id) = response_id else {
        return (Vec::new(), through_monitor_seq);
    };
    let mut deltas = Vec::new();
    let mut sequence = 0u64;
    for message in &snapshot.messages {
        let delta = match message {
            DebugWsMessage::SegmentAppend {
                response_id: rid,
                segment,
            } if rid == response_id => FlowDelta {
                sequence,
                kind: format!("segment.{}", segment_kind_str(segment.kind)),
                payload: Some(serde_json::json!({ "text": segment.text })),
                ts_ms: Some(segment.timestamp_ms),
            },
            DebugWsMessage::EventAppend {
                response_id: rid,
                event,
            } if rid == response_id => FlowDelta {
                sequence,
                kind: format!("event.{}", event.kind),
                payload: Some(serde_json::json!({
                    "summary": event.summary,
                    "payload_preview": event.payload_preview,
                })),
                ts_ms: Some(event.timestamp_ms),
            },
            DebugWsMessage::RequestStatus {
                response_id: rid,
                status,
                completed_at_ms,
                error,
            } if rid == response_id => FlowDelta {
                sequence,
                kind: "status".to_string(),
                payload: Some(serde_json::json!({
                    "status": request_status_str(*status),
                    "error": error,
                })),
                ts_ms: *completed_at_ms,
            },
            _ => continue,
        };
        deltas.push(delta);
        sequence += 1;
    }
    (deltas, through_monitor_seq)
}

/// The snake_case wire string for a [`crate::monitor::DebugSegmentKind`] (matches
/// the frozen `DebugSegmentKind` union: output/reasoning/tool).
fn segment_kind_str(kind: crate::monitor::DebugSegmentKind) -> &'static str {
    match kind {
        crate::monitor::DebugSegmentKind::Output => "output",
        crate::monitor::DebugSegmentKind::Reasoning => "reasoning",
        crate::monitor::DebugSegmentKind::Tool => "tool",
    }
}

/// The snake_case wire string for a [`crate::monitor::DebugRequestStatus`].
fn request_status_str(status: crate::monitor::DebugRequestStatus) -> &'static str {
    match status {
        crate::monitor::DebugRequestStatus::Running => "running",
        crate::monitor::DebugRequestStatus::Completed => "completed",
        crate::monitor::DebugRequestStatus::Failed => "failed",
    }
}

/// Parse a captured (already-redacted + capped JSON) body `Arc<[u8]>` back into a
/// `serde_json::Value` for the inspector. A body that does not parse as JSON (a
/// truncated capture, a non-JSON payload) falls back to a JSON string of the
/// lossy UTF-8 so the field is still present + renderable rather than dropped.
fn parse_captured_body(body: &Arc<[u8]>) -> serde_json::Value {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => value,
        Err(_) => serde_json::Value::String(String::from_utf8_lossy(body).into_owned()),
    }
}

async fn durable_sections(
    history: &crate::dashboard_history::DashboardHistory,
    api_call_id: &str,
) -> Vec<CapturedSection> {
    let Some(path) = history.artifact_path(api_call_id).await else {
        return Vec::new();
    };
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(path).ok()?;
        let artifact: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let sections = artifact.get("sections")?.as_object()?;
        let order = [
            "inbound_request",
            "normalized_request",
            "upstream_request",
            "upstream_response",
            "served_response",
        ];
        let mut captured = Vec::new();
        for name in order {
            let Some(section) = sections.get(name).and_then(serde_json::Value::as_object) else {
                continue;
            };
            captured.push(CapturedSection {
                name: name.to_string(),
                bytes: section
                    .get("bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                partial: section
                    .get("partial")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
                encoding: section
                    .get("encoding")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                content: section
                    .get("content")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            });
        }
        Some(captured)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Handlers (each `State(Arc<Gateway>)`; no-store + auth applied by the route layer)
// ---------------------------------------------------------------------------

/// `GET /dashboard/api/flows?status=&model=&upstream=&page=&limit=` — the flow
/// table. Lists newest-first from the FlowStore (D1), filters by status/model/
/// upstream, pages, and stamps the FlowStore domain `flow_seq`. Each row carries
/// its `cost` (usage × served-model price).
pub async fn dashboard_flows(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<FlowsQuery>,
) -> Response {
    if let Some(cut_id) = query.cut_id {
        let Some(cut) = gateway.dashboard_history().cut_by_id(cut_id).await else {
            return json_no_store(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": {"code": "historical_cut_not_found", "cut_id": cut_id}}),
            );
        };
        let status_filter = query.status.as_deref().and_then(parse_status_filter);
        let model_filter = query.model.as_deref().map(str::to_ascii_lowercase);
        let upstream_filter = query.upstream.as_deref().map(str::to_ascii_lowercase);
        let mut rows: Vec<FlowRow> = gateway
            .dashboard_history()
            .flow_summaries_as_of(cut_id)
            .await
            .iter()
            .map(|summary| FlowRow::from_summary(summary, gateway.as_ref()))
            .filter(|row| {
                status_filter.is_none_or(|status| row.status == status)
                    && model_filter.as_ref().is_none_or(|wanted| {
                        row.model_requested
                            .as_deref()
                            .into_iter()
                            .chain(row.model_served.as_deref())
                            .any(|model| model.to_ascii_lowercase().contains(wanted))
                    })
                    && upstream_filter.as_ref().is_none_or(|wanted| {
                        row.upstream_target
                            .as_deref()
                            .is_some_and(|target| target.to_ascii_lowercase().contains(wanted))
                    })
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.started_ms));
        let total = rows.len();
        return json_no_store(
            StatusCode::OK,
            &FlowsResponse {
                flows: apply_paging(rows, query.page, query.limit),
                total,
                flow_seq: cut.snapshot.cursors.flow_seq,
            },
        );
    }
    let (records, flow_seq) = gateway.flow_store().list_with_seq();
    let status_filter = query.status.as_deref().and_then(parse_status_filter);
    let model_filter = query.model.as_deref().map(str::to_ascii_lowercase);
    let upstream_filter = query.upstream.as_deref().map(str::to_ascii_lowercase);

    let rows: Vec<FlowRow> = records
        .iter()
        .filter(|record| {
            status_filter.is_none_or(|status| record.status == status)
                && model_filter
                    .as_ref()
                    .is_none_or(|wanted| record_matches_model(record, wanted))
                && upstream_filter.as_ref().is_none_or(|wanted| {
                    record
                        .upstream_target
                        .as_deref()
                        .is_some_and(|target| target.to_ascii_lowercase().contains(wanted))
                })
        })
        .map(|record| FlowRow::from_record(record, gateway.as_ref()))
        .collect();

    let total = rows.len();
    let paged = apply_paging(rows, query.page, query.limit);
    json_no_store(
        StatusCode::OK,
        &FlowsResponse {
            flows: paged,
            total,
            flow_seq,
        },
    )
}

/// `GET /dashboard/api/flows/:id` — the 3-pane inspector body (`:id == api_call_id`,
/// joined by either id via the FlowStore link index). Returns the three captured
/// on-wire bodies (absent, not error, when evicted), the inbound headers, the
/// replayed deltas (MonitorHub snapshot filtered by `response_id`), usage, the
/// terminal, timing, the served identity, and the `cost`. `404` for an unknown id.
pub async fn dashboard_flow_detail(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    Query(query): Query<FlowDetailQuery>,
) -> Response {
    // Capture the record AND its own mutation watermark in one lock hold so the
    // detail's `flow_seq` is the record's own cursor (D7b R1 finding 3), not a
    // later global value bumped by unrelated flows.
    if query.cut_id.is_none()
        && let Some((record, flow_seq)) = gateway.flow_store().detail_with_seq(&id)
    {
        let normalized = record.usage.map(crate::dashboard_flow::normalize_usage);
        let cost = record.terminal_cost_usd;
        let cost_confidence = record.terminal_cost_confidence.into();
        let (deltas, deltas_through_monitor_seq) =
            replay_deltas(record.response_id.as_deref(), gateway.debug_snapshot());
        let inbound_headers = if record.headers.is_empty() {
            None
        } else {
            Some(
                record
                    .headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect::<BTreeMap<String, String>>(),
            )
        };
        let captured_sections = durable_sections(gateway.dashboard_history(), &id).await;
        let body = FlowDetailBody {
            detail_source: FlowDetailSource::Live,
            flow_seq,
            revision: record.revision,
            api_call_id: record.api_call_id.clone(),
            response_id: record.response_id.clone(),
            inbound_body: record.inbound_body.as_ref().map(parse_captured_body),
            inbound_headers,
            normalized: record.normalized.as_ref().map(parse_captured_body),
            upstream_body: record.upstream_body.as_ref().map(parse_captured_body),
            upstream_response: record.upstream_response.as_ref().map(|response| {
                FlowUpstreamResponse {
                    body: parse_captured_body(&response.bytes),
                    truncated: response.truncated,
                }
            }),
            model_requested: record.model_requested.clone(),
            model_served: record.model_served.clone(),
            upstream_target: record.upstream_target.clone(),
            usage: record.usage,
            normalized_usage: normalized.map(|value| value.usage),
            usage_anomaly_count: normalized.map_or(0, |value| value.anomaly_count),
            effective_route_limit: record.effective_route_limit,
            cache_price_impact_usd: record.cache_price_impact_usd,
            status: record.status,
            deltas_through_monitor_seq,
            deltas,
            terminal_reason: record.terminal_reason.clone(),
            started_ms: record.started_ms,
            finished_ms: record.finished_ms,
            elapsed_ms: record.elapsed_ms,
            cost,
            cost_confidence,
            phases: record.phases,
            attempts: record.attempts.clone(),
            first_upstream_byte_ms: record.first_upstream_byte_ms,
            captured_sections,
        };
        return json_no_store(StatusCode::OK, &body);
    }

    let (summary, flow_seq, monitor_seq) = if let Some(cut_id) = query.cut_id {
        let Some(cut) = gateway.dashboard_history().cut_by_id(cut_id).await else {
            return json_no_store(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": {"code": "historical_cut_not_found", "cut_id": cut_id}}),
            );
        };
        let Some(summary) = gateway
            .dashboard_history()
            .flow_summary_at(&id, cut_id)
            .await
        else {
            return json_no_store(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": {"code": "historical_flow_not_found", "cut_id": cut_id, "api_call_id": id}}),
            );
        };
        (
            summary,
            cut.snapshot.cursors.flow_seq,
            cut.snapshot.cursors.monitor_seq,
        )
    } else {
        let Some(summary) = gateway.dashboard_history().latest_flow_summary(&id).await else {
            return json_no_store(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": {"code": "flow_expired", "message": "live flow expired and no durable history exists", "api_call_id": id}}),
            );
        };
        let latest = gateway.dashboard_history().latest_cut().await;
        (
            summary,
            latest
                .as_ref()
                .map_or(0, |cut| cut.snapshot.cursors.flow_seq),
            latest
                .as_ref()
                .map_or(0, |cut| cut.snapshot.cursors.monitor_seq),
        )
    };
    let captured_sections =
        durable_sections(gateway.dashboard_history(), &summary.api_call_id).await;
    let section = |name: &str| {
        captured_sections
            .iter()
            .find(|section| section.name == name)
            .map(|section| section.content.clone())
    };
    let messages = gateway
        .dashboard_history()
        .monitor_messages_through(monitor_seq)
        .await;
    let (deltas, deltas_through_monitor_seq) = replay_deltas(
        summary.response_id.as_deref(),
        DebugSnapshot {
            last_sequence: monitor_seq,
            messages,
        },
    );
    let normalized = summary.usage.map(crate::dashboard_flow::normalize_usage);
    let body = FlowDetailBody {
        detail_source: FlowDetailSource::Durable,
        flow_seq,
        revision: summary.revision,
        api_call_id: summary.api_call_id.clone(),
        response_id: summary.response_id.clone(),
        inbound_body: section("inbound_request"),
        inbound_headers: None,
        // Legacy artifacts predate durable canonical-body capture. Absence is honest;
        // newly captured files can add `normalized_request` without changing this API.
        normalized: section("normalized_request"),
        upstream_body: section("upstream_request"),
        upstream_response: None,
        model_requested: summary.model_requested.clone(),
        model_served: summary.model_served.clone(),
        upstream_target: summary.upstream_target.clone(),
        usage: summary.usage,
        normalized_usage: normalized.map(|value| value.usage),
        usage_anomaly_count: normalized.map_or(0, |value| value.anomaly_count),
        effective_route_limit: summary.effective_route_limit,
        cache_price_impact_usd: summary.cache_price_impact_usd,
        status: summary.status,
        deltas_through_monitor_seq,
        deltas,
        terminal_reason: summary.terminal_reason.clone(),
        started_ms: summary.started_ms,
        finished_ms: summary.finished_ms,
        elapsed_ms: summary.elapsed_ms,
        cost: summary.terminal_cost_usd,
        cost_confidence: summary.terminal_cost_confidence.into(),
        phases: summary.phases,
        attempts: summary.attempts.clone(),
        first_upstream_byte_ms: summary.first_upstream_byte_ms,
        captured_sections,
    };
    json_no_store(StatusCode::OK, &body)
}

/// `GET /dashboard/api/metrics` — the live stats tiles (D5 view) + the metrics
/// domain `metrics_seq` + the live open-flow `active_streams` count + the priced
/// `cost_per_min`. Per-window TRUE per-second rates (D13 divides by the window
/// seconds). The view + its cursor are captured in ONE metrics-lock hold so the
/// body and `metrics_seq` are consistent.
pub async fn dashboard_metrics(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<HistoricalCutQuery>,
) -> Response {
    if let Some(cut_id) = query.cut_id {
        let Some(cut) = gateway.dashboard_history().cut_by_id(cut_id).await else {
            return json_no_store(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": {"code": "historical_cut_not_found", "cut_id": cut_id}}),
            );
        };
        return json_no_store(
            StatusCode::OK,
            &metrics_body(
                &cut.snapshot.instant,
                cut.snapshot.last_activity.as_ref(),
                cut.snapshot.engine_throughput.as_ref(),
                cut.snapshot.taken_at_ms,
                cut.snapshot.cursors.metrics_seq,
            ),
        );
    }
    let body = if let Some(cut) = gateway.metrics().latest_published_metrics() {
        metrics_body(
            &cut.instant,
            cut.last_activity.as_ref(),
            cut.engine_throughput.as_ref(),
            cut.taken_at_ms,
            cut.cursors.metrics_seq,
        )
    } else {
        // A manually-constructed Gateway may omit the DI bootstrap publication. Return
        // the explicit zero-sample/unavailable shape; never independently recompute a
        // second presentation or allocate a cursor outside the process publisher.
        metrics_body(
            &crate::metrics::InstantMetricSample::bootstrap(0),
            None,
            None,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            0,
        )
    };
    json_no_store(StatusCode::OK, &body)
}

/// `GET /dashboard/api/overview?window=m1|m5|h1&at=&status=&model=&upstream=&client=`.
/// Live reads consume the latest process-wide immutable metrics cut. Historical reads
/// use the absolute nearest retained cut (ties prefer the older cut), including the terminal-time prices
/// embedded in that cut; neither path consults the current flow list or reprices data.
pub async fn dashboard_overview(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<OverviewQuery>,
) -> Response {
    let (status, status_scope) = normalize_overview_status(query.status.as_deref());
    if status == Some(FlowStatus::Open) {
        return json_no_store(
            StatusCode::UNPROCESSABLE_ENTITY,
            &serde_json::json!({
                "error": {
                    "code": "terminal_analytics_unavailable_for_open_scope",
                    "message": "terminal analytics unavailable for open-only scope"
                }
            }),
        );
    }
    let model = clean_overview_filter(query.model);
    let upstream = clean_overview_filter(query.upstream);
    let client = clean_overview_filter(query.client);
    let filter = OverviewFilter {
        status,
        model: model.clone(),
        upstream: upstream.clone(),
        client: client.clone(),
    };
    let requested_at_ms = query.at.map(u128::from);

    let (generated_at_ms, metrics_seq, selected_at_ms, selected_cut_id, mut aggregate) =
        if let Some(cut_id) = query.cut_id {
            if let Some(cut) = gateway.dashboard_history().cut_by_id(cut_id).await {
                (
                    cut.snapshot.taken_at_ms,
                    cut.snapshot.cursors.metrics_seq,
                    Some(cut.snapshot.taken_at_ms),
                    Some(cut.cut_id),
                    overview_window_report(&cut.snapshot.metrics, query.window).overview(&filter),
                )
            } else {
                let mut empty =
                    overview_window_report(&MetricsView::default(), query.window).overview(&filter);
                empty.data_quality = OverviewDataQuality::Unavailable;
                (
                    requested_at_ms.unwrap_or(cut_id as u128),
                    0,
                    None,
                    Some(cut_id),
                    empty,
                )
            }
        } else if let Some(at) = requested_at_ms {
            if let Some(cut) = gateway.metrics().nearest_snapshot(at) {
                (
                    cut.taken_at_ms,
                    cut.cursors.metrics_seq,
                    Some(cut.taken_at_ms),
                    u64::try_from(cut.taken_at_ms).ok(),
                    overview_window_report(&cut.metrics, query.window).overview(&filter),
                )
            } else if let Some(cut) = gateway
                .dashboard_history()
                .nearest_cut(at.min(u64::MAX as u128) as u64)
                .await
            {
                (
                    cut.snapshot.taken_at_ms,
                    cut.snapshot.cursors.metrics_seq,
                    Some(cut.snapshot.taken_at_ms),
                    Some(cut.cut_id),
                    overview_window_report(&cut.snapshot.metrics, query.window).overview(&filter),
                )
            } else {
                let mut empty =
                    overview_window_report(&MetricsView::default(), query.window).overview(&filter);
                empty.data_quality = OverviewDataQuality::Unavailable;
                (at, 0, None, None, empty)
            }
        } else if let Some(cut) = gateway.metrics().latest_published_metrics() {
            (
                cut.taken_at_ms,
                cut.cursors.metrics_seq,
                Some(cut.taken_at_ms),
                None,
                overview_window_report(&cut.view, query.window).overview(&filter),
            )
        } else {
            let mut empty =
                overview_window_report(&MetricsView::default(), query.window).overview(&filter);
            empty.data_quality = OverviewDataQuality::Unavailable;
            (dashboard_now_ms(), 0, None, None, empty)
        };

    // A filtered query over a window with bounded overflow is necessarily partial: the
    // fixed `__other__` key no longer carries enough identity to prove whether a folded
    // sample matched. The metrics projection already marks every overflowed aggregate;
    // retain the explicit assignment here as a guard against future projection changes.
    if aggregate.overflow.overflowed {
        aggregate.data_quality = OverviewDataQuality::Partial;
    }

    json_no_store(
        StatusCode::OK,
        &OverviewResponse {
            generated_at_ms,
            metrics_seq,
            scope: OverviewScope {
                window: query.window,
                cut_id: selected_cut_id,
                requested_at_ms,
                selected_at_ms,
                status: status_scope,
                model,
                upstream,
                client,
            },
            aggregate,
        },
    )
}

/// `GET /dashboard/api/topology` — the provider topology (D4 nodes + edges) + the
/// price table + the topology domain `topology_seq`. Edges carry per-upstream
/// per-second request/token/cost rates rolled up from the live `m1` metrics window.
pub async fn dashboard_topology(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<HistoricalCutQuery>,
) -> Response {
    if let Some(cut_id) = query.cut_id {
        let Some(cut) = gateway.dashboard_history().cut_by_id(cut_id).await else {
            return json_no_store(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": {"code": "historical_cut_not_found", "cut_id": cut_id}}),
            );
        };
        return json_no_store(
            StatusCode::OK,
            &topology_body(
                &cut.snapshot.topology,
                gateway.price_table(),
                &cut.snapshot.metrics.window_1m,
                &cut.snapshot.backend_metrics,
            ),
        );
    }
    let body = if let Some(cut) = gateway.metrics().latest_published_metrics() {
        topology_body(
            &cut.topology,
            gateway.price_table(),
            &cut.view.window_1m,
            &cut.backend_metrics,
        )
    } else {
        topology_body(
            &ProviderHealthSnapshot::default(),
            gateway.price_table(),
            &MetricsView::default().window_1m,
            &crate::backend_metrics::BackendMetricsSnapshot::default(),
        )
    };
    json_no_store(StatusCode::OK, &body)
}

/// `GET /dashboard/api/catalog` — the model catalog as a BARE array `[{id,
/// context_limit}]` (no cursor; a static-ish read). Sourced from the upstream
/// `/v1/models` snapshot via the `UpstreamClient` (ids + per-model context
/// window), reusing the SAME `context_limit_by_id` parse that feeds G3 budgeting
/// (no second max-context parser — gap 06).
///
/// `context_limit` is surfaced NULLABLE: an upstream that advertises no window
/// yields `None` (serialized absent), NOT a non-null `0`. The prior `unwrap_or(0)`
/// collapse is removed (gap 06): it lied-with-zeros — a `0` ceiling is
/// indistinguishable from a real value and reads as garbage/infinite utilization
/// in spec 09's gauge. The frontend renders `—` on the missing window.
///
/// An upstream catalog-fetch failure yields an empty array (the dashboard simply
/// shows no catalog) rather than a 5xx that would blank the whole view.
pub async fn dashboard_catalog(State(gateway): State<Arc<Gateway>>) -> Response {
    let entries = match gateway.upstream_client().supported_model_catalog().await {
        Ok(catalog) => catalog
            .into_iter()
            .map(|entry| CatalogEntry {
                id: entry.id,
                // Pass the parsed `Option<i64>` THROUGH unchanged: a known window
                // serializes as the integer, an unknown one as absent/null. Do NOT
                // re-collapse to 0 (the gap 06 lie-with-zeros fix).
                context_limit: entry.context_limit,
            })
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    json_no_store(StatusCode::OK, &entries)
}

/// `GET /dashboard/api/snapshot?at=<unix_ms>` — a body-free frozen cut from the D5
/// snapshot ring (`snapshot_at(ts)` nearest ≤ ts, or the latest cut when `at` is
/// absent). Reshapes the cut's metrics ([`MetricsView`]) + topology
/// ([`ProviderHealthSnapshot`]) into their REST bodies and prices the body-free
/// summaries. `200` with empty summaries + `null` metrics/topology + zero cursors
/// when no cut has been taken yet (rather than a 404 the SPA would treat as fatal).
pub async fn dashboard_snapshot(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    // Widen the `u64` query instant to the `u128` `snapshot_at` key (the query
    // deserializer cannot parse `u128`; unix-ms fits `u64`).
    let at_query = query.at.map(u128::from);
    let selected = if let Some(cut_id) = query.cut_id {
        gateway
            .dashboard_history()
            .cut_by_id(cut_id)
            .await
            .map(|cut| (cut.snapshot, Some(cut.cut_id), true))
    } else if let Some(at) = query.at {
        if let Some(cut) = gateway.metrics().snapshot_at(u128::from(at)) {
            let cut_id = u64::try_from(cut.taken_at_ms).ok();
            Some((cut, cut_id, false))
        } else {
            gateway
                .dashboard_history()
                .cut_at_or_before(at)
                .await
                .map(|cut| (cut.snapshot, Some(cut.cut_id), true))
        }
    } else if let Some(cut) = gateway.metrics().latest_snapshot() {
        let cut_id = u64::try_from(cut.taken_at_ms).ok();
        Some((cut, cut_id, false))
    } else {
        gateway
            .dashboard_history()
            .latest_cut()
            .await
            .map(|cut| (cut.snapshot, Some(cut.cut_id), true))
    };
    let history = gateway.metrics().snapshot_history_metadata();
    let Some((cut, cut_id, durable)) = selected else {
        // No cut yet (the 5 s task has not run, or every cut is newer than `at`):
        // a contract-valid empty snapshot, not a 404.
        return json_no_store(
            StatusCode::OK,
            &SnapshotResponse {
                cut_id: query.cut_id,
                cursors: SeqCursors::default(),
                at_ms: at_query.unwrap_or(0),
                summaries: Vec::new(),
                metrics: None,
                topology: None,
                history,
                flow_summaries_truncated: false,
                monitor_messages: Vec::new(),
            },
        );
    };

    let prices = gateway.price_table();
    let durable_summaries = if durable {
        gateway
            .dashboard_history()
            .flow_summaries_as_of(cut_id.unwrap_or_default())
            .await
    } else {
        Vec::new()
    };
    let summary_source = if durable && !durable_summaries.is_empty() {
        durable_summaries.as_slice()
    } else {
        cut.summaries.as_slice()
    };
    let summaries: Vec<FlowRow> = summary_source
        .iter()
        .map(|summary| FlowRow::from_summary(summary, gateway.as_ref()))
        .collect();
    // Reshape the cut's body-free metrics view into the REST `/metrics` shape, with
    // the cut's own `metrics_seq` cursor. `active_streams` is derived from the FROZEN
    // cut's open summaries (D13 R1 HIGH) — NOT the live FlowStore — so a historical
    // `?at=` reflects how many streams were open AT THAT CUT, not now. The cut's
    // `summaries` are the same body-free flow projections captured in the snapshot's
    // single critical section, so counting `status == Open` among them is consistent
    // with the rest of the frozen cut.
    let metrics = Some(metrics_body(
        &cut.instant,
        cut.last_activity.as_ref(),
        cut.engine_throughput.as_ref(),
        cut.taken_at_ms,
        cut.cursors.metrics_seq,
    ));
    let topology = Some(topology_body(
        &cut.topology,
        prices,
        &cut.metrics.window_1m,
        &cut.backend_metrics,
    ));
    let monitor_messages = if durable {
        gateway
            .dashboard_history()
            .monitor_messages_through(cut.cursors.monitor_seq)
            .await
    } else {
        Vec::new()
    };
    json_no_store(
        StatusCode::OK,
        &SnapshotResponse {
            cut_id,
            cursors: SeqCursors {
                flow_seq: cut.cursors.flow_seq,
                metrics_seq: cut.cursors.metrics_seq,
                topology_seq: cut.cursors.topology_seq,
                monitor_seq: cut.cursors.monitor_seq,
                backend_metrics_seq: cut.cursors.backend_metrics_seq,
            },
            at_ms: cut.taken_at_ms,
            summaries,
            metrics,
            topology,
            history,
            flow_summaries_truncated: cut.flow_summaries_truncated,
            monitor_messages,
        },
    )
}

/// Durable scrubber history. Points are materialized from the exact persisted cuts;
/// downsampling always retains the oldest/newest requested point so the UI's bounds and
/// playhead remain honest.
pub async fn dashboard_history(
    State(gateway): State<Arc<Gateway>>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let metadata = gateway.dashboard_history().metadata().await;
    let durable_cuts = gateway
        .dashboard_history()
        .cuts_between(query.from, query.to)
        .await;
    // Durable writes are asynchronous and may lag or drop under pressure. Merge them
    // with the bounded in-memory five-second ring and deduplicate by the coordinated cut
    // timestamp (also the durable cut id), preferring the durable row when both exist.
    let mut merged =
        std::collections::BTreeMap::<u128, crate::dashboard_history::HistoricalCut>::new();
    for snapshot in gateway.metrics().snapshots_between(query.from, query.to) {
        let Ok(cut_id) = u64::try_from(snapshot.taken_at_ms) else {
            continue;
        };
        merged.insert(
            snapshot.taken_at_ms,
            crate::dashboard_history::HistoricalCut { cut_id, snapshot },
        );
    }
    for cut in durable_cuts {
        merged.insert(cut.snapshot.taken_at_ms, cut);
    }
    let cuts = merged.into_values().collect::<Vec<_>>();
    let limit = query.limit.unwrap_or(2_000).clamp(2, 10_000);
    let retained_cuts = cuts.len();
    let oldest_at_ms = cuts.first().map(|cut| cut.snapshot.taken_at_ms);
    let newest_at_ms = cuts.last().map(|cut| cut.snapshot.taken_at_ms);
    let selected: Vec<_> = if cuts.len() <= limit {
        cuts
    } else {
        let last = cuts.len() - 1;
        (0..limit)
            .map(|index| {
                let source = index.saturating_mul(last) / (limit - 1);
                cuts[source].clone()
            })
            .collect()
    };
    let points: Vec<HistoryPoint> = selected
        .into_iter()
        .map(|cut| HistoryPoint {
            cut_id: cut.cut_id,
            at_ms: cut.snapshot.taken_at_ms,
            cursors: SeqCursors {
                flow_seq: cut.snapshot.cursors.flow_seq,
                metrics_seq: cut.snapshot.cursors.metrics_seq,
                topology_seq: cut.snapshot.cursors.topology_seq,
                monitor_seq: cut.snapshot.cursors.monitor_seq,
                backend_metrics_seq: cut.snapshot.cursors.backend_metrics_seq,
            },
            instant: cut.snapshot.instant.clone(),
            engine_throughput: cut.snapshot.engine_throughput.clone(),
        })
        .collect();
    json_no_store(
        StatusCode::OK,
        &HistoryResponse {
            oldest_at_ms,
            newest_at_ms,
            retained_cuts,
            database_bytes: metadata.database_bytes,
            dropped_writes: metadata.dropped_writes,
            points,
        },
    )
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Whether a record's served OR requested model contains the (lowercased) filter
/// substring — the `model=` filter matches either identity so a row is findable by
/// what the client asked for OR what served it.
fn record_matches_model(record: &FlowRecord, wanted: &str) -> bool {
    record
        .model_served
        .as_deref()
        .is_some_and(|model| model.to_ascii_lowercase().contains(wanted))
        || record
            .model_requested
            .as_deref()
            .is_some_and(|model| model.to_ascii_lowercase().contains(wanted))
}

fn overview_window_report(view: &MetricsView, window: OverviewWindow) -> &WindowReport {
    match window {
        OverviewWindow::M1 => &view.window_1m,
        OverviewWindow::M5 => &view.window_5m,
        OverviewWindow::H1 => &view.window_1h,
    }
}

fn dashboard_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Parse a `status=` filter value into a [`FlowStatus`] (the frozen
/// open/completed/failed/cancelled enum). An unrecognized value yields `None` so
/// the filter is simply ignored (no rows wrongly hidden by a typo).
fn parse_status_filter(value: &str) -> Option<FlowStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "open" => Some(FlowStatus::Open),
        "completed" => Some(FlowStatus::Completed),
        "failed" => Some(FlowStatus::Failed),
        "cancelled" => Some(FlowStatus::Cancelled),
        _ => None,
    }
}

fn normalize_overview_status(value: Option<&str>) -> (Option<FlowStatus>, Option<String>) {
    let status = value.and_then(parse_status_filter);
    let effective = status.map(|status| match status {
        FlowStatus::Open => "open",
        FlowStatus::Completed => "completed",
        FlowStatus::Failed => "failed",
        FlowStatus::Cancelled => "cancelled",
    });
    (status, effective.map(str::to_string))
}

fn clean_overview_filter(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Apply 1-based `page`/`limit` paging to the filtered rows. Absent `limit` ⇒ all
/// rows (no paging). Absent `page` ⇒ page 1. An out-of-range page yields an empty
/// slice (the SPA shows no rows, with `total` telling it how many exist).
fn apply_paging(rows: Vec<FlowRow>, page: Option<usize>, limit: Option<usize>) -> Vec<FlowRow> {
    let Some(limit) = limit.filter(|limit| *limit > 0) else {
        return rows;
    };
    let page = page.unwrap_or(1).max(1);
    let start = (page - 1).saturating_mul(limit);
    rows.into_iter().skip(start).take(limit).collect()
}

/// Serialize `body` as JSON with the dashboard security headers + `no-store` (D7a):
/// EVERY `/dashboard/api/*` response is uncacheable (auth-scoped, per-request) and
/// carries the locked-down CSP/nosniff/no-referrer/X-Frame-Options set, exactly
/// like the auth-layer responses. A serialization failure (should be unreachable —
/// the DTOs are plain data) degrades to a 500 with the same headers.
fn json_no_store<T: Serialize>(status: StatusCode, body: &T) -> Response {
    let response = match serde_json::to_vec(body) {
        Ok(bytes) => (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to serialize response",
        )
            .into_response(),
    };
    crate::dashboard_auth::no_store(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_contract_serializes_exact_scope_rollups_and_window_names() {
        assert_eq!(
            serde_json::from_str::<OverviewWindow>("\"m5\"").unwrap(),
            OverviewWindow::M5
        );
        let metrics = crate::metrics::MetricsLayer::new();
        let input = crate::dashboard_flow::TerminalMetricsInputs {
            model_requested: Some("requested".to_string()),
            model_served: Some("served".to_string()),
            endpoint: "/v1/responses".to_string(),
            upstream: Some("provider".to_string()),
            client_label: Some("client".to_string()),
            usage: Some(FlowUsage {
                prompt: 100,
                completion: 20,
                total: 120,
                cached: Some(0),
                reasoning: Some(0),
            }),
            failure_reason: crate::dashboard_flow::TerminalReasonClass::Stop,
            cost_usd: Some(0.25),
            cost_confidence: crate::dashboard_flow::TerminalCostConfidence::Confident,
            effective_route_limit: Some(8_192),
            ..Default::default()
        };
        metrics.record_terminal_inputs(FlowStatus::Completed, 10, &input);
        let aggregate = metrics
            .view()
            .window_5m
            .overview(&OverviewFilter::default());
        let response = OverviewResponse {
            generated_at_ms: 123,
            metrics_seq: 7,
            scope: OverviewScope {
                window: OverviewWindow::M5,
                cut_id: None,
                requested_at_ms: None,
                selected_at_ms: Some(123),
                status: None,
                model: None,
                upstream: None,
                client: None,
            },
            aggregate,
        };
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["scope"]["window"], "m5");
        assert_eq!(json["totals"]["requests"], 1);
        assert_eq!(json["cost"]["total_usd"], 0.25);
        assert_eq!(json["context"]["effective_route_limit_min"], 8_192);
        assert_eq!(json["provider_attempts_global"]["scope"], "global");
    }

    #[test]
    fn overview_scope_echoes_only_normalized_effective_filters() {
        assert_eq!(
            normalize_overview_status(Some("  FAILED ")),
            (Some(FlowStatus::Failed), Some("failed".to_string()))
        );
        assert_eq!(
            normalize_overview_status(Some("not-a-status")),
            (None, None),
            "an ignored status must not be echoed as if it constrained the aggregate"
        );
        assert_eq!(
            clean_overview_filter(Some("  model-a  ".to_string())),
            Some("model-a".to_string())
        );
        assert_eq!(clean_overview_filter(Some("   ".to_string())), None);
    }

    /// A price with an EXPLICITLY configured cached rate (presence `true`) — the
    /// default for these cost tests. Confidence-specific tests use
    /// `ModelPrice::without_cached` to exercise the unconfigured-cache path.
    fn price(input: f64, output: f64, cached: f64) -> ModelPrice {
        ModelPrice::new(input, output, cached)
    }

    /// A usage with a REPORTED (measured) cached count and a reported `0` reasoning
    /// (gap 07 `Some` — distinct from the UNAVAILABLE `None` the dedicated tests use).
    fn usage(prompt: i64, completion: i64, cached: i64) -> FlowUsage {
        FlowUsage {
            prompt,
            completion,
            cached: Some(cached),
            reasoning: Some(0),
            total: prompt + completion,
        }
    }

    fn record_terminal_cost(
        metrics: &crate::metrics::MetricsLayer,
        model: &str,
        usage: Option<FlowUsage>,
        cost_usd: Option<f64>,
        confidence: crate::dashboard_flow::TerminalCostConfidence,
    ) {
        metrics.record_terminal_inputs(
            crate::dashboard_flow::FlowStatus::Completed,
            900,
            &crate::dashboard_flow::TerminalMetricsInputs {
                model_served: Some(model.to_string()),
                endpoint: "/v1/responses".to_string(),
                upstream: Some("vllm-a".to_string()),
                usage,
                cost_usd,
                cost_confidence: confidence,
                ..Default::default()
            },
        );
    }

    /// The cost model splits prompt into uncached (input rate) + cached (cache
    /// rate) and bills completion at the output rate. 90 uncached prompt @ 2.0/1k
    /// + 10 cached @ 0.5/1k + 40 completion @ 6.0/1k = 0.18 + 0.005 + 0.24 = 0.425.
    #[test]
    fn cost_for_usage_splits_cached_prompt_and_bills_completion() {
        let cost = cost_for_usage(usage(100, 40, 10), price(2.0, 6.0, 0.5));
        assert!((cost - 0.425).abs() < 1e-9, "cost {cost} == 0.425");
    }

    /// `cached > prompt` (a transient/odd report) never yields a negative input
    /// charge or bills more cached tokens than the canonical prompt volume.
    #[test]
    fn cost_for_usage_clamps_cached_over_prompt() {
        let cost = cost_for_usage(usage(10, 0, 50), price(2.0, 6.0, 0.5));
        // cached clamps to prompt=10; 10/1000*0.5 = 0.005.
        assert!(
            (cost - 0.005).abs() < 1e-9,
            "cost {cost} == 0.005 (bounded subset)"
        );
    }

    /// A degenerate configured price (an absurd magnitude that overflows to ∞, or a
    /// NaN) must NOT yield a non-finite cost — `serde_json` errors on NaN/±∞ and
    /// would 500 the read. `cost_for_usage` collapses a non-finite result to `0.0`
    /// (the `finite` guard), so the JSON stays well-formed.
    #[test]
    fn cost_for_usage_is_finite_even_for_overflowing_prices() {
        // 1e9 tokens × an f64::MAX per-1k rate overflows the product to +inf.
        let cost = cost_for_usage(
            usage(1_000_000_000, 1_000_000_000, 0),
            price(f64::MAX, f64::MAX, 0.0),
        );
        assert!(
            cost.is_finite(),
            "an overflowing price must not produce ±inf cost"
        );
        // A NaN-producing price likewise sanitizes to a finite value.
        assert!(cost_for_usage(usage(1, 1, 0), price(f64::NAN, 1.0, 0.0)).is_finite());
    }

    /// A model with no configured price contributes no cost to a window roll-up
    /// (it is simply skipped — never a fabricated zero that would understate the
    /// per-1k rate of the priced buckets).
    #[test]
    fn price_lookup_is_exact_then_case_insensitive() {
        let mut prices = HashMap::new();
        prices.insert("GLM-5.1".to_string(), price(1.0, 2.0, 0.0));
        assert!(price_lookup(&prices, "glm-5.1").is_some());
        assert!(price_lookup(&prices, "other").is_none());
    }

    /// Additive wire round trip: an idle response carries the retained request-bearing
    /// sample, while an older response may omit it and still deserialize.
    #[test]
    fn metrics_body_instant_sample_round_trips() {
        let instant = crate::metrics::InstantMetricSample {
            ready: true,
            interval_duration_ms: Some(1_250),
            accepted_per_sec: Some(0.8),
            active_streams_now: 2,
            ..Default::default()
        };
        let last_activity = crate::metrics::LastActivitySample {
            at_ms: 41_000,
            instant: instant.clone(),
        };
        let engine_throughput = crate::backend_metrics::EngineThroughputSample {
            generated_tokens_per_sec: 37.5,
            sampled_at_ms: 41_500,
            measured_sources: 1,
            total_sources: 2,
            coverage: crate::backend_metrics::BackendMetricsCoverage::Partial,
        };
        let value = serde_json::to_value(metrics_body(
            &crate::metrics::InstantMetricSample::bootstrap(0),
            Some(&last_activity),
            Some(&engine_throughput),
            42_000,
            7,
        ))
        .unwrap();
        assert_eq!(value["generated_at_ms"], 42_000);
        assert_eq!(value["last_activity"]["at_ms"], 41_000);
        assert_eq!(
            value["last_activity"]["instant"]["interval_duration_ms"],
            1_250
        );
        assert_eq!(value["engine_throughput"]["generated_tokens_per_sec"], 37.5);
        assert_eq!(value["engine_throughput"]["coverage"], "partial");
        assert!(value.get("windows").is_none());

        let decoded: MetricsSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);

        let legacy = serde_json::json!({
            "metrics_seq": 7,
            "generated_at_ms": 42_000,
            "instant": crate::metrics::InstantMetricSample::bootstrap(0),
        });
        let decoded: MetricsSnapshot = serde_json::from_value(legacy).unwrap();
        assert!(decoded.last_activity.is_none());
        assert!(decoded.engine_throughput.is_none());
    }

    /// 1-based paging: page 2 with limit 2 over 5 rows yields rows 3..=4; a limit
    /// of 0 (or absent) returns all rows; an out-of-range page yields empty.
    #[test]
    fn apply_paging_pages_1_based() {
        let rows = |n: usize| -> Vec<FlowRow> {
            (0..n)
                .map(|i| FlowRow {
                    revision: 1,
                    api_call_id: format!("api_{i}"),
                    response_id: None,
                    method: "POST".to_string(),
                    uri: "/v1/responses".to_string(),
                    model_requested: None,
                    model_served: None,
                    upstream_target: None,
                    usage: None,
                    normalized_usage: None,
                    usage_anomaly_count: 0,
                    effective_route_limit: None,
                    cache_price_impact_usd: None,
                    status: FlowStatus::Completed,
                    started_ms: 0,
                    finished_ms: None,
                    elapsed_ms: None,
                    terminal_reason: None,
                    client_label: None,
                    client_source: None,
                    cost: None,
                    cost_confidence: CostConfidence::Unavailable,
                    phases: PhaseTimings::default(),
                    attempts: Vec::new(),
                    first_upstream_byte_ms: None,
                })
                .collect()
        };
        let page2 = apply_paging(rows(5), Some(2), Some(2));
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].api_call_id, "api_2");
        assert_eq!(page2[1].api_call_id, "api_3");
        // No limit ⇒ all rows.
        assert_eq!(apply_paging(rows(5), None, None).len(), 5);
        // Out-of-range page ⇒ empty.
        assert!(apply_paging(rows(3), Some(9), Some(2)).is_empty());
    }

    /// Gap 04 review F3: `FlowRow` carries the OPTIONAL `client_label`/`client_source`
    /// attribution fields, serialized with `skip_serializing_if` so a PRESENT pair emits
    /// the snake_case keys (with the key-hash label + `key_hash` source) while an ABSENT
    /// pair OMITS both keys entirely (never `null`/empty-string-as-id). This pins the
    /// `/flows` + `/snapshot` summary wire contract for the new fields.
    #[test]
    fn flow_row_serializes_optional_client_attribution_present_and_absent() {
        let base = || FlowRow {
            revision: 1,
            api_call_id: "api_x".to_string(),
            response_id: None,
            method: "POST".to_string(),
            uri: "/v1/responses".to_string(),
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            normalized_usage: None,
            usage_anomaly_count: 0,
            effective_route_limit: None,
            cache_price_impact_usd: None,
            status: FlowStatus::Open,
            started_ms: 0,
            finished_ms: None,
            elapsed_ms: None,
            terminal_reason: None,
            client_label: None,
            client_source: None,
            cost: None,
            cost_confidence: CostConfidence::Unavailable,
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
        };

        // PRESENT: a key-hash attribution emits both snake_case keys with the expected
        // values (the label is a `key-<hex>` id — a one-way prefix, never a raw key).
        let present = FlowRow {
            client_label: Some("key-deadbeef0123".to_string()),
            client_source: Some(ClientSource::KeyHash),
            ..base()
        };
        let value = serde_json::to_value(&present).expect("serialize present row");
        assert_eq!(value["client_label"], serde_json::json!("key-deadbeef0123"));
        assert_eq!(value["client_source"], serde_json::json!("key_hash"));

        // ABSENT: an unattributed row OMITS both keys (skip_serializing_if), so the
        // frontend sees no key rather than a fabricated `null`/`0`.
        let absent = serde_json::to_value(base()).expect("serialize absent row");
        let obj = absent.as_object().expect("object");
        assert!(
            !obj.contains_key("client_label"),
            "absent label key omitted: {absent}"
        );
        assert!(
            !obj.contains_key("client_source"),
            "absent source key omitted: {absent}"
        );
    }

    /// Gap 05 review F2: `FlowDetailBody` carries the OPTIONAL `upstream_response`
    /// (the captured upstream RESPONSE/ERROR body + `truncated`) on the LIVE detail path.
    /// This pins the new wire field with a serialize → deserialize ROUND-TRIP (AGENTS.md:
    /// no new wire field without a round-trip proof). The enclosing `FlowDetailBody` is
    /// serialize-only (it is only ever a response), so the round-trip is on the
    /// self-contained `FlowUpstreamResponse` sub-DTO: we SERIALIZE the whole detail body
    /// (proving the field is wired in + that `skip_serializing_if` OMITS it when absent),
    /// then DESERIALIZE the `upstream_response` value back into `FlowUpstreamResponse` and
    /// assert it survives. Covers PRESENT with `truncated` false AND true, and ABSENT.
    #[test]
    fn flow_detail_body_upstream_response_round_trips_present_and_absent() {
        let base = || FlowDetailBody {
            detail_source: FlowDetailSource::Live,
            flow_seq: 7,
            revision: 1,
            api_call_id: "api_d".to_string(),
            response_id: Some("resp_d".to_string()),
            inbound_body: None,
            inbound_headers: None,
            normalized: None,
            upstream_body: None,
            upstream_response: None,
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            normalized_usage: None,
            usage_anomaly_count: 0,
            effective_route_limit: None,
            cache_price_impact_usd: None,
            status: FlowStatus::Failed,
            deltas_through_monitor_seq: 41,
            deltas: Vec::new(),
            terminal_reason: None,
            started_ms: 1,
            finished_ms: None,
            elapsed_ms: None,
            cost: None,
            cost_confidence: CostConfidence::Unavailable,
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
            captured_sections: Vec::new(),
        };

        // PRESENT (not truncated): a JSON error body survives the round-trip intact,
        // with `truncated: false`. Serialize the whole detail body, then deserialize the
        // sub-object back into the typed DTO.
        let present = FlowDetailBody {
            upstream_response: Some(FlowUpstreamResponse {
                body: serde_json::json!({"error": {"message": "backend on fire"}}),
                truncated: false,
            }),
            ..base()
        };
        let value = serde_json::to_value(&present).expect("serialize present detail");
        assert_eq!(
            value["deltas_through_monitor_seq"],
            serde_json::json!(41),
            "the replay watermark is a required scalar on flow detail"
        );
        assert_eq!(
            value["upstream_response"]["body"]["error"]["message"],
            serde_json::json!("backend on fire")
        );
        assert_eq!(
            value["upstream_response"]["truncated"],
            serde_json::json!(false)
        );
        let rt: FlowUpstreamResponse = serde_json::from_value(value["upstream_response"].clone())
            .expect("deserialize present upstream_response");
        assert_eq!(
            rt.body,
            serde_json::json!({"error": {"message": "backend on fire"}}),
            "the captured body survives serialize → deserialize"
        );
        assert!(!rt.truncated, "truncated false survives the round-trip");

        // PRESENT (truncated): a cap-truncated body keeps `truncated: true` across the
        // round-trip so the dashboard can flag a PARTIAL body.
        let truncated = FlowDetailBody {
            upstream_response: Some(FlowUpstreamResponse {
                body: serde_json::Value::String("partial prefix…".to_string()),
                truncated: true,
            }),
            ..base()
        };
        let value = serde_json::to_value(&truncated).expect("serialize truncated detail");
        assert_eq!(
            value["upstream_response"]["truncated"],
            serde_json::json!(true)
        );
        let rt: FlowUpstreamResponse = serde_json::from_value(value["upstream_response"].clone())
            .expect("deserialize truncated upstream_response");
        assert!(rt.truncated, "truncated true survives the round-trip");
        assert_eq!(rt.body, serde_json::json!("partial prefix…"));

        // ABSENT: no captured body OMITS the key entirely (skip_serializing_if), never
        // a `null`.
        let value = serde_json::to_value(base()).expect("serialize absent detail");
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("upstream_response"),
            "absent upstream_response key omitted (not null): {value}"
        );
    }

    /// Gap 10b — a measured attempt the spine-projection tests reuse: a SERVED attempt
    /// with a wire first byte (so `first_upstream_byte_ms` is `Some`) and no error
    /// class/failover reason (the success case). Body-free scalar provenance only.
    fn served_attempt() -> Attempt {
        Attempt {
            provider: Some("vllm-a".to_string()),
            model: Some("llama-3.1-70b".to_string()),
            start_ms: 1_000,
            end_ms: 1_220,
            duration_ms: Some(220),
            first_upstream_byte_ms: Some(1_220),
            first_upstream_byte_offset_ms: Some(220),
            status: crate::dashboard_flow::AttemptStatus::Served,
            error_class: None,
            failover_reason: None,
        }
    }

    /// Gap 10b — a measured phase bundle the spine-projection tests reuse: every phase
    /// stamped (a fully-completed flow), monotonic. The unit `0` is never used as an
    /// epoch (a real wall-clock stamp is large), so a present value unambiguously means
    /// "this phase ran".
    fn measured_phases() -> PhaseTimings {
        PhaseTimings {
            ingress_ms: Some(1_000),
            normalization_done_ms: Some(1_030),
            routing_decision_ms: Some(1_050),
            first_content_delta_ms: Some(1_500),
            stream_end_ms: Some(5_320),
            finalize_ms: Some(5_340),
            ..Default::default()
        }
    }

    /// Gap 10b — `FlowRow` (the `/flows` list + `/snapshot` summary row) PROJECTS the
    /// gap-02 phases (flattened) + gap-03 attempts + `first_upstream_byte_ms`. This pins
    /// the row wire contract for the spine fields: a measured flow EMITS the flattened
    /// phase scalars (`ingress_ms`/`first_content_delta_ms`/…), the `attempts` array (with
    /// the served attempt's wire first byte), and `first_upstream_byte_ms`; an unmeasured
    /// flow OMITS every spine key (don't-lie-with-zeros: absent, never `0`). The flattened
    /// `PhaseTimings` + each `Attempt` are deserialized BACK into their (Deserialize) DTOs
    /// to prove the round-trip survives (AGENTS.md: no new wire field without a round-trip).
    #[test]
    fn flow_row_projects_spine_fields_present_and_absent() {
        let base = || FlowRow {
            revision: 1,
            api_call_id: "api_s".to_string(),
            response_id: None,
            method: "POST".to_string(),
            uri: "/v1/responses".to_string(),
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            normalized_usage: None,
            usage_anomaly_count: 0,
            effective_route_limit: None,
            cache_price_impact_usd: None,
            status: FlowStatus::Completed,
            started_ms: 1_000,
            finished_ms: None,
            elapsed_ms: None,
            terminal_reason: None,
            client_label: None,
            client_source: None,
            cost: None,
            cost_confidence: CostConfidence::Unavailable,
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
        };

        // PRESENT: a measured flow projects the flattened phases + attempts + wire TTFB.
        let present = FlowRow {
            phases: measured_phases(),
            attempts: vec![served_attempt()],
            first_upstream_byte_ms: Some(1_220),
            ..base()
        };
        let value = serde_json::to_value(&present).expect("serialize present row");
        // Phases are FLATTENED as sibling scalars on the row (not nested).
        assert_eq!(value["ingress_ms"], serde_json::json!(1_000));
        assert_eq!(value["first_content_delta_ms"], serde_json::json!(1_500));
        assert_eq!(value["finalize_ms"], serde_json::json!(5_340));
        assert_eq!(value["first_upstream_byte_ms"], serde_json::json!(1_220));
        // The attempt survives a round-trip back into the typed `Attempt`.
        let attempts: Vec<Attempt> =
            serde_json::from_value(value["attempts"].clone()).expect("deserialize attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].status,
            crate::dashboard_flow::AttemptStatus::Served
        );
        assert_eq!(attempts[0].first_upstream_byte_ms, Some(1_220));
        // The flattened phases survive a round-trip back into `PhaseTimings`.
        let phases: PhaseTimings =
            serde_json::from_value(value.clone()).expect("deserialize flattened phases");
        assert_eq!(phases, measured_phases());

        // ABSENT: an unmeasured flow OMITS every spine key — no flattened phase scalar, no
        // `attempts`, no `first_upstream_byte_ms` (don't-lie-with-zeros: absent, never `0`).
        let value = serde_json::to_value(base()).expect("serialize absent row");
        let obj = value.as_object().expect("object");
        for key in [
            "ingress_ms",
            "normalization_done_ms",
            "routing_decision_ms",
            "first_content_delta_ms",
            "stream_end_ms",
            "finalize_ms",
            "attempts",
            "first_upstream_byte_ms",
        ] {
            assert!(
                !obj.contains_key(key),
                "absent spine key {key} omitted (not 0/null): {value}"
            );
        }
    }

    /// Gap 10b — `FlowDetailBody` (the `/flows/:id` inspector) PROJECTS the FULL gap-02
    /// phase set (flattened) + the gap-03 attempts + `first_upstream_byte_ms` (this is where
    /// gap 10's waterfall + gap 11's attempt stepper live). Same present/absent + round-trip
    /// proof as the row, on the detail DTO.
    #[test]
    fn flow_detail_body_projects_spine_fields_present_and_absent() {
        let base = || FlowDetailBody {
            detail_source: FlowDetailSource::Live,
            flow_seq: 3,
            revision: 1,
            api_call_id: "api_sd".to_string(),
            response_id: None,
            inbound_body: None,
            inbound_headers: None,
            normalized: None,
            upstream_body: None,
            upstream_response: None,
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            normalized_usage: None,
            usage_anomaly_count: 0,
            effective_route_limit: None,
            cache_price_impact_usd: None,
            status: FlowStatus::Completed,
            deltas_through_monitor_seq: 17,
            deltas: Vec::new(),
            terminal_reason: None,
            started_ms: 1_000,
            finished_ms: None,
            elapsed_ms: None,
            cost: None,
            cost_confidence: CostConfidence::Unavailable,
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
            captured_sections: Vec::new(),
        };

        // PRESENT: the inspector carries the measured waterfall + attempt trace.
        let present = FlowDetailBody {
            phases: measured_phases(),
            attempts: vec![served_attempt()],
            first_upstream_byte_ms: Some(1_220),
            ..base()
        };
        let value = serde_json::to_value(&present).expect("serialize present detail");
        assert_eq!(value["ingress_ms"], serde_json::json!(1_000));
        assert_eq!(value["stream_end_ms"], serde_json::json!(5_320));
        assert_eq!(value["first_upstream_byte_ms"], serde_json::json!(1_220));
        let attempts: Vec<Attempt> =
            serde_json::from_value(value["attempts"].clone()).expect("deserialize attempts");
        assert_eq!(attempts, vec![served_attempt()]);
        let phases: PhaseTimings =
            serde_json::from_value(value.clone()).expect("deserialize flattened phases");
        assert_eq!(phases, measured_phases());

        // ABSENT: an errored-before-content flow omits the spine keys it never measured.
        let value = serde_json::to_value(base()).expect("serialize absent detail");
        let obj = value.as_object().expect("object");
        for key in [
            "ingress_ms",
            "first_content_delta_ms",
            "attempts",
            "first_upstream_byte_ms",
        ] {
            assert!(
                !obj.contains_key(key),
                "absent spine key {key} omitted on detail (not 0/null): {value}"
            );
        }
    }

    /// The REST replay ordinal and the MonitorHub cursor are intentionally different
    /// clocks. Repeated, same-millisecond segments retain consecutive per-flow ordinals,
    /// while the separately returned watermark comes from the ONE snapshot that supplied
    /// those messages. The frontend can therefore drop only live messages at/before 77
    /// without confusing ordinal `0/1` for monitor sequence numbers.
    #[test]
    fn replay_deltas_keeps_ordinal_separate_from_snapshot_monitor_watermark() {
        let segment = |response_id: &str| DebugWsMessage::SegmentAppend {
            response_id: response_id.to_string(),
            segment: crate::monitor::DebugSegment {
                timestamp_ms: 123,
                kind: crate::monitor::DebugSegmentKind::Output,
                text: ".".to_string(),
            },
        };
        let snapshot = DebugSnapshot {
            last_sequence: 77,
            messages: vec![
                segment("resp_target"),
                segment("resp_other"),
                segment("resp_target"),
            ],
        };

        let (deltas, through_monitor_seq) = replay_deltas(Some("resp_target"), snapshot);

        assert_eq!(through_monitor_seq, 77);
        assert_eq!(
            deltas
                .iter()
                .map(|delta| delta.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "sequence stays the per-flow replay ordinal"
        );
        assert_eq!(deltas[0].ts_ms, Some(123));
        assert_eq!(deltas[1].ts_ms, Some(123));
        assert_eq!(
            deltas[0].payload, deltas[1].payload,
            "repeated content survives"
        );
    }

    /// Even a flow that is not linked yet carries the snapshot watermark. Otherwise a
    /// temporarily empty replay would append retained pre-snapshot live history as if it
    /// were new when the response id appears later.
    #[test]
    fn replay_deltas_without_response_id_still_returns_snapshot_watermark() {
        let (deltas, through_monitor_seq) = replay_deltas(
            None,
            DebugSnapshot {
                last_sequence: 9,
                messages: Vec::new(),
            },
        );
        assert!(deltas.is_empty());
        assert_eq!(through_monitor_seq, 9);
    }

    /// The `status=` filter parses the frozen open/completed/failed/cancelled enum
    /// and ignores an unrecognized value (a typo hides no rows).
    #[test]
    fn parse_status_filter_matches_the_frozen_enum() {
        assert_eq!(parse_status_filter("open"), Some(FlowStatus::Open));
        assert_eq!(
            parse_status_filter("CANCELLED"),
            Some(FlowStatus::Cancelled)
        );
        assert_eq!(parse_status_filter("bogus"), None);
    }

    /// Gap 07 — don't-lie-with-zeros for usage: an UNREPORTED (`None`) cached/reasoning
    /// class is ABSENT on the wire (never a fabricated `0`), and a provider-REPORTED `0`
    /// is a present, measured `0` — the two are DISTINCT. Round-trips the changed
    /// `FlowUsage` field through serialize → JSON (AGENTS.md: no changed wire field
    /// without a proof) at the `FlowRow` projection the frontend reads.
    #[test]
    fn flow_usage_unreported_class_is_absent_measured_zero_is_present() {
        let row = |cached: Option<i64>, reasoning: Option<i64>| FlowRow {
            revision: 1,
            api_call_id: "api_u".to_string(),
            response_id: None,
            method: "POST".to_string(),
            uri: "/v1/responses".to_string(),
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: Some(FlowUsage {
                prompt: 100,
                completion: 40,
                total: 140,
                cached,
                reasoning,
            }),
            normalized_usage: None,
            usage_anomaly_count: 0,
            effective_route_limit: None,
            cache_price_impact_usd: None,
            status: FlowStatus::Completed,
            started_ms: 0,
            finished_ms: None,
            elapsed_ms: None,
            terminal_reason: None,
            client_label: None,
            client_source: None,
            cost: None,
            cost_confidence: CostConfidence::Unavailable,
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
        };

        // UNREPORTED cached + reasoning ⇒ both keys OMITTED on the usage object (the
        // frontend renders `—`), never a fake `0`. prompt/completion/total stay present.
        let value = serde_json::to_value(row(None, None)).expect("serialize unreported");
        let usage = value["usage"].as_object().expect("usage object");
        assert!(
            !usage.contains_key("cached"),
            "unreported cached is ABSENT (unavailable), not 0: {usage:?}"
        );
        assert!(
            !usage.contains_key("reasoning"),
            "unreported reasoning is ABSENT (unavailable), not 0"
        );
        assert_eq!(usage["prompt"], serde_json::json!(100));
        assert_eq!(usage["total"], serde_json::json!(140));

        // A provider-REPORTED 0 is a PRESENT measured `0` — DISTINCT from absent.
        let value = serde_json::to_value(row(Some(0), Some(0))).expect("serialize zero");
        let usage = value["usage"].as_object().expect("usage object");
        assert_eq!(
            usage["cached"],
            serde_json::json!(0),
            "a reported cached=0 is a present measured 0 (≠ unavailable)"
        );
        assert_eq!(usage["reasoning"], serde_json::json!(0));
    }

    /// Gap 07 — cost CONFIDENCE tier rules (spec 07 acceptance). Reuses `cost_confidence`
    /// directly so each branch is pinned:
    /// - unpriced ⇒ `unavailable` (cost is `None`, never a fabricated 0);
    /// - priced + reported `cached = 0` ⇒ `confident` even with NO configured cache rate;
    /// - priced + `cached > 0`/UNREPORTED + NO configured cache rate ⇒ `estimated`;
    /// - priced + `cached > 0`/UNREPORTED + a CONFIGURED cache rate ⇒ `confident`.
    #[test]
    fn cost_confidence_tiers_match_the_spec() {
        let priced_no_cache = ModelPrice::without_cached(2.0, 6.0);
        let priced_with_cache = ModelPrice::new(2.0, 6.0, 0.5);
        let some = |cached: Option<i64>| {
            Some(FlowUsage {
                prompt: 100,
                completion: 40,
                total: 140,
                cached,
                reasoning: Some(0),
            })
        };

        // Unpriced ⇒ unavailable regardless of usage.
        assert_eq!(
            cost_confidence(None, some(Some(10))),
            CostConfidence::Unavailable
        );
        // Priced + reported cached = 0 ⇒ confident (nothing bills at the cache rate),
        // even though this price has NO configured cache rate.
        assert_eq!(
            cost_confidence(Some(priced_no_cache), some(Some(0))),
            CostConfidence::Confident,
            "a reported cached=0 stays confident"
        );
        // Priced + cached > 0 + NO configured cache rate ⇒ estimated (those tokens would
        // silently bill at the default 0.0).
        assert_eq!(
            cost_confidence(Some(priced_no_cache), some(Some(10))),
            CostConfidence::Estimated,
            "cached>0 with no configured cache rate ⇒ estimated"
        );
        // Priced + UNREPORTED cached + NO configured cache rate ⇒ estimated.
        assert_eq!(
            cost_confidence(Some(priced_no_cache), some(None)),
            CostConfidence::Estimated,
            "unreported cached with no configured cache rate ⇒ estimated"
        );
        // Priced + cached > 0 + a CONFIGURED cache rate ⇒ confident (priced honestly).
        assert_eq!(
            cost_confidence(Some(priced_with_cache), some(Some(10))),
            CostConfidence::Confident,
            "a configured cache rate prices cached>0 confidently"
        );
        // Priced + UNREPORTED cached + a CONFIGURED cache rate ⇒ still confident.
        assert_eq!(
            cost_confidence(Some(priced_with_cache), some(None)),
            CostConfidence::Confident
        );
    }

    /// Gap 07 — the AGGREGATE window cost confidence is `estimated` if ANY priced bucket
    /// would silently bill cached at the default `0.0`: a window with one priced flow
    /// whose cached was UNREPORTED (against a model with no configured cache rate) reports
    /// `cost_confidence: estimated` even though the summed `cached_tokens == 0` — no
    /// silently-confident total. A window with only a reported-cached-0 priced flow stays
    /// `confident`; an unpriced-only window is `unavailable`.
    #[test]
    fn window_cost_confidence_aggregates_estimated() {
        use crate::dashboard_flow::FlowStatus as FS;
        use crate::metrics::MetricsLayer;
        let mut prices = HashMap::new();
        prices.insert("priced".to_string(), ModelPrice::without_cached(2.0, 6.0));

        // (a) one priced flow with UNREPORTED cached → estimated aggregate.
        let metrics = MetricsLayer::new();
        record_terminal_cost(
            &metrics,
            "priced",
            Some(FlowUsage {
                prompt: 1000,
                completion: 500,
                total: 1500,
                cached: None, // unreported
                reasoning: Some(0),
            }),
            Some(5.0),
            crate::dashboard_flow::TerminalCostConfidence::Estimated,
        );
        let body = rest_window_tile(&metrics.view().window_1m, 60.0, 0, &prices);
        assert_eq!(
            body.cost_confidence,
            CostConfidence::Estimated,
            "unreported cached on a no-cache-rate model ⇒ estimated aggregate (summed cached==0)"
        );
        assert_eq!(body.cost_confidence, CostConfidence::Estimated);

        // (b) one priced flow with a REPORTED cached=0 → confident aggregate.
        let metrics = MetricsLayer::new();
        record_terminal_cost(
            &metrics,
            "priced",
            Some(FlowUsage {
                prompt: 1000,
                completion: 500,
                total: 1500,
                cached: Some(0), // reported zero
                reasoning: Some(0),
            }),
            Some(5.0),
            crate::dashboard_flow::TerminalCostConfidence::Confident,
        );
        let body = rest_window_tile(&metrics.view().window_1m, 60.0, 0, &prices);
        assert_eq!(
            body.cost_confidence,
            CostConfidence::Confident,
            "a reported cached=0 keeps the aggregate confident"
        );

        // (c) an unpriced-only window ⇒ unavailable (cost_per_min renders —).
        let metrics = MetricsLayer::new();
        metrics.record_terminal(
            FS::Completed,
            Some("free"),
            "/v1/responses",
            Some("vllm-a"),
            900,
            Some(usage(10, 5, 0)),
            &[],
        );
        let body = rest_window_tile(&metrics.view().window_1m, 60.0, 0, &prices);
        assert_eq!(body.cost_confidence, CostConfidence::Unavailable);
    }

    /// Gap 07 review round 1, finding 2 — a MIXED window (one CONFIDENT priced bucket
    /// PLUS one UNPRICED usage-bearing bucket) is `estimated`, NOT `confident`: the
    /// unpriced bucket's real spend is OMITTED from `cost_per_min`, so the total is a
    /// PARTIAL undercount of the window's true cost — a silently-confident total the
    /// spec forbids. It stays `estimated` (not `unavailable`) because the priced bucket
    /// makes `cost_per_min` a real, rendered number. A contrast case proves a usage-LESS
    /// unpriced bucket (a flow that reported NO tokens) does NOT taint a confident window
    /// (it adds no missing cost), and `unavailable` is still reserved for an all-unpriced
    /// window.
    #[test]
    fn window_cost_confidence_mixed_priced_and_unpriced_usage_is_estimated() {
        use crate::dashboard_flow::FlowStatus as FS;
        use crate::metrics::MetricsLayer;
        // The priced model has a CONFIGURED cache rate, so its own bucket is confident —
        // isolating the unpriced-bucket effect (no cached-rate fallback confounder).
        let mut prices = HashMap::new();
        prices.insert("priced".to_string(), ModelPrice::new(2.0, 6.0, 1.0));

        // (d) confident priced bucket (reported cached=0, configured cache rate) PLUS an
        // unpriced usage-bearing bucket ⇒ estimated (partial total).
        let metrics = MetricsLayer::new();
        record_terminal_cost(
            &metrics,
            "priced",
            Some(FlowUsage {
                prompt: 1000,
                completion: 500,
                total: 1500,
                cached: Some(0), // reported zero ⇒ this bucket alone is confident
                reasoning: Some(0),
            }),
            Some(5.0),
            crate::dashboard_flow::TerminalCostConfidence::Confident,
        );
        metrics.record_terminal(
            FS::Completed,
            Some("free"), // unpriced, but it carried real usage
            "/v1/responses",
            Some("vllm-b"),
            500,
            Some(usage(800, 400, 0)),
            &[],
        );
        let body = rest_window_tile(&metrics.view().window_1m, 60.0, 0, &prices);
        assert_eq!(
            body.cost_confidence,
            CostConfidence::Estimated,
            "a priced-confident bucket + an unpriced USAGE-BEARING bucket ⇒ estimated \
             (the unpriced spend is omitted from cost_per_min — a partial total)"
        );
        assert_eq!(body.cost_confidence, CostConfidence::Estimated);
        // The priced bucket makes the total a real number (NOT unavailable): a priced
        // sample exists, so $/min renders.
        assert_eq!(
            body.priced_samples, 1,
            "exactly the priced bucket is countable"
        );
        assert!(
            body.cost_per_min.is_some_and(|cost| cost > 0.0),
            "cost_per_min is a real (if partial) number, so estimated — not unavailable"
        );

        // (e) confident priced bucket PLUS a usage-LESS unpriced bucket (no tokens
        // reported — e.g. a failure) ⇒ STILL confident: the unpriced bucket adds no
        // missing cost, so the total is complete.
        let metrics = MetricsLayer::new();
        record_terminal_cost(
            &metrics,
            "priced",
            Some(FlowUsage {
                prompt: 1000,
                completion: 500,
                total: 1500,
                cached: Some(0),
                reasoning: Some(0),
            }),
            Some(5.0),
            crate::dashboard_flow::TerminalCostConfidence::Confident,
        );
        // A terminal flow on an unpriced model that reported NO usage (usage_samples == 0
        // for its bucket): bumps the count but contributes no token throughput/cost.
        metrics.record_terminal(
            FS::Failed,
            Some("free"),
            "/v1/responses",
            Some("vllm-b"),
            120,
            None,
            &[],
        );
        let body = rest_window_tile(&metrics.view().window_1m, 60.0, 0, &prices);
        assert_eq!(
            body.cost_confidence,
            CostConfidence::Confident,
            "a usage-LESS unpriced bucket adds no missing cost ⇒ the window stays confident"
        );
    }

    /// Gap 12 — a minimal `ProviderHealth` for the topology DTO tests.
    fn provider_health(id: &str) -> crate::upstream::ProviderHealth {
        crate::upstream::ProviderHealth {
            id: id.to_string(),
            name: id.to_string(),
            route: None,
            base_url: "https://example.invalid/v1".to_string(),
            status: crate::upstream::ProviderStatus::Healthy,
            cooling_until_ms: None,
            last_error: None,
            served_count: 0,
            failover_count: 0,
            consecutive_failures: 0,
            catalog_fetched_ms: None,
            catalog_size: None,
        }
    }

    /// Gap 12 (AGENTS.md changed-wire-field rule): the per-provider latency/error metrics
    /// are projected onto the `/topology` node as an ADDITIVE `per_provider` field and
    /// survive a JSON round-trip. A provider WITH in-window attempt samples carries a
    /// `derived` tile (real p50/p95/p99 + error rate + the bounded error distribution); a
    /// provider with NO samples OMITS the field entirely (don't-lie-with-zeros — absent,
    /// never a fabricated `0ms`/`0%`), leaving the frozen `TopologyNode` contract intact.
    #[test]
    fn topology_body_projects_per_provider_metrics_and_omits_zero_sample_nodes() {
        use crate::dashboard_flow::AttemptErrorClass;
        use crate::dashboard_flow::AttemptStatus;
        use crate::metrics::MetricsLayer;

        let metrics = MetricsLayer::new();
        // provider-a is hit by a failed primary then a served-elsewhere flow; provider-b
        // never appears in any attempt (a configured-but-idle provider).
        let attempts = vec![
            Attempt {
                provider: Some("provider-a".to_string()),
                model: Some("m".to_string()),
                start_ms: 1_000,
                end_ms: 1_080,
                duration_ms: Some(80),
                first_upstream_byte_ms: None,
                first_upstream_byte_offset_ms: None,
                status: AttemptStatus::Failed,
                error_class: Some(AttemptErrorClass::HttpStatus),
                failover_reason: Some(crate::dashboard_flow::AttemptFailoverReason::ProviderFailed),
            },
            Attempt {
                provider: Some("provider-c".to_string()),
                model: Some("m".to_string()),
                start_ms: 1_000,
                end_ms: 1_040,
                duration_ms: Some(40),
                first_upstream_byte_ms: Some(1_040),
                first_upstream_byte_offset_ms: Some(40),
                status: AttemptStatus::Served,
                error_class: None,
                failover_reason: None,
            },
        ];
        metrics.record_terminal(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("provider-c"),
            40,
            None,
            &attempts,
        );

        let snapshot = ProviderHealthSnapshot {
            version: 7,
            providers: vec![provider_health("provider-a"), provider_health("provider-b")],
        };
        let prices: HashMap<String, ModelPrice> = HashMap::new();
        let backend = crate::backend_metrics::BackendMetricsSnapshot {
            seq: 9,
            generated_at_ms: 1_000,
            providers: BTreeMap::from([(
                crate::backend_metrics::LogicalProviderKey {
                    route: None,
                    provider_id: "provider-a".to_string(),
                },
                crate::backend_metrics::BackendProviderMetrics {
                    engine_kind: crate::backend_metrics::BackendEngineKind::Vllm,
                    status: crate::backend_metrics::BackendMetricsStatus::Fresh,
                    coverage: crate::backend_metrics::BackendMetricsCoverage::Full,
                    scraped_at_ms: Some(1_000),
                    last_success_ms: Some(1_000),
                    last_error_class: None,
                    instant: crate::backend_metrics::BackendInstantMetrics::default(),
                    windows: crate::backend_metrics::BackendMetricWindows::default(),
                },
            )]),
            overflow_count: 0,
            engine_throughput: None,
        };
        let body = topology_body(&snapshot, &prices, &metrics.view().window_1m, &backend);

        let value = serde_json::to_value(&body).expect("serialize topology body");
        let nodes = value["nodes"].as_array().expect("nodes array");
        let node_a = nodes
            .iter()
            .find(|n| n["id"] == serde_json::json!("provider-a"))
            .expect("provider-a node");
        let node_b = nodes
            .iter()
            .find(|n| n["id"] == serde_json::json!("provider-b"))
            .expect("provider-b node");

        // provider-a has a sample → a `derived` per_provider tile with real values.
        let per_a = &node_a["per_provider"];
        assert_eq!(per_a["data_quality"], serde_json::json!("derived"));
        assert_eq!(per_a["provider"], serde_json::json!("provider-a"));
        assert_eq!(per_a["samples"], serde_json::json!(1));
        assert_eq!(per_a["failed"], serde_json::json!(1));
        assert_eq!(per_a["error_rate"], serde_json::json!(100.0));
        assert_eq!(per_a["errors"]["http_status"], serde_json::json!(1));
        assert_eq!(per_a["p50"], serde_json::Value::Null);
        assert_eq!(per_a["p95"], serde_json::Value::Null);
        assert_eq!(per_a["p99"], serde_json::Value::Null);
        assert_eq!(node_a["engine_metrics"]["engine_kind"], "vllm");

        // provider-b has NO samples → the field is ABSENT (don't-lie-with-zeros).
        assert!(
            node_b.get("per_provider").is_none(),
            "a zero-sample provider omits per_provider entirely (unavailable, not 0)"
        );
        assert!(node_b.get("engine_metrics").is_none());

        // The per_provider tile round-trips back into the typed DTO.
        let typed: crate::metrics::ProviderLatency =
            serde_json::from_value(per_a.clone()).expect("deserialize ProviderLatency");
        assert_eq!(typed.provider, "provider-a");
        assert_eq!(typed.failed, 1);
        assert_eq!(typed.errors.http_status, 1);
    }
}
