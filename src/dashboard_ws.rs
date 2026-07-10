//! `/dashboard/ws` — the batched dashboard WebSocket envelope (D7, stage D7b).
//!
//! This module owns the dashboard data socket: the batched [`DashboardFrame`]
//! wire envelope, its [`DashboardPayload`] arms, the per-domain `{domain, seq}`
//! stamping, and the `/dashboard/ws` handler (auth + `Origin` + cookie-`exp`
//! close, reusing D7a's [`crate::dashboard_auth::DashboardAuth::authenticate_ws`]).
//!
//! ## Why a BATCHED envelope (the bug it fixes)
//! `MonitorHub` emits a [`crate::monitor::DebugUpdate`] carrying a
//! `Vec<DebugWsMessage>` under ONE `sequence` (monitor.rs). If each sibling
//! `DebugWsMessage` were wrapped in its own per-frame-sequenced envelope, the
//! client's per-domain whole-frame dedup (`seq <= last_seq[domain]` drops the
//! frame) would drop every sibling after the first — they all share the same
//! sequence. So the Monitor domain emits exactly ONE [`DashboardFrame`] per
//! `DebugUpdate` (`seq = DebugUpdate.sequence`, `batch` = its messages), and
//! whole-frame dedup then drops a WHOLE stale update, never a live sibling.
//!
//! ## Domain routing
//! Monitor and flow state use independent authoritative publishers. Every
//! `DebugUpdate` remains transcript-only in the Monitor domain, including its raw
//! usage/status messages. Flow-domain rows come directly from the FlowStore's
//! versioned mutation channel, carrying the exact post-mutation record plus its
//! global flow cursor and per-flow revision. No socket-time monitor→store join can
//! race terminal finalization or stamp a later record with an older event cursor.
//!
//! ## Sourcing each `DashboardPayload` arm
//! - `Monitor` ← `MonitorHub` (`DebugUpdate` batch), 1:1, nested + tagged.
//! - `FlowStatus` ← authoritative FlowStore mutation, projected as a full `FlowRow`.
//! - `MetricTick` ← the process-wide immutable metrics publisher cut (D5),
//!   `seq = metrics presentation seq`, flattened to the `/api/metrics` shape.
//! - `TopologyUpdate` ← the topology Arc carried by that same coordinated cut,
//!   `seq = ProviderHealthSnapshot.version`.
//!
//! ## `/debug/ws` is UNCHANGED
//! The bare `DebugWsMessage` contract on `/debug/ws` (debug_ui.rs) is untouched —
//! the batched envelope is dashboard-only.

use crate::dashboard_flow::DashboardFlowStore;
use crate::dashboard_flow::FlowMutation;
use crate::dashboard_flow::FlowMutationPhase;
use crate::engine::Gateway;
use crate::metrics::MetricsView;
use crate::monitor::DebugUpdate;
use crate::monitor::DebugWsMessage;
use crate::upstream::ProviderHealthSnapshot;
use axum::extract::State;
use axum::extract::ws::CloseFrame;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use futures::SinkExt;
use futures::StreamExt;
use futures::stream::SplitSink;
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::broadcast;

/// The explicit WS close code the dashboard SPA recognizes as a SESSION EXPIRY /
/// auth failure (`dashboard-frontend/src/api/ws.ts` `WS_AUTH_CLOSE`): on `4401` the
/// SPA bounces to login instead of treating the drop as a transient blip to probe +
/// reconnect (D7b R2 finding 3). EVERY expiry close path sends this code so a genuinely
/// expired session is never mistaken for a network blip and silently reconnected.
const WS_AUTH_CLOSE_CODE: u16 = 4401;

/// Explicit reconnectable close for a lagged bounded publisher. RFC 6455 code
/// 1013 asks the client to retry later; reconnect takes a fresh authoritative
/// snapshot instead of continuing after a cursor gap.
const WS_TRANSIENT_CLOSE_CODE: u16 = 1013;

// ---------------------------------------------------------------------------
// Wire envelope — the BATCHED DashboardFrame (matches the D9 golden fixtures
// in dashboard-frontend/src/api/ws.fixtures.ts byte-for-byte)
// ---------------------------------------------------------------------------

/// The four per-domain cursors the dashboard tracks. Each [`DashboardFrame`]
/// carries exactly one, and the client dedups whole frames per-domain
/// (`seq <= last_seq[domain]` drops the batch). Serializes snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    Flow,
    Metrics,
    Topology,
    Monitor,
}

/// The batched WS envelope: ONE frame per source update (e.g. one `DebugUpdate`),
/// carrying the originating domain, that domain's sequence at the cut, and the
/// batch of payloads. Per-domain whole-frame dedup on the client drops the WHOLE
/// `batch` when `seq <= last_seq[domain]`, so a batched Monitor frame never loses
/// a sibling to dedup.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DashboardFrame {
    pub domain: Domain,
    pub seq: u64,
    pub batch: Vec<DashboardPayload>,
}

/// The four per-domain cursors carried on the initial [`SnapshotMessage`] — the
/// `{flow,metrics,topology,monitor}` sequences the SPA installs as its dedup
/// baseline (`commitSnapshot` in `dashboard-frontend/src/api/ws.ts`). Serializes
/// snake_case to the frozen `SeqCursors` contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SeqCursors {
    pub flow_seq: u64,
    pub metrics_seq: u64,
    pub topology_seq: u64,
    pub monitor_seq: u64,
}

/// The full `/api/metrics`-shaped snapshot body (the flat tile + the three
/// windows) PLUS its `metrics_seq` cursor — the snapshot-time analogue of a live
/// [`DashboardPayload::MetricTick`]. Mirrors the frontend `MetricsResponse`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MetricsSnapshot {
    pub metrics_seq: u64,
    pub reqs_per_sec: f64,
    pub active_streams: u64,
    pub error_pct: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub tokens_per_sec: f64,
    pub cost_per_min: f64,
    /// Terminal-flow sample count of the headline (`m1`) window — the
    /// measured/unavailable signal for latency/error, mirrored from `windows.m1.samples`.
    pub samples: u64,
    /// Headline (`m1`) usage-sample count — the `tokens_per_sec` measurability
    /// denominator, mirrored from `windows.m1.usage_samples` (gap 01 finding 3).
    pub usage_samples: u64,
    /// Headline (`m1`) priced-usage-sample count — the `cost_per_min` measurability
    /// denominator, mirrored from `windows.m1.priced_samples` (gap 01 finding 3).
    pub priced_samples: u64,
    /// Headline (`m1`) aggregate cost confidence (gap 07), mirrored from
    /// `windows.m1.cost_confidence` — so the headline `$/min` is labelled estimated
    /// when any priced bucket bills cached at the default `0.0`.
    pub cost_confidence: crate::dashboard_api::CostConfidence,
    pub windows: MetricWindows,
}

/// The full `/api/topology`-shaped snapshot body (nodes + edges + the price table)
/// PLUS its `topology_seq` cursor. Mirrors the frontend `TopologyResponse`. The
/// price table is empty until D13 wires the price config; an empty map satisfies
/// the frontend `isPriceTable` guard (vacuously every value is a finite price).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TopologySnapshot {
    pub topology_seq: u64,
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
    pub price_table: std::collections::BTreeMap<String, ModelPrice>,
}

/// One model's price row (`/api/topology` `price_table` value). The crate's
/// SINGLE `ModelPrice` definition lives in [`crate::config`] — it is BOTH the
/// config-loaded type AND the wire type, so REST `/dashboard/api/topology` and
/// this WS topology snapshot serialize the SAME shape — re-exported here so the
/// `TopologySnapshot` field type reads naturally and D13 can populate it from the
/// same `Config::price_table` source. All three rates are finite (the frontend
/// `isModelPrice` guard rejects NaN/Inf).
pub use crate::config::ModelPrice;

/// The INITIAL WS message: a `type:"snapshot"` envelope the SPA waits for BEFORE
/// it renders. The frontend (`dashboard-frontend/src/api/ws.ts`) BUFFERS every
/// live [`DashboardFrame`] until this lands (`snapshotApplied`), so it MUST be the
/// FIRST frame on a `/dashboard/ws` connection — else the dashboard never renders
/// (D7b R1 finding 1). It seeds the store's cursors + flow rows + metrics/topology
/// baseline in one atomic install (`restoreLiveSnapshot`); subsequent live frames
/// build on it. Internally tagged `type:"snapshot"` to match the frozen
/// `SnapshotFrame` discriminant.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SnapshotMessage {
    /// Discriminant — always `"snapshot"`; the SPA routes on it.
    #[serde(rename = "type")]
    pub kind: SnapshotTag,
    /// Dashboard contract version. The SPA verifies this before installing any
    /// cursor or opening the live pipeline.
    pub schema_version: u32,
    pub cursors: SeqCursors,
    /// Wire-facing flow ROWS (gap 10b `FlowRow`), NOT raw `SnapshotFlowSummary`s: the SPA's
    /// `isSnapshotFrame` validates every row with the same guard as `/flows` (gap 07 requires
    /// `cost_confidence` on EVERY row), so the WS snapshot must carry the same projection as
    /// the REST reads — a raw summary (no cost fields) fails validation and the SPA then
    /// silently drops the snapshot and sits at `connecting` forever, shadow-buffering frames.
    pub flows: Vec<crate::dashboard_api::FlowRow>,
    /// Metrics baseline (or `null` when metrics are disabled).
    pub metrics: Option<MetricsSnapshot>,
    /// Topology baseline (or `null` when no providers are published yet).
    pub topology: Option<TopologySnapshot>,
}

/// The literal `"snapshot"` tag for [`SnapshotMessage::kind`] (a unit enum so the
/// value is fixed at the type level and serializes to exactly that string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTag {
    Snapshot,
}

/// One dashboard payload. Internally `type`-tagged (snake_case) to match the
/// frozen contract. The `Monitor` arm NESTS the real (itself-tagged)
/// [`DebugWsMessage`] under `message` — it is NOT flattened (both carry `type`).
/// The `usage`/`flow_status` arms are keyed by `api_call_id` (authoritative) with
/// an optional secondary `response_id`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DashboardPayload {
    /// One per `DebugWsMessage` in the originating `DebugUpdate` batch; the real
    /// message is nested under `message` (itself `type`-tagged).
    Monitor { message: DebugWsMessage },
    /// Schema-v1-compatible cumulative usage arm. New sockets do not emit this
    /// separately: each authoritative `FlowStatus` carries the full row including
    /// usage. Retaining the arm keeps additive schema-v2 compatibility for recorded
    /// frames and older clients.
    ///
    /// Gap 07 review round 1, finding 1 — `cached`/`reasoning` are `Option<i64>`
    /// serialized with `skip_serializing_if`, mirroring [`FlowUsage`] (and the frontend
    /// `UsagePayload`): an UNREPORTED class is ABSENT on the wire, DISTINCT from a
    /// provider-reported `0`. They are SOURCED from the authoritative
    /// [`FlowRecord::usage`] (the honest `Option<i64>`), NOT from the monitor
    /// [`DebugWsMessage::Usage`] (which collapses an unreported class to a bare `0` — the
    /// BARE `/debug/ws` contract that AGENTS.md freezes integer-only). The always-present
    /// `prompt`/`completion`/`total` stay the monitor's freshest cumulative counts. This
    /// stops a live dashboard row showing an unavailable token class as a measured zero.
    Usage {
        api_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        prompt: i64,
        completion: i64,
        total: i64,
        /// Cache-read prompt tokens; `Some(n)` measured (incl. a reported `0`), `None`
        /// (absent on the wire) ⇒ the upstream did not report a cached breakdown.
        #[serde(skip_serializing_if = "Option::is_none")]
        cached: Option<i64>,
        /// Reasoning tokens; `Some(n)` measured (incl. a reported `0`), `None` (absent on
        /// the wire) ⇒ the upstream did not report reasoning details.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<i64>,
    },
    /// The flat `/api/metrics`-shaped metric tile (metrics domain).
    MetricTick(MetricTick),
    /// Authoritative per-flow mutation. The complete [`FlowRow`] is flattened to
    /// preserve the schema-v1 field locations while adding revision/cost/attribution
    /// and the bounded mutation `phase`. It is built from the exact record snapshot
    /// carried by the FlowStore broadcast, never by joining a monitor event later.
    FlowStatus {
        phase: FlowMutationPhase,
        /// Flattening keeps `api_call_id`, status, usage, timing, and all previous
        /// flow-status keys at their established top-level wire locations.
        #[serde(flatten)]
        row: Box<crate::dashboard_api::FlowRow>,
    },
    /// The provider topology cut (topology domain): nodes (D4 `ProviderHealth`,
    /// `catalog_size` flattened to a non-null count) + gateway→provider edges.
    TopologyUpdate {
        nodes: Vec<TopologyNode>,
        edges: Vec<TopologyEdge>,
    },
}

/// The flat metric-tile shape carried by a `metric_tick` payload — mirrors the
/// `/dashboard/api/metrics` REST body (sans cursor). The top level repeats the
/// `m1` window's fields (the dashboard's headline tile) and nests all three
/// windows under `windows`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MetricTick {
    pub reqs_per_sec: f64,
    pub active_streams: u64,
    pub error_pct: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub tokens_per_sec: f64,
    pub cost_per_min: f64,
    /// Terminal-flow sample count of the headline (`m1`) window — the
    /// measured/unavailable signal for latency/error, mirrored from `windows.m1.samples`.
    pub samples: u64,
    /// Headline (`m1`) usage-sample count — the `tokens_per_sec` measurability
    /// denominator, mirrored from `windows.m1.usage_samples` (gap 01 finding 3).
    pub usage_samples: u64,
    /// Headline (`m1`) priced-usage-sample count — the `cost_per_min` measurability
    /// denominator, mirrored from `windows.m1.priced_samples` (gap 01 finding 3).
    pub priced_samples: u64,
    /// Headline (`m1`) aggregate cost confidence (gap 07), mirrored from
    /// `windows.m1.cost_confidence` — so the headline `$/min` is labelled estimated
    /// when any priced bucket bills cached at the default `0.0`.
    pub cost_confidence: crate::dashboard_api::CostConfidence,
    pub windows: MetricWindows,
}

/// The three sliding windows (`m1`/`m5`/`h1`) of a [`MetricTick`].
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MetricWindows {
    pub m1: MetricWindow,
    pub m5: MetricWindow,
    pub h1: MetricWindow,
}

/// One sliding-window metric tile. Same fields as the headline tile.
///
/// `samples` is the count of TERMINAL (finalized) flows that fell in the window —
/// the data-quality signal the frontend uses to tell a genuine measured `0` from an
/// `unavailable` gap (gap 01 / "don't lie with zeros"). When `samples == 0` the
/// latency/tok-s/cost/error-% fields are NOT measurable (no finalized flow fed them),
/// so the strip renders them `—`; `reqs_per_sec` (a genuine `0` for an idle window)
/// and `active_streams` (live open-flow count) stay numeric. The field is a finite
/// `u64`, so it never violates the frozen finite-number wire contract.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct MetricWindow {
    pub reqs_per_sec: f64,
    pub active_streams: u64,
    pub error_pct: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub tokens_per_sec: f64,
    pub cost_per_min: f64,
    /// Terminal-flow sample count in this window (the measured/unavailable signal for
    /// latency + error-%). `0` ⇒ no finalized flow fed the latency/error fields ⇒ they
    /// render `—`.
    pub samples: u64,
    /// Count of those terminal flows that reported token usage (gap 01 review round 1,
    /// finding 3) — the SEPARATE `tokens_per_sec` measurability denominator. Token and
    /// cost availability are NOT the same as `samples`: a window can have `samples > 0`
    /// yet `usage_samples == 0` (every finalized flow omitted usage), in which case
    /// `tokens_per_sec`/`cost_per_min` are unmeasurable and render `—`, never a fake `0`.
    pub usage_samples: u64,
    /// Count of usage-bearing terminal flows whose served model has a configured price
    /// (gap 01 finding 3) — the `cost_per_min` measurability denominator. `0` ⇒ no
    /// PRICED usage in the window ⇒ `cost_per_min` renders `—`, distinguishing an
    /// unpriced model from a genuine measured `$0.00`. All three are finite `u64`s, so
    /// they never violate the frozen finite-number wire contract.
    pub priced_samples: u64,
    /// Gap 07 — the aggregate [`CostConfidence`](crate::dashboard_api::CostConfidence)
    /// of this window's `cost_per_min`. `unavailable` when nothing in the window is
    /// priced (`priced_samples == 0` ⇒ `cost_per_min` renders `—`); `estimated` when
    /// ANY priced bucket would bill cached tokens at the default `0.0` (cached `> 0` or
    /// UNREPORTED against a model with no configured cache rate) — no silently-confident
    /// total; `confident` only when every priced bucket's billed classes have known
    /// rates. Surfaced so the strip can LABEL an estimated cost as such.
    pub cost_confidence: crate::dashboard_api::CostConfidence,
}

/// A topology node — the D4 `ProviderHealth` shape, except `catalog_size` is
/// flattened from `Option<u64>` to a non-null `u64` (defaulting `None → 0`): the
/// frozen frontend contract validates `catalog_size` as a required unsigned int
/// (NOT nullable), unlike the other `Option` fields which serde emits as `null`.
/// Every other field mirrors `ProviderHealth` exactly (keys always present, the
/// nullable ones as JSON `null`).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TopologyNode {
    pub id: String,
    pub name: String,
    pub route: Option<String>,
    pub base_url: String,
    pub status: crate::upstream::ProviderStatus,
    pub cooling_until_ms: Option<u64>,
    pub last_error: Option<String>,
    pub served_count: u64,
    pub failover_count: u64,
    pub consecutive_failures: u32,
    pub catalog_fetched_ms: Option<u64>,
    /// Flattened from `ProviderHealth::catalog_size: Option<u64>` to a required
    /// non-null count (`None → 0`) per the frozen contract.
    pub catalog_size: u64,
    /// Gap 12 — the ADDITIVE per-provider latency (p50/p95/p99) + error distribution for
    /// this provider over the m1 window, aggregated off the evict-safe per-attempt trace
    /// (spec 03), NOT the point-in-time `ProviderHealth` counters. `None`/ABSENT when the
    /// provider had ZERO attempt samples in the window (don't-lie-with-zeros — the
    /// frontend renders `—`, never a fabricated `0ms`/`0%`). `skip_serializing_if` keeps
    /// the field off the wire entirely for a no-sample node, so the EXISTING frozen
    /// `TopologyNode` contract (D9/D10/D12) is undisturbed; spec 13 reads this field. The
    /// LIVE WS topology frame leaves it `None` (the WS frame does not join metrics, like
    /// its `0.0` edge rates) — it is populated only on the REST `/topology` + `/snapshot`
    /// reshape, which already join the m1 window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_provider: Option<crate::metrics::ProviderLatency>,
}

impl TopologyNode {
    /// Project a D4 [`ProviderHealth`](crate::upstream::ProviderHealth) into a
    /// topology node (`catalog_size` flattened `None → 0`). `pub(crate)` so D13's
    /// REST `/topology` builds the SAME node shape as the WS topology frame. The gap-12
    /// `per_provider` metrics are left `None` here (the live WS frame does not join the
    /// metrics window); [`from_health_with_metrics`](Self::from_health_with_metrics)
    /// populates them for the REST `/topology` + `/snapshot` reshape.
    pub(crate) fn from_health(health: &crate::upstream::ProviderHealth) -> Self {
        Self {
            id: health.id.clone(),
            name: health.name.clone(),
            route: health.route.clone(),
            base_url: health.base_url.clone(),
            status: health.status,
            cooling_until_ms: health.cooling_until_ms,
            last_error: health.last_error.clone(),
            served_count: health.served_count,
            failover_count: health.failover_count,
            consecutive_failures: health.consecutive_failures,
            catalog_fetched_ms: health.catalog_fetched_ms,
            // Contract: non-null required count; an unfetched catalog is 0, not null.
            catalog_size: health.catalog_size.unwrap_or(0),
            // Gap 12: no metrics join on this path — populated only via
            // `from_health_with_metrics` (REST `/topology` + `/snapshot`).
            per_provider: None,
        }
    }

    /// Gap 12 — like [`from_health`](Self::from_health) but ALSO attaches this provider's
    /// per-provider latency/error metrics from the m1 window, looked up by the provider's
    /// `id` (the SAME key the per-attempt trace records under). `None`/absent when the
    /// provider has no in-window attempt samples (don't-lie-with-zeros). Used by the REST
    /// `/topology` handler + the `/snapshot` reshape (which already join the m1 window for
    /// the edge rates), so the per-provider tiles ride the same metrics cut as the edges.
    pub(crate) fn from_health_with_metrics(
        health: &crate::upstream::ProviderHealth,
        window_1m: &crate::metrics::WindowReport,
    ) -> Self {
        let mut node = Self::from_health(health);
        node.per_provider = window_1m.provider_latency(&health.id);
        node
    }
}

/// A topology edge (gateway → provider). The aggregate throughput/token/cost
/// rates are D5/D13 roll-ups; until a price/throughput aggregation feeds them
/// they serialize as `0.0` (the contract requires the keys present + finite, not
/// a specific value), so the byte-shape is exact while the rich values land in
/// D13.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TopologyEdge {
    pub from: String,
    pub to: String,
    pub throughput: f64,
    pub tokens_per_sec: f64,
    pub cost_per_sec: f64,
}

// ---------------------------------------------------------------------------
// Frame builders (pure + unit-testable)
// ---------------------------------------------------------------------------

/// Build the single transcript-domain frame for one monitor update. Monitor
/// messages are no longer joined back to FlowStore records; usage/status remain in
/// the transcript exactly as `/debug/ws` emitted them, while authoritative flow
/// rows arrive independently from [`FlowMutation`] broadcasts.
pub fn frames_for_update(
    update: &DebugUpdate,
    _flow_store: &DashboardFlowStore,
) -> Vec<DashboardFrame> {
    if update.messages.is_empty() {
        return Vec::new();
    }
    vec![DashboardFrame {
        domain: Domain::Monitor,
        seq: update.sequence,
        batch: update
            .messages
            .iter()
            .cloned()
            .map(|message| DashboardPayload::Monitor { message })
            .collect(),
    }]
}

/// Project one authoritative FlowStore mutation to the complete row payload used
/// by REST and snapshots. Pricing happens against the gateway configuration at send
/// time, while every flow field comes from the exact record `Arc` in the event.
fn frame_for_flow_mutation(event: &FlowMutation, gateway: &Gateway) -> DashboardFrame {
    debug_assert_eq!(event.seq, event.record.record_seq);
    debug_assert_eq!(event.revision, event.record.revision);
    DashboardFrame {
        domain: Domain::Flow,
        seq: event.seq,
        batch: vec![DashboardPayload::FlowStatus {
            phase: event.phase,
            row: Box::new(crate::dashboard_api::FlowRow::from_record(
                &event.record,
                gateway,
            )),
        }],
    }
}

/// The next STRICTLY-MONOTONIC metrics-domain wire cursor (gap 01 review round 1,
/// finding 1). `view_seq` is the metrics ring's own `metrics_seq`; `last_emitted` is the
/// last cursor this connection put on the wire. Returns `max(view_seq, last_emitted + 1)`:
/// a genuine ring advance carries the true `view_seq`, while an active-stream-only change
/// (which does NOT bump `view_seq`) still advances the cursor by one so the frame is not
/// dropped as a same-seq duplicate by the client's per-domain `seq <= cursor` dedup. The
/// result is always `> last_emitted`, keeping the metrics domain's `{domain, seq}` cursor
/// monotonic WITHOUT a global watermark (AGENTS.md). `saturating_add` guards the (absurd)
/// `u64::MAX` edge so the cursor never wraps.
#[cfg(test)]
fn next_metrics_cursor(view_seq: u64, last_emitted: u64) -> u64 {
    view_seq.max(last_emitted.saturating_add(1))
}

/// Build a metrics-domain `MetricTick` frame from a collapsed [`MetricsView`]
/// (D5), the live open-flow `active_streams` count, and the price table.
///
/// Gap 01: the live tick is built from the SAME [`crate::dashboard_api::metrics_body`]
/// the REST `/dashboard/api/metrics` read uses — ONE honest computation for both
/// surfaces — so `active_streams`, `tokens_per_sec`, `cost_per_min`, and the TRUE
/// per-second `reqs_per_sec` carry real values on the live wire (previously this path
/// hard-coded `active_streams`/`tokens_per_sec`/`cost_per_min` to `0.0` and shipped raw
/// counts as `reqs_per_sec`, so the strip read all-`0` once it folded a WS tick even
/// while real traffic streamed). The single-CAS terminal feed stays idempotent; this
/// only changes how the already-recorded view is collapsed for the wire.
pub fn metric_tick_frame(
    view: &MetricsView,
    seq: u64,
    active_streams: u64,
    prices: &std::collections::HashMap<String, ModelPrice>,
) -> DashboardFrame {
    let body = crate::dashboard_api::metrics_body(view, seq, active_streams, prices);
    DashboardFrame {
        domain: Domain::Metrics,
        seq,
        batch: vec![DashboardPayload::MetricTick(MetricTick {
            reqs_per_sec: body.reqs_per_sec,
            active_streams: body.active_streams,
            error_pct: body.error_pct,
            p50: body.p50,
            p95: body.p95,
            p99: body.p99,
            tokens_per_sec: body.tokens_per_sec,
            cost_per_min: body.cost_per_min,
            samples: body.samples,
            usage_samples: body.usage_samples,
            priced_samples: body.priced_samples,
            cost_confidence: body.cost_confidence,
            windows: body.windows,
        })],
    }
}

/// Build a topology-domain `TopologyUpdate` frame from a D4
/// [`ProviderHealthSnapshot`]. The frame's `seq` is the snapshot `version`. Each
/// provider becomes a node; one gateway→provider edge is emitted per node (the
/// rate fields are D5/D13 roll-ups, `0.0` for now — shape exact).
pub fn topology_frame(snapshot: &ProviderHealthSnapshot) -> DashboardFrame {
    let nodes: Vec<TopologyNode> = snapshot
        .providers
        .iter()
        .map(TopologyNode::from_health)
        .collect();
    let edges: Vec<TopologyEdge> = snapshot
        .providers
        .iter()
        .map(|provider| TopologyEdge {
            from: "gateway".to_string(),
            to: provider.id.clone(),
            throughput: 0.0,
            tokens_per_sec: 0.0,
            cost_per_sec: 0.0,
        })
        .collect();
    DashboardFrame {
        domain: Domain::Topology,
        seq: snapshot.version,
        batch: vec![DashboardPayload::TopologyUpdate { nodes, edges }],
    }
}

/// Build the metrics half of the initial [`SnapshotMessage`] from a collapsed
/// [`MetricsView`] (D5) + its `metrics_seq` + the live open-flow `active_streams`
/// count + the price table. Same flat tile + three windows as a live
/// [`DashboardPayload::MetricTick`], with the cursor attached — and built by the SAME
/// [`crate::dashboard_api::metrics_body`] the REST read and the live tick use (gap 01),
/// so the initial snapshot's strip is honest from the first frame (real
/// `active_streams`/`tokens_per_sec`/`cost_per_min`/true rates), not a raw-count/`0.0`
/// placeholder the SPA would render before the first live tick.
fn metrics_snapshot(
    view: &MetricsView,
    metrics_seq: u64,
    active_streams: u64,
    prices: &std::collections::HashMap<String, ModelPrice>,
) -> MetricsSnapshot {
    crate::dashboard_api::metrics_body(view, metrics_seq, active_streams, prices)
}

/// Build the topology half of the initial [`SnapshotMessage`] from a D4
/// [`ProviderHealthSnapshot`]. Same nodes/edges as a live [`topology_frame`], with
/// the `topology_seq` cursor attached and an (empty until D13) `price_table`.
fn topology_snapshot(snapshot: &ProviderHealthSnapshot) -> TopologySnapshot {
    let nodes: Vec<TopologyNode> = snapshot
        .providers
        .iter()
        .map(TopologyNode::from_health)
        .collect();
    let edges: Vec<TopologyEdge> = snapshot
        .providers
        .iter()
        .map(|provider| TopologyEdge {
            from: "gateway".to_string(),
            to: provider.id.clone(),
            throughput: 0.0,
            tokens_per_sec: 0.0,
            cost_per_sec: 0.0,
        })
        .collect();
    TopologySnapshot {
        topology_seq: snapshot.version,
        nodes,
        edges,
        // D13 wires the real price config; an empty table is contract-valid.
        price_table: std::collections::BTreeMap::new(),
    }
}

/// Build the INITIAL `type:"snapshot"` message a fresh `/dashboard/ws` connection
/// MUST send FIRST (D7b R1 finding 1) — before any live [`DashboardFrame`]. The SPA
/// buffers every frame until this lands, so it seeds the whole baseline atomically:
/// the four per-domain cursors, the body-free flow rows, and the metrics/topology
/// cuts. Every cursor comes from the same authoritative read that built its domain
/// body; `flow_seq` is the FlowStore mutation cursor and is independent of the
/// monitor transcript cursor.
fn snapshot_message(
    flows: Vec<crate::dashboard_api::FlowRow>,
    flow_seq: u64,
    metrics: Option<MetricsSnapshot>,
    topology: Option<TopologySnapshot>,
    monitor_seq: u64,
) -> SnapshotMessage {
    SnapshotMessage {
        kind: SnapshotTag::Snapshot,
        schema_version: crate::dashboard_contracts::DASHBOARD_SCHEMA_VERSION,
        cursors: SeqCursors {
            flow_seq,
            metrics_seq: metrics.as_ref().map_or(0, |m| m.metrics_seq),
            topology_seq: topology.as_ref().map_or(0, |t| t.topology_seq),
            monitor_seq,
        },
        flows,
        metrics,
        topology,
    }
}

// ---------------------------------------------------------------------------
// /dashboard/ws handler
// ---------------------------------------------------------------------------

/// `GET /dashboard/ws` — the batched dashboard WebSocket. Mirrors `/debug/ws`'s
/// auth posture (D7a): the HTTP-layer scoping has attached the shared
/// [`crate::dashboard_auth::DashboardAuth`]; here we re-validate the signed
/// session cookie + the WS `Origin` allow-list (CSWSH defense) and capture the
/// cookie `exp` so a per-connection timer closes the socket at expiry. A request
/// that fails cookie+Origin is rejected `401 no-store` BEFORE the upgrade. The
/// bearer fallback is intentionally NOT honored for WS (browsers can't set
/// `Authorization` on a `WebSocket`).
pub async fn dashboard_ws(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let auth = match gateway.dashboard_auth() {
        Some(auth) => auth,
        // Unreachable (the route registers only when auth exists), but fail
        // closed rather than serving an unauthenticated socket.
        None => {
            return crate::dashboard_auth::no_store(
                (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
            );
        }
    };
    let Some(exp) = auth.authenticate_ws(&headers) else {
        return crate::dashboard_auth::no_store(
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        );
    };
    upgrade
        .on_upgrade(move |socket| dashboard_socket(socket, gateway, exp))
        .into_response()
}

/// Drive one `/dashboard/ws` connection: send the INITIAL `type:"snapshot"` message
/// FIRST (the SPA buffers every live frame until it lands — D7b R1 finding 1), then
/// replay the retained monitor transcript as batched frames, then multiplex the
/// authoritative flow publisher, live monitor transcript, and shared metrics/topology
/// publisher — all racing the cookie-`exp` close timer.
/// `session_exp == u64::MAX` (dev-open) yields an effectively-infinite timer.
async fn dashboard_socket(socket: WebSocket, gateway: Arc<Gateway>, session_exp: u64) {
    let flow_store = gateway.flow_store().clone();
    let Some(mut flow_rx) = flow_store.subscribe() else {
        return;
    };
    let Some(mut metrics_rx) = gateway.metrics().subscribe_published_metrics() else {
        return;
    };
    let mut monitor_rx = gateway.subscribe_monitor();
    let snapshot = gateway.debug_snapshot();

    // Split the socket so the loop can READ inbound alongside writing (D7b R2 finding
    // 4): without an inbound read, a browser-side close / peer disconnect is invisible
    // and this task + its broadcast receiver linger until the cookie `exp` — wasting a
    // receiver slot (broadcast lag pressure) and a task per dead connection. The read
    // half surfaces the peer's `Close`/EOF so we tear down PROMPTLY.
    let (mut sink, mut stream) = socket.split();

    // Arm the expiry timer BEFORE any send so a near-/already-expired cookie
    // closes the socket even mid-replay.
    let expiry = wait_for_session_expiry(session_exp);
    tokio::pin!(expiry);

    // -- (finding 1) The INITIAL snapshot message — the FIRST thing on the wire --
    // The SPA gates ALL live frames behind `snapshotApplied`, so this MUST precede
    // every `DashboardFrame`. It seeds the dedup cursors + flow rows + metrics/
    // topology baseline atomically. The metrics/topology cursors here are the live
    // watermarks the loop below resumes from, so the next published cut is the first
    // NEW frame (no redundant baseline frame, no self-dedup). The monitor cursor is
    // 0: the snapshot body carries NO transcript, so the retained-transcript replay
    // below (seq = `last_sequence`) is ACCEPTED, seeding the inspector history.
    //
    // (D7b R2 finding 2) Each domain's body + its dedup cursor are captured ATOMICALLY
    // (one lock hold per store), so the snapshot never pairs an older body with a newer
    // cursor — which would permanently dedup-drop that mutation's own live frame:
    //  - metrics/topology: one immutable process-published cut (shared with REST)
    //  - flows:    `snapshot_summaries_with_seq()` (body + authoritative FlowStore
    //              cursor under one lock; subscription happened before this read).
    // Subscribe BEFORE reading the baseline: a cut racing this read remains pending on
    // the watch receiver. The process publisher is the only presentation-seq allocator.
    let published = metrics_rx.borrow_and_update().clone();
    let (metrics, topology, mut last_topology_version) = if let Some(cut) = published {
        (
            Some(metrics_snapshot(
                &cut.view,
                cut.cursors.metrics_seq,
                cut.active_streams,
                gateway.price_table(),
            )),
            Some(topology_snapshot(&cut.topology)),
            cut.topology.version,
        )
    } else {
        // Manually-constructed gateways can omit the DI bootstrap cut. Absence is an
        // explicit unavailable baseline; never recompute stores or mint a presentation
        // cursor outside the sole process publisher. The watch receiver will deliver
        // the first real cut when one is published.
        (None, None, 0)
    };
    // Body-free flow summaries AND their authoritative FlowStore cursor are captured
    // under one lock. The receiver was subscribed first, so mutations racing this read
    // are either included in the snapshot (and deduped by this baseline) or queued with
    // a strictly newer FlowStore sequence.
    let (flow_summaries, flow_store_seq) = flow_store.snapshot_summaries_with_seq();
    let flow_rows: Vec<crate::dashboard_api::FlowRow> = flow_summaries
        .iter()
        .map(|summary| crate::dashboard_api::FlowRow::from_summary(summary, &gateway))
        .collect();
    let initial = snapshot_message(
        flow_rows,
        flow_store_seq,
        metrics,
        topology,
        // monitor baseline 0 — the transcript rides the replay frame below.
        0,
    );
    // Replay the retained monitor transcript as one monitor-only frame. Flow rows are
    // never reconstructed from this transcript; the FlowStore snapshot/event stream is
    // their sole authority.
    let snapshot_update = DebugUpdate {
        sequence: snapshot.last_sequence,
        messages: snapshot.messages.clone(),
    };
    let snapshot_frames = frames_for_update(&snapshot_update, &flow_store);
    // Send the snapshot FIRST, then the replay frames, racing expiry throughout
    // (finding 1: snapshot strictly precedes every frame).
    match send_initial(&initial, &snapshot_frames, expiry.as_mut(), &mut sink).await {
        SendOutcome::Completed => {}
        SendOutcome::Expired => {
            send_auth_close(&mut sink).await;
            return;
        }
        SendOutcome::Failed => return,
    }

    loop {
        tokio::select! {
            biased;
            // Session expired mid-connection: close the socket with the EXPLICIT 4401
            // auth-close code (finding 3) so the SPA bounces to login, not reconnects.
            _ = &mut expiry => {
                send_auth_close(&mut sink).await;
                return;
            }
            // (finding 4) Inbound from the peer: a `Close`, an EOF (`None`), or a read
            // error means the browser/proxy hung up — tear down NOW rather than lingering
            // until `exp`. We don't process inbound data frames (the dashboard socket is
            // server→client only); any inbound `Text`/`Binary`/`Ping`/`Pong` is ignored
            // and we keep serving (axum answers Pings at the protocol layer).
            inbound = stream.next() => {
                if inbound_is_terminal(&inbound) {
                    return;
                }
                // Non-terminal inbound (data/ping/pong): ignore, keep serving.
            }
            // Prefer authoritative row mutations over transcript traffic when both
            // channels are ready; a token-heavy monitor stream must not starve the
            // terminal FlowStore record.
            received = flow_rx.recv() => {
                match received {
                    Ok(event) if event.seq <= flow_store_seq => {}
                    Ok(event) => {
                        let frame = frame_for_flow_mutation(&event, &gateway);
                        match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut sink).await {
                            SendOutcome::Completed => {}
                            SendOutcome::Expired => {
                                send_auth_close(&mut sink).await;
                                return;
                            }
                            SendOutcome::Failed => return,
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        send_transient_close(&mut sink, "flow lag; resnapshot required").await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            received = monitor_rx.recv() => {
                match received {
                    // Dedup at the source against the replayed snapshot: an update
                    // already covered by the snapshot's last_sequence is skipped
                    // (the client would whole-frame-dedup it anyway).
                    Ok(update) if update.sequence <= snapshot.last_sequence => {}
                    Ok(update) => {
                        let frames = frames_for_update(&update, &flow_store);
                        match send_frames(&frames, expiry.as_mut(), &mut sink).await {
                            SendOutcome::Completed => {}
                            SendOutcome::Expired => {
                                send_auth_close(&mut sink).await;
                                return;
                            }
                            SendOutcome::Failed => return,
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        send_transient_close(&mut sink, "monitor lag; resnapshot required").await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            changed = metrics_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                // Clone the immutable shared cut before awaiting sends; never hold a watch borrow
                // across an await. One cut feeds metrics and any topology generation it contains.
                let Some(cut) = metrics_rx.borrow_and_update().clone() else {
                    continue;
                };
                let frame = metric_tick_frame(
                    &cut.view,
                    cut.cursors.metrics_seq,
                    cut.active_streams,
                    gateway.price_table(),
                );
                match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut sink).await {
                    SendOutcome::Completed => {}
                    SendOutcome::Expired => {
                        send_auth_close(&mut sink).await;
                        return;
                    }
                    SendOutcome::Failed => return,
                }
                if cut.topology.version != last_topology_version {
                    last_topology_version = cut.topology.version;
                    let frame = topology_frame(&cut.topology);
                    match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut sink).await {
                        SendOutcome::Completed => {}
                        SendOutcome::Expired => {
                            send_auth_close(&mut sink).await;
                            return;
                        }
                        SendOutcome::Failed => return,
                    }
                }
            }
        }
    }
}

/// The EXPLICIT `4401` auth/expiry close frame (D7b R2 finding 3). The dashboard SPA
/// (`ws.ts` `WS_AUTH_CLOSE`) treats `4401` as a confirmed session expiry and bounces to
/// login; an unclassified `Close(None)` (RFC `1005`/no code) is instead read as an
/// abnormal blip and reconnected — so an expired session would silently reconnect into
/// another rejection loop. Pure constructor so the code is unit-testable (the socket
/// loop that sends it can't be built off a real upgrade in a unit test).
fn auth_close_frame() -> Message {
    Message::Close(Some(CloseFrame {
        code: WS_AUTH_CLOSE_CODE,
        reason: "session expired".into(),
    }))
}

/// Reconnectable close used when a bounded broadcast receiver falls behind. The
/// reason names the lost domain for diagnostics; reconnect always begins with a
/// new multi-domain snapshot, so no partial replay is attempted.
fn transient_close_frame(reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code: WS_TRANSIENT_CLOSE_CODE,
        reason: reason.into(),
    }))
}

/// Send the [`auth_close_frame`] on EVERY expiry path (finding 3). Best-effort: the
/// peer may already be gone, and errors are ignored since we are tearing down anyway.
async fn send_auth_close(sink: &mut SplitSink<WebSocket, Message>) {
    let _ = sink.send(auth_close_frame()).await;
}

async fn send_transient_close(sink: &mut SplitSink<WebSocket, Message>, reason: &'static str) {
    let _ = sink.send(transient_close_frame(reason)).await;
}

/// Classify an inbound WS poll (`stream.next()`) into "stop serving?" (D7b R2 finding
/// 4). A peer `Close`, an EOF (`None` — stream ended), or a transport error all mean the
/// browser/proxy hung up, so the socket must tear down PROMPTLY rather than linger until
/// the cookie `exp` (wasting a broadcast-receiver slot + a task per dead connection).
/// Any other inbound message (`Text`/`Binary`/`Ping`/`Pong`) is ignored — the dashboard
/// socket is server→client only and axum answers Pings at the protocol layer — so we
/// keep serving. Generic over the error type so it is unit-testable without an
/// `axum::Error` (which can't be constructed off a real socket).
fn inbound_is_terminal<E>(inbound: &Option<Result<Message, E>>) -> bool {
    matches!(inbound, None | Some(Err(_)) | Some(Ok(Message::Close(_))))
}

/// A sink for the dashboard wire messages. Abstracts the WS socket so the
/// send/expiry race ([`send_frames`] / [`send_snapshot`]) is unit-testable with a
/// mock sink — an `axum` `WebSocket` can't be constructed off a real upgrade in a
/// unit test.
trait FrameSink {
    /// Send one frame; `false` means the peer is gone (sending should stop).
    fn send_frame(&mut self, frame: &DashboardFrame) -> impl Future<Output = bool>;
    /// Send the initial `type:"snapshot"` message; `false` means the peer is gone.
    fn send_snapshot_message(&mut self, snap: &SnapshotMessage) -> impl Future<Output = bool>;
}

impl FrameSink for SplitSink<WebSocket, Message> {
    fn send_frame(&mut self, frame: &DashboardFrame) -> impl Future<Output = bool> {
        send_one(self, frame)
    }
    fn send_snapshot_message(&mut self, snap: &SnapshotMessage) -> impl Future<Output = bool> {
        send_snapshot_one(self, snap)
    }
}

/// Outcome of [`send_frames`]: the batch drained fully, the session expired
/// mid-send (caller must send the WS `Close`), or a send failed (peer gone).
#[derive(Debug, PartialEq, Eq)]
enum SendOutcome {
    Completed,
    Expired,
    Failed,
}

/// Send a batch of `frames` into `sink`, racing each send against the armed
/// `expiry` future so no frame is delivered past the cookie `exp` (even between
/// frames, under backpressure). The race is `biased` so a ready expiry wins
/// deterministically over a ready send — the connection must not outlive `exp`.
async fn send_frames(
    frames: &[DashboardFrame],
    mut expiry: std::pin::Pin<&mut (impl Future<Output = ()> + ?Sized)>,
    sink: &mut impl FrameSink,
) -> SendOutcome {
    for frame in frames {
        tokio::select! {
            biased;
            _ = expiry.as_mut() => return SendOutcome::Expired,
            sent = sink.send_frame(frame) => {
                if !sent {
                    return SendOutcome::Failed;
                }
            }
        }
    }
    SendOutcome::Completed
}

/// Send the initial snapshot message into `sink`, racing the armed `expiry` future
/// so it is never delivered past the cookie `exp`. The snapshot MUST precede every
/// `DashboardFrame` (finding 1); the same `biased` race as [`send_frames`] keeps an
/// already-/near-expired cookie from emitting it.
async fn send_snapshot(
    snapshot: &SnapshotMessage,
    mut expiry: std::pin::Pin<&mut (impl Future<Output = ()> + ?Sized)>,
    sink: &mut impl FrameSink,
) -> SendOutcome {
    tokio::select! {
        biased;
        _ = expiry.as_mut() => SendOutcome::Expired,
        sent = sink.send_snapshot_message(snapshot) => {
            if sent { SendOutcome::Completed } else { SendOutcome::Failed }
        }
    }
}

/// The connection PREAMBLE in its mandated order (D7b R1 finding 1): the initial
/// `type:"snapshot"` message FIRST, then the retained-transcript replay `frames`.
/// The SPA buffers every frame until the snapshot lands, so the snapshot strictly
/// precedes every frame here. Each step races `expiry`; a mid-preamble expiry/peer
/// loss short-circuits with `Expired`/`Failed` (the caller closes the socket). This
/// is its own unit so the snapshot-first ordering is testable with a recording sink
/// (an `axum` `WebSocket` can't be built off a real upgrade).
async fn send_initial(
    snapshot: &SnapshotMessage,
    frames: &[DashboardFrame],
    mut expiry: std::pin::Pin<&mut (impl Future<Output = ()> + ?Sized)>,
    sink: &mut impl FrameSink,
) -> SendOutcome {
    match send_snapshot(snapshot, expiry.as_mut(), sink).await {
        SendOutcome::Completed => {}
        other => return other,
    }
    send_frames(frames, expiry.as_mut(), sink).await
}

/// Serialize + send one frame as a WS text message. A serialization failure is
/// treated as a no-op success (skip the frame) rather than tearing down the
/// socket, mirroring `/debug/ws`. Writes to the split sink half (the read half is
/// raced separately for inbound-close detection — finding 4).
async fn send_one(sink: &mut SplitSink<WebSocket, Message>, frame: &DashboardFrame) -> bool {
    let Ok(payload) = serde_json::to_string(frame) else {
        return true;
    };
    sink.send(Message::Text(payload.into())).await.is_ok()
}

/// Serialize + send the initial snapshot message as a WS text message. Like
/// [`send_one`], a serialization failure is a no-op success rather than a teardown.
async fn send_snapshot_one(
    sink: &mut SplitSink<WebSocket, Message>,
    snapshot: &SnapshotMessage,
) -> bool {
    let Ok(payload) = serde_json::to_string(snapshot) else {
        return true;
    };
    sink.send(Message::Text(payload.into())).await.is_ok()
}

/// A far-future cap for the expiry timer (dev-open passes `u64::MAX`); keeps
/// `tokio::time::sleep` from overflowing on an absurd duration. A real cookie
/// `exp` (≤ 24 h) is always far below this.
const MAX_EXPIRY_WAIT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Sleep until the session `exp` (unix secs), then return. Derived from the wall
/// clock (`SystemTime`) but waited via `tokio::time::sleep`, so a paused-clock
/// test can drive it with `tokio::time::advance`.
async fn wait_for_session_expiry(session_exp: u64) {
    tokio::time::sleep(session_remaining(session_exp)).await;
}

/// Remaining time until `session_exp` (unix secs), saturating at zero and capped
/// at [`MAX_EXPIRY_WAIT`]. Uses the FULL sub-second wall clock (not a whole-second
/// truncation) so the socket closes within the `exp` second, matching `/debug/ws`.
fn session_remaining(session_exp: u64) -> Duration {
    let exp = Duration::from_secs(session_exp);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    exp.saturating_sub(now).min(MAX_EXPIRY_WAIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard_flow::FlowStatus;
    use crate::dashboard_flow::FlowUsage;
    use crate::dashboard_flow::PhaseTimings;
    use crate::dashboard_flow::capture_body;
    use crate::dashboard_flow::redact_headers;
    use crate::monitor::DebugRequest;
    use crate::monitor::DebugRequestStats;
    use crate::monitor::DebugRequestStatus;
    use crate::monitor::DebugSegment;
    use crate::monitor::DebugSegmentKind;
    use axum::http::HeaderMap;

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn test_flow_row(api_call_id: &str, status: FlowStatus) -> crate::dashboard_api::FlowRow {
        crate::dashboard_api::FlowRow {
            revision: 1,
            api_call_id: api_call_id.to_string(),
            response_id: None,
            method: "POST".to_string(),
            uri: "/v1/responses".to_string(),
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            status,
            started_ms: 1_000,
            finished_ms: None,
            elapsed_ms: None,
            terminal_reason: None,
            client_label: None,
            client_source: None,
            cost: None,
            cost_confidence: crate::dashboard_api::CostConfidence::Unavailable,
            phases: PhaseTimings::default(),
            attempts: Vec::new(),
            first_upstream_byte_ms: None,
        }
    }

    // -- the batched-envelope no-drop invariant (the key fix) --------------

    /// A `DebugUpdate` carrying MULTIPLE sibling `DebugWsMessage`s → exactly ONE
    /// `DashboardFrame{domain:Monitor, seq=DebugUpdate.sequence}` whose `batch`
    /// holds ALL the (non-flow) siblings. The whole-frame per-domain dedup then
    /// drops a stale WHOLE update, never an individual sibling.
    #[test]
    fn debug_update_with_siblings_becomes_one_monitor_frame_with_all_messages() {
        let store = DashboardFlowStore::disabled();
        let update = DebugUpdate {
            sequence: 6,
            messages: vec![
                DebugWsMessage::SegmentAppend {
                    response_id: "resp_001".to_string(),
                    segment: DebugSegment {
                        timestamp_ms: 1,
                        kind: DebugSegmentKind::Output,
                        text: "Hello".to_string(),
                    },
                },
                DebugWsMessage::SegmentAppend {
                    response_id: "resp_001".to_string(),
                    segment: DebugSegment {
                        timestamp_ms: 2,
                        kind: DebugSegmentKind::Output,
                        text: ", world".to_string(),
                    },
                },
                DebugWsMessage::SnapshotDone,
            ],
        };
        let frames = frames_for_update(&update, &store);
        // ONE monitor frame, no flow frame (no usage/status here).
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame.domain, Domain::Monitor);
        assert_eq!(frame.seq, 6, "monitor seq == DebugUpdate.sequence");
        assert_eq!(
            frame.batch.len(),
            3,
            "ALL three siblings ride one frame — none dropped by dedup"
        );
        for payload in &frame.batch {
            assert!(matches!(payload, DashboardPayload::Monitor { .. }));
        }
    }

    /// Without the FlowStore link, a monitor `Usage`/`RequestStatus` cannot be
    /// enriched into a flow payload, so NO flow frame is emitted — but both messages
    /// still ride the monitor batch (the monitor batch ALWAYS carries every original
    /// sibling — finding 2), so no transcript data is lost.
    #[test]
    fn unresolved_usage_status_stay_in_monitor_batch_no_flow_frame() {
        let store = DashboardFlowStore::disabled();
        let update = DebugUpdate {
            sequence: 9,
            messages: vec![
                DebugWsMessage::Usage {
                    response_id: "resp_x".to_string(),
                    prompt: 1,
                    completion: 2,
                    total: 3,
                    cached: 0,
                    reasoning: 0,
                },
                DebugWsMessage::RequestStatus {
                    response_id: "resp_x".to_string(),
                    status: DebugRequestStatus::Completed,
                    completed_at_ms: Some(10),
                    error: None,
                },
            ],
        };
        let frames = frames_for_update(&update, &store);
        assert_eq!(frames.len(), 1, "no flow frame without a resolvable record");
        assert_eq!(frames[0].domain, Domain::Monitor);
        assert_eq!(
            frames[0].batch.len(),
            2,
            "both stay in monitor, none dropped"
        );
    }

    #[test]
    fn linked_monitor_usage_and_status_remain_transcript_only() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            None,
            crate::dashboard_flow::ClientAttribution::none(),
        );
        store.link("resp_1".to_string(), "api_1".to_string());
        let update = DebugUpdate {
            sequence: 44,
            messages: vec![
                DebugWsMessage::Usage {
                    response_id: "resp_1".to_string(),
                    prompt: 2,
                    completion: 3,
                    total: 5,
                    cached: 0,
                    reasoning: 0,
                },
                DebugWsMessage::RequestStatus {
                    response_id: "resp_1".to_string(),
                    status: DebugRequestStatus::Completed,
                    completed_at_ms: Some(9),
                    error: None,
                },
            ],
        };

        let frames = frames_for_update(&update, &store);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].domain, Domain::Monitor);
        assert_eq!(frames[0].seq, 44);
        assert_eq!(frames[0].batch.len(), 2);
        assert!(
            frames[0]
                .batch
                .iter()
                .all(|payload| matches!(payload, DashboardPayload::Monitor { .. }))
        );
    }

    /// The serialized Monitor frame matches `GOLDEN_MONITOR_FRAME_JSON` exactly:
    /// `domain:"monitor"`, `seq:6`, a 4-element batch of `monitor` payloads each
    /// NESTING an itself-tagged `DebugWsMessage` under `message`.
    #[test]
    fn monitor_frame_matches_golden_fixture_bytes() {
        let frame = DashboardFrame {
            domain: Domain::Monitor,
            seq: 6,
            batch: vec![
                DashboardPayload::Monitor {
                    message: DebugWsMessage::RequestUpsert {
                        request: DebugRequest {
                            response_id: "resp_001".to_string(),
                            model: "llama-3.1-70b".to_string(),
                            started_at_ms: 1718900000000,
                            updated_at_ms: 1718900000000,
                            completed_at_ms: None,
                            status: DebugRequestStatus::Running,
                            stats: DebugRequestStats {
                                input_items: 3,
                                tool_count: 0,
                                turn_count: 1,
                                user_messages: 1,
                                assistant_messages: 0,
                                system_messages: 1,
                                developer_messages: 0,
                                reasoning_items: 0,
                                function_calls: 0,
                                function_outputs: 0,
                                tool_items: 0,
                                input_chars: 42,
                                instructions_chars: 0,
                            },
                            error: None,
                            usage: None,
                        },
                    },
                },
                DashboardPayload::Monitor {
                    message: DebugWsMessage::SegmentAppend {
                        response_id: "resp_001".to_string(),
                        segment: DebugSegment {
                            timestamp_ms: 1718900000001,
                            kind: DebugSegmentKind::Output,
                            text: "Hello".to_string(),
                        },
                    },
                },
                DashboardPayload::Monitor {
                    message: DebugWsMessage::SegmentAppend {
                        response_id: "resp_001".to_string(),
                        segment: DebugSegment {
                            timestamp_ms: 1718900000002,
                            kind: DebugSegmentKind::Output,
                            text: ", world".to_string(),
                        },
                    },
                },
                DashboardPayload::Monitor {
                    message: DebugWsMessage::RequestStatus {
                        response_id: "resp_001".to_string(),
                        status: DebugRequestStatus::Completed,
                        completed_at_ms: Some(1718900000003),
                        error: None,
                    },
                },
            ],
        };
        // Compare as serde_json::Value so the assertion is key-order independent
        // but byte-equivalent on shape + values.
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "monitor",
            "seq": 6,
            "batch": [
                {
                    "type": "monitor",
                    "message": {
                        "type": "request_upsert",
                        "request": {
                            "response_id": "resp_001",
                            "model": "llama-3.1-70b",
                            "started_at_ms": 1718900000000u64,
                            "updated_at_ms": 1718900000000u64,
                            "completed_at_ms": null,
                            "status": "running",
                            "stats": {
                                "input_items": 3, "tool_count": 0, "turn_count": 1, "user_messages": 1,
                                "assistant_messages": 0, "system_messages": 1, "developer_messages": 0,
                                "reasoning_items": 0, "function_calls": 0, "function_outputs": 0, "tool_items": 0,
                                "input_chars": 42, "instructions_chars": 0
                            },
                            "error": null
                        }
                    }
                },
                { "type": "monitor", "message": { "type": "segment_append", "response_id": "resp_001", "segment": { "timestamp_ms": 1718900000001u64, "kind": "output", "text": "Hello" } } },
                { "type": "monitor", "message": { "type": "segment_append", "response_id": "resp_001", "segment": { "timestamp_ms": 1718900000002u64, "kind": "output", "text": ", world" } } },
                { "type": "monitor", "message": { "type": "request_status", "response_id": "resp_001", "status": "completed", "completed_at_ms": 1718900000003u64, "error": null } }
            ]
        });
        assert_eq!(got, want, "monitor frame must match the D9 golden bytes");
    }

    /// The `usage` payload matches `GOLDEN_USAGE_FRAME_JSON`: `type:"usage"`,
    /// `api_call_id` + `response_id` + the five token fields, under domain `flow`. With
    /// BOTH cached/reasoning REPORTED (`Some`), the bytes are byte-identical to the frozen
    /// frontend golden fixture (`ws.fixtures.ts`) — the `Option` migration (gap 07 review
    /// round 1, finding 1) is wire-compatible when the classes are present.
    #[test]
    fn usage_frame_matches_golden_fixture_bytes() {
        let frame = DashboardFrame {
            domain: Domain::Flow,
            seq: 4,
            batch: vec![DashboardPayload::Usage {
                api_call_id: "api_001".to_string(),
                response_id: Some("resp_001".to_string()),
                prompt: 812,
                completion: 240,
                total: 1052,
                cached: Some(128),
                reasoning: Some(0),
            }],
        };
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "flow",
            "seq": 4,
            "batch": [
                { "type": "usage", "api_call_id": "api_001", "response_id": "resp_001", "prompt": 812, "completion": 240, "total": 1052, "cached": 128, "reasoning": 0 }
            ]
        });
        assert_eq!(got, want);
    }

    /// Gap 07 review round 1, finding 1 — an UNREPORTED cached/reasoning class is OMITTED
    /// on the dashboard `usage` wire (don't-lie-with-zeros), DISTINCT from a present `0`.
    /// `prompt`/`completion`/`total` always serialize. This is the absence-vs-`0`
    /// guarantee the frontend `UsagePayload` (optional `cached`/`reasoning`) relies on.
    #[test]
    fn usage_frame_omits_unreported_cached_reasoning() {
        let frame = DashboardFrame {
            domain: Domain::Flow,
            seq: 7,
            batch: vec![DashboardPayload::Usage {
                api_call_id: "api_002".to_string(),
                response_id: None,
                prompt: 100,
                completion: 50,
                total: 150,
                cached: None,    // unreported ⇒ absent on the wire
                reasoning: None, // unreported ⇒ absent on the wire
            }],
        };
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "flow",
            "seq": 7,
            "batch": [
                { "type": "usage", "api_call_id": "api_002", "prompt": 100, "completion": 50, "total": 150 }
            ]
        });
        assert_eq!(
            got, want,
            "unreported cached/reasoning (and a None response_id) are all absent — never a fabricated 0"
        );

        // A REPORTED zero is DISTINCT — it serializes as a present `0`.
        let reported_zero = DashboardPayload::Usage {
            api_call_id: "api_003".to_string(),
            response_id: None,
            prompt: 1,
            completion: 1,
            total: 2,
            cached: Some(0),
            reasoning: None,
        };
        let json = serde_json::to_value(&reported_zero).expect("serialize");
        assert_eq!(
            json["cached"],
            serde_json::json!(0),
            "a reported 0 is present"
        );
        assert!(
            json.get("reasoning").is_none(),
            "an unreported class is still absent alongside a reported-0 sibling"
        );
    }

    /// The `flow_status` payload matches `GOLDEN_FLOW_STATUS_FRAME_JSON`:
    /// `type:"flow_status"`, `api_call_id`, `response_id`, `status`,
    /// `model_requested`/`model_served`/`upstream_target`, a nested `usage`,
    /// `started_ms`, `elapsed_ms`.
    #[test]
    fn flow_status_frame_matches_golden_fixture_bytes() {
        let mut row = test_flow_row("api_001", FlowStatus::Completed);
        row.revision = 9;
        row.response_id = Some("resp_001".to_string());
        row.model_requested = Some("gpt-4o".to_string());
        row.model_served = Some("llama-3.1-70b".to_string());
        row.upstream_target = Some("vllm-a".to_string());
        row.usage = Some(FlowUsage {
            prompt: 812,
            completion: 512,
            total: 1324,
            cached: Some(128),
            reasoning: Some(0),
        });
        row.started_ms = 1718900000000;
        row.elapsed_ms = Some(3100);
        let frame = DashboardFrame {
            domain: Domain::Flow,
            seq: 5,
            batch: vec![DashboardPayload::FlowStatus {
                phase: FlowMutationPhase::Terminal,
                row: Box::new(row),
            }],
        };
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "flow",
            "seq": 5,
            "batch": [
                {
                    "type": "flow_status",
                    "phase": "terminal",
                    "revision": 9,
                    "api_call_id": "api_001",
                    "response_id": "resp_001",
                    "status": "completed",
                    "model_requested": "gpt-4o",
                    "model_served": "llama-3.1-70b",
                    "upstream_target": "vllm-a",
                    "usage": { "prompt": 812, "completion": 512, "total": 1324, "cached": 128, "reasoning": 0 },
                    "started_ms": 1718900000000u64,
                    "elapsed_ms": 3100,
                    "method": "POST",
                    "uri": "/v1/responses",
                    "cost": null,
                    "cost_confidence": "unavailable"
                }
            ]
        });
        assert_eq!(got, want);
    }

    /// Gap 10b — the live `flow_status` payload PROJECTS the spine fields that are
    /// meaningful PROGRESSIVELY for a LIVE flow: the gap-02 `phases` (flattened as sibling
    /// scalars on the payload), the gap-03 `attempts`, and the gap-03 `first_upstream_byte_ms`.
    /// A flow with measured phases/attempts EMITS them (so a live row lights up its waterfall
    /// and stepper incrementally); a flow without them OMITS every spine key (absent, never a
    /// fabricated `0`). The flattened `PhaseTimings` and each `Attempt` deserialize back into
    /// their DTOs to prove the round-trip (AGENTS.md: no new wire field without one).
    #[test]
    fn flow_status_payload_projects_spine_fields_present_and_absent() {
        use crate::dashboard_flow::Attempt;
        use crate::dashboard_flow::AttemptStatus;
        use crate::dashboard_flow::PhaseTimings;

        let phases = PhaseTimings {
            ingress_ms: Some(1_000),
            normalization_done_ms: Some(1_030),
            routing_decision_ms: Some(1_050),
            first_content_delta_ms: Some(1_500),
            stream_end_ms: None,
            finalize_ms: None,
        };
        let attempt = Attempt {
            provider: Some("vllm-a".to_string()),
            model: Some("llama-3.1-70b".to_string()),
            start_ms: 1_050,
            end_ms: 1_220,
            first_upstream_byte_ms: Some(1_220),
            status: AttemptStatus::Served,
            error_class: None,
            failover_reason: None,
        };

        // PRESENT: a live flow that has reached first content + recorded its serving attempt.
        let mut present_row = test_flow_row("api_001", FlowStatus::Open);
        present_row.response_id = Some("resp_001".to_string());
        present_row.model_served = Some("llama-3.1-70b".to_string());
        present_row.upstream_target = Some("vllm-a".to_string());
        present_row.elapsed_ms = Some(500);
        present_row.phases = phases;
        present_row.attempts = vec![attempt.clone()];
        present_row.first_upstream_byte_ms = Some(1_220);
        let present = DashboardPayload::FlowStatus {
            phase: FlowMutationPhase::Progress,
            row: Box::new(present_row),
        };
        let value = serde_json::to_value(&present).expect("serialize present payload");
        // Phases flattened as sibling scalars next to `type` (NOT nested).
        assert_eq!(value["ingress_ms"], serde_json::json!(1_000));
        assert_eq!(value["first_content_delta_ms"], serde_json::json!(1_500));
        assert_eq!(value["first_upstream_byte_ms"], serde_json::json!(1_220));
        // The not-yet-reached phases (stream_end/finalize) are ABSENT, never `0`.
        let obj = value.as_object().expect("object");
        assert!(
            !obj.contains_key("stream_end_ms"),
            "unreached phase absent: {value}"
        );
        assert!(
            !obj.contains_key("finalize_ms"),
            "unreached phase absent: {value}"
        );
        // Round-trip the attempt + flattened phases back into their DTOs.
        let attempts: Vec<Attempt> =
            serde_json::from_value(value["attempts"].clone()).expect("deserialize attempts");
        assert_eq!(attempts, vec![attempt]);
        let rt: PhaseTimings =
            serde_json::from_value(value.clone()).expect("deserialize flattened phases");
        assert_eq!(rt, phases);

        // ABSENT: a freshly-opened flow with no spine measured yet omits every spine key.
        let absent = DashboardPayload::FlowStatus {
            phase: FlowMutationPhase::Open,
            row: Box::new(test_flow_row("api_002", FlowStatus::Open)),
        };
        let value = serde_json::to_value(&absent).expect("serialize absent payload");
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
                "absent spine key {key} omitted on live flow_status (not 0/null): {value}"
            );
        }
    }

    /// The `metric_tick` payload matches the flat `GOLDEN_METRIC_TICK_FRAME_JSON`
    /// SHAPE: the headline tile fields + a `windows{m1,m5,h1}` map, each a full
    /// `MetricWindow`. (Exact numeric roll-ups are D13; this asserts the keys +
    /// types are byte-shape-exact.)
    #[test]
    fn metric_tick_frame_matches_golden_fixture_shape() {
        let frame = DashboardFrame {
            domain: Domain::Metrics,
            seq: 2,
            batch: vec![DashboardPayload::MetricTick(MetricTick {
                reqs_per_sec: 4.2,
                active_streams: 3,
                error_pct: 1.1,
                p50: 180.0,
                p95: 920.0,
                p99: 1840.0,
                tokens_per_sec: 142.0,
                cost_per_min: 0.21,
                samples: 252,
                usage_samples: 250,
                priced_samples: 240,
                cost_confidence: crate::dashboard_api::CostConfidence::Estimated,
                windows: MetricWindows {
                    m1: MetricWindow {
                        reqs_per_sec: 4.2,
                        active_streams: 3,
                        error_pct: 1.1,
                        p50: 180.0,
                        p95: 920.0,
                        p99: 1840.0,
                        tokens_per_sec: 142.0,
                        cost_per_min: 0.21,
                        samples: 252,
                        usage_samples: 250,
                        priced_samples: 240,
                        cost_confidence: crate::dashboard_api::CostConfidence::Estimated,
                    },
                    m5: MetricWindow {
                        reqs_per_sec: 3.8,
                        active_streams: 3,
                        error_pct: 1.0,
                        p50: 175.0,
                        p95: 900.0,
                        p99: 1800.0,
                        tokens_per_sec: 128.0,
                        cost_per_min: 0.19,
                        samples: 1140,
                        usage_samples: 1130,
                        priced_samples: 1100,
                        cost_confidence: crate::dashboard_api::CostConfidence::Estimated,
                    },
                    h1: MetricWindow {
                        reqs_per_sec: 2.9,
                        active_streams: 2,
                        error_pct: 0.8,
                        p50: 160.0,
                        p95: 850.0,
                        p99: 1700.0,
                        tokens_per_sec: 100.0,
                        cost_per_min: 0.15,
                        samples: 10440,
                        usage_samples: 10400,
                        priced_samples: 10000,
                        cost_confidence: crate::dashboard_api::CostConfidence::Estimated,
                    },
                },
            })],
        };
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "metrics",
            "seq": 2,
            "batch": [
                {
                    "type": "metric_tick",
                    "reqs_per_sec": 4.2, "active_streams": 3, "error_pct": 1.1,
                    "p50": 180.0, "p95": 920.0, "p99": 1840.0, "tokens_per_sec": 142.0, "cost_per_min": 0.21,
                    "samples": 252, "usage_samples": 250, "priced_samples": 240, "cost_confidence": "estimated",
                    "windows": {
                        "m1": { "reqs_per_sec": 4.2, "active_streams": 3, "error_pct": 1.1, "p50": 180.0, "p95": 920.0, "p99": 1840.0, "tokens_per_sec": 142.0, "cost_per_min": 0.21, "samples": 252, "usage_samples": 250, "priced_samples": 240, "cost_confidence": "estimated" },
                        "m5": { "reqs_per_sec": 3.8, "active_streams": 3, "error_pct": 1.0, "p50": 175.0, "p95": 900.0, "p99": 1800.0, "tokens_per_sec": 128.0, "cost_per_min": 0.19, "samples": 1140, "usage_samples": 1130, "priced_samples": 1100, "cost_confidence": "estimated" },
                        "h1": { "reqs_per_sec": 2.9, "active_streams": 2, "error_pct": 0.8, "p50": 160.0, "p95": 850.0, "p99": 1700.0, "tokens_per_sec": 100.0, "cost_per_min": 0.15, "samples": 10440, "usage_samples": 10400, "priced_samples": 10000, "cost_confidence": "estimated" }
                    }
                }
            ]
        });
        assert_eq!(got, want);
    }

    /// Gap 01 finding 1: the metrics-domain cursor stays STRICTLY MONOTONIC across both
    /// a genuine ring advance AND an active-stream-only change (which does not bump the
    /// ring `metrics_seq`). An active-only change must still produce a `seq` greater than
    /// the last emitted one so the client does not drop it as a same-seq duplicate; a
    /// later genuine ring advance must still win when it is higher.
    #[test]
    fn next_metrics_cursor_is_strictly_monotonic_for_active_only_changes() {
        // Baseline: snapshot seeded the cursor at the ring seq (say 5).
        let mut emitted = 5u64;
        // Active-stream-only change: ring seq unchanged (5), cursor must advance to 6.
        let next = next_metrics_cursor(5, emitted);
        assert_eq!(
            next, 6,
            "active-only change advances the cursor past the last"
        );
        assert!(next > emitted);
        emitted = next;
        // Another active-only change: 5 -> 7 (still strictly increasing).
        let next = next_metrics_cursor(5, emitted);
        assert_eq!(next, 7);
        emitted = next;
        // A genuine ring advance to seq 9 (a terminal finalized): the true seq wins
        // because it exceeds last_emitted + 1.
        let next = next_metrics_cursor(9, emitted);
        assert_eq!(next, 9, "a higher ring seq is carried verbatim");
        assert!(next > emitted);
        emitted = next;
        // A ring seq that did NOT advance past the nudged cursor still increments by one
        // (never goes backwards, never repeats).
        let next = next_metrics_cursor(9, emitted);
        assert_eq!(next, 10);
    }

    /// The `topology_update` payload matches `GOLDEN_TOPOLOGY_FRAME_JSON`:
    /// `type:"topology_update"`, a `nodes` array (D4 `ProviderHealth` shape with a
    /// NON-NULL `catalog_size`), and a gateway→provider `edges` array. Built via
    /// [`topology_frame`] off a real `ProviderHealthSnapshot`.
    #[test]
    fn topology_frame_matches_golden_fixture_shape() {
        use crate::upstream::ProviderHealth;
        use crate::upstream::ProviderStatus;
        let snapshot = ProviderHealthSnapshot {
            version: 2,
            providers: vec![ProviderHealth {
                id: "vllm-a".to_string(),
                name: "vllm-a (8001)".to_string(),
                route: None,
                base_url: "http://localhost:8001".to_string(),
                status: ProviderStatus::Healthy,
                cooling_until_ms: None,
                last_error: None,
                served_count: 1280,
                failover_count: 0,
                consecutive_failures: 0,
                catalog_fetched_ms: Some(1718899995000),
                catalog_size: Some(12),
            }],
        };
        let frame = topology_frame(&snapshot);
        assert_eq!(frame.domain, Domain::Topology);
        assert_eq!(frame.seq, 2, "topology seq == snapshot version");
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "topology",
            "seq": 2,
            "batch": [
                {
                    "type": "topology_update",
                    "nodes": [
                        {
                            "id": "vllm-a", "name": "vllm-a (8001)", "route": null, "base_url": "http://localhost:8001",
                            "status": "healthy", "cooling_until_ms": null, "last_error": null,
                            "served_count": 1280, "failover_count": 0, "consecutive_failures": 0,
                            "catalog_fetched_ms": 1718899995000u64, "catalog_size": 12
                        }
                    ],
                    "edges": [
                        { "from": "gateway", "to": "vllm-a", "throughput": 0.0, "tokens_per_sec": 0.0, "cost_per_sec": 0.0 }
                    ]
                }
            ]
        });
        assert_eq!(
            got, want,
            "topology frame must match the D9 golden node shape"
        );
    }

    /// A topology node with an UNFETCHED catalog (`catalog_size: None`) serializes
    /// `catalog_size: 0` (non-null), per the frozen contract's required-uint key —
    /// the one field that does NOT follow the `Option → null` rule.
    #[test]
    fn topology_node_catalog_size_none_serializes_as_zero_not_null() {
        use crate::upstream::ProviderHealth;
        use crate::upstream::ProviderStatus;
        let snapshot = ProviderHealthSnapshot {
            version: 1,
            providers: vec![ProviderHealth {
                id: "p".to_string(),
                name: "p".to_string(),
                route: None,
                base_url: "http://x".to_string(),
                status: ProviderStatus::Healthy,
                cooling_until_ms: None,
                last_error: None,
                served_count: 0,
                failover_count: 0,
                consecutive_failures: 0,
                catalog_fetched_ms: None,
                catalog_size: None,
            }],
        };
        let value = serde_json::to_value(topology_frame(&snapshot)).expect("serialize");
        let node = &value["batch"][0]["nodes"][0];
        assert_eq!(node["catalog_size"], serde_json::json!(0));
        assert!(
            !node["catalog_size"].is_null(),
            "catalog_size is a required non-null uint"
        );
    }

    // -- the initial snapshot message (finding 1) --------------------------

    /// The serialized `SnapshotMessage` matches the frozen `SnapshotFrame` contract
    /// the SPA's `isSnapshotFrame` guard requires (`dashboard-frontend/src/api/
    /// types.ts`): `type:"snapshot"`, a `cursors` quad, a `flows` array of body-free
    /// summaries, and `metrics`/`topology` either their full shape or `null`. A
    /// mismatch here means the SPA drops the snapshot and never renders.
    #[test]
    fn snapshot_message_matches_frontend_snapshot_frame_shape() {
        use crate::upstream::ProviderHealth;
        use crate::upstream::ProviderStatus;
        // A live store with one finalized flow → one body-free summary.
        let store = DashboardFlowStore::new();
        store.open(
            "api_001".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            Some(capture_body(b"{}")),
            crate::dashboard_flow::ClientAttribution::none(),
        );
        store.finalize("api_001", FlowStatus::Completed, None, None);
        // Project summaries → wire-facing FlowRows the way `dashboard_socket` does (no
        // Gateway in this test, so build the row literally — the SHAPE is what's asserted:
        // the SPA's `isSnapshotFrame` requires the gap-07 `cost_confidence` on every row).
        let flows: Vec<crate::dashboard_api::FlowRow> = store
            .snapshot_summaries()
            .iter()
            .map(|s| crate::dashboard_api::FlowRow {
                revision: s.revision,
                api_call_id: s.api_call_id.clone(),
                response_id: s.response_id.clone(),
                method: s.method.clone(),
                uri: s.uri.clone(),
                model_requested: s.model_requested.clone(),
                model_served: s.model_served.clone(),
                upstream_target: s.upstream_target.clone(),
                usage: s.usage,
                status: s.status,
                started_ms: s.started_ms,
                finished_ms: s.finished_ms,
                elapsed_ms: s.elapsed_ms,
                terminal_reason: s.terminal_reason.clone(),
                client_label: s.client_label.clone(),
                client_source: s.client_source,
                cost: None,
                cost_confidence: crate::dashboard_api::CostConfidence::Unavailable,
                phases: s.phases,
                attempts: s.attempts.clone(),
                first_upstream_byte_ms: s.first_upstream_byte_ms,
            })
            .collect();
        let flow_seq = store.flow_seq();

        let metrics = Some(metrics_snapshot(
            &MetricsView::default(),
            7,
            0,
            &std::collections::HashMap::new(),
        ));
        let snapshot = ProviderHealthSnapshot {
            version: 3,
            providers: vec![ProviderHealth {
                id: "vllm-a".to_string(),
                name: "vllm-a".to_string(),
                route: None,
                base_url: "http://localhost:8001".to_string(),
                status: ProviderStatus::Healthy,
                cooling_until_ms: None,
                last_error: None,
                served_count: 0,
                failover_count: 0,
                consecutive_failures: 0,
                catalog_fetched_ms: None,
                catalog_size: None,
            }],
        };
        let topology = Some(topology_snapshot(&snapshot));
        let msg = snapshot_message(flows, flow_seq, metrics, topology, 0);
        let value = serde_json::to_value(&msg).expect("serialize");

        // Discriminant the SPA routes on.
        assert_eq!(value["type"], serde_json::json!("snapshot"));
        // The four per-domain cursors (all present, the SPA installs them as dedup
        // baselines). metrics_seq/topology_seq mirror the carried bodies; monitor 0.
        let cursors = &value["cursors"];
        assert_eq!(cursors["flow_seq"], serde_json::json!(flow_seq));
        assert_eq!(cursors["metrics_seq"], serde_json::json!(7));
        assert_eq!(cursors["topology_seq"], serde_json::json!(3));
        assert_eq!(cursors["monitor_seq"], serde_json::json!(0));
        // flows: an array of body-free wire ROWS keyed by api_call_id (no body keys).
        let flows = value["flows"].as_array().expect("flows is an array");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0]["api_call_id"], serde_json::json!("api_001"));
        assert_eq!(flows[0]["status"], serde_json::json!("completed"));
        assert!(
            flows[0].get("inbound_body").is_none(),
            "summaries are body-free"
        );
        // Gap 07: the SPA's `isSnapshotFrame` REQUIRES `cost_confidence` on every snapshot
        // row (same guard as `/flows`); a raw `SnapshotFlowSummary` (no cost fields) fails
        // validation and bricks the client at `connecting`. `cost` may be null but the
        // confidence tag must be PRESENT.
        assert_eq!(
            flows[0]["cost_confidence"],
            serde_json::json!("unavailable"),
            "snapshot rows must carry the gap-07 cost_confidence tag"
        );
        // metrics: the flat tile + metrics_seq + windows{m1,m5,h1}.
        let m = &value["metrics"];
        assert_eq!(m["metrics_seq"], serde_json::json!(7));
        assert!(m["windows"]["m1"].is_object());
        assert!(m["windows"]["m5"].is_object());
        assert!(m["windows"]["h1"].is_object());
        // topology: topology_seq + nodes + edges + a (possibly empty) price_table map.
        let t = &value["topology"];
        assert_eq!(t["topology_seq"], serde_json::json!(3));
        assert!(t["nodes"].is_array());
        assert!(t["edges"].is_array());
        assert!(
            t["price_table"].is_object(),
            "price_table is an object map (empty until D13)"
        );
        // catalog_size on a snapshot node follows the same non-null-uint rule.
        assert_eq!(t["nodes"][0]["catalog_size"], serde_json::json!(0));
    }

    /// When metrics/topology are absent (disabled / no providers), the snapshot
    /// carries JSON `null` for them and zeroes their cursors — the SPA's
    /// `isSnapshotFrame` accepts `metrics`/`topology` of `null`.
    #[test]
    fn snapshot_message_serializes_null_metrics_topology() {
        let msg = snapshot_message(Vec::new(), 0, None, None, 0);
        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(value["type"], serde_json::json!("snapshot"));
        assert!(value["metrics"].is_null(), "absent metrics → null");
        assert!(value["topology"].is_null(), "absent topology → null");
        assert_eq!(value["cursors"]["metrics_seq"], serde_json::json!(0));
        assert_eq!(value["cursors"]["topology_seq"], serde_json::json!(0));
        assert!(value["flows"].as_array().unwrap().is_empty());
    }

    // -- expiry timer ------------------------------------------------------

    #[test]
    fn expired_session_has_zero_remaining() {
        assert_eq!(
            session_remaining(now_unix().saturating_sub(60)),
            Duration::ZERO
        );
        assert_eq!(session_remaining(0), Duration::ZERO);
    }

    #[test]
    fn future_session_has_positive_remaining() {
        let remaining = session_remaining(now_unix() + 120);
        assert!(
            remaining > Duration::from_secs(60),
            "remaining: {remaining:?}"
        );
        assert!(remaining <= Duration::from_secs(120));
    }

    /// The per-connection expiry timer fires once the cookie `exp` passes (the
    /// future the socket `select!`s on to send a `Close`).
    #[tokio::test(start_paused = true)]
    async fn expiry_wait_completes_after_exp_passes() {
        let exp = now_unix() + 2;
        let waiter = tokio::spawn(wait_for_session_expiry(exp));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!waiter.is_finished(), "must not close before exp");
        tokio::time::advance(Duration::from_secs(2)).await;
        waiter.await.expect("expiry wait completes");
    }

    // -- send/expiry race --------------------------------------------------

    fn monitor_frames(n: usize) -> Vec<DashboardFrame> {
        (0..n)
            .map(|i| DashboardFrame {
                domain: Domain::Monitor,
                seq: i as u64,
                batch: vec![DashboardPayload::Monitor {
                    message: DebugWsMessage::SnapshotDone,
                }],
            })
            .collect()
    }

    /// A mock [`FrameSink`]: counts sends, optionally sleeps per send to model
    /// backpressure, and optionally "fails" (peer gone) at a given send index.
    struct MockSink {
        sent: usize,
        per_send: Duration,
        fail_at: Option<usize>,
    }

    impl FrameSink for MockSink {
        async fn send_frame(&mut self, _frame: &DashboardFrame) -> bool {
            if self.per_send > Duration::ZERO {
                tokio::time::sleep(self.per_send).await;
            }
            self.sent += 1;
            !matches!(self.fail_at, Some(at) if self.sent == at)
        }
        async fn send_snapshot_message(&mut self, _snap: &SnapshotMessage) -> bool {
            if self.per_send > Duration::ZERO {
                tokio::time::sleep(self.per_send).await;
            }
            self.sent += 1;
            !matches!(self.fail_at, Some(at) if self.sent == at)
        }
    }

    /// A recording sink that logs the ORDER + kind of each wire message so a test
    /// can assert the snapshot precedes every frame (finding 1). Each `send_*`
    /// returns success; ordering, not backpressure, is the unit under test here.
    #[derive(Default)]
    struct RecordingSink {
        log: Vec<WireKind>,
    }
    #[derive(Debug, PartialEq, Eq)]
    enum WireKind {
        Snapshot,
        Frame(Domain),
    }
    impl FrameSink for RecordingSink {
        async fn send_frame(&mut self, frame: &DashboardFrame) -> bool {
            self.log.push(WireKind::Frame(frame.domain));
            true
        }
        async fn send_snapshot_message(&mut self, _snap: &SnapshotMessage) -> bool {
            self.log.push(WireKind::Snapshot);
            true
        }
    }

    fn sample_snapshot() -> SnapshotMessage {
        snapshot_message(Vec::new(), 0, None, None, 0)
    }

    /// D7b R1 finding 1: the connection preamble sends the `type:"snapshot"` message
    /// as the VERY FIRST wire message, BEFORE any `DashboardFrame`. The SPA buffers
    /// frames until the snapshot lands, so this ordering is what makes the dashboard
    /// render at all.
    #[tokio::test(start_paused = true)]
    async fn send_initial_emits_snapshot_before_any_frame() {
        let expiry = wait_for_session_expiry(now_unix() + 3600);
        tokio::pin!(expiry);
        let mut sink = RecordingSink::default();
        let frames = monitor_frames(3);
        let outcome = send_initial(&sample_snapshot(), &frames, expiry.as_mut(), &mut sink).await;
        assert_eq!(outcome, SendOutcome::Completed);
        // FIRST message is the snapshot; the replay frames follow.
        assert_eq!(
            sink.log.first(),
            Some(&WireKind::Snapshot),
            "the snapshot must be the FIRST wire message"
        );
        assert_eq!(sink.log.len(), 4, "snapshot + 3 frames");
        for entry in &sink.log[1..] {
            assert!(
                matches!(entry, WireKind::Frame(_)),
                "everything after the snapshot is a frame"
            );
        }
    }

    /// The snapshot send is itself gated by the expiry race: an already-expired
    /// cookie emits NOTHING (no snapshot, no frame) and yields `Expired`.
    #[tokio::test(start_paused = true)]
    async fn send_initial_with_expired_cookie_emits_nothing() {
        let expiry = wait_for_session_expiry(now_unix().saturating_sub(10));
        tokio::pin!(expiry);
        let mut sink = RecordingSink::default();
        let outcome = send_initial(
            &sample_snapshot(),
            &monitor_frames(3),
            expiry.as_mut(),
            &mut sink,
        )
        .await;
        assert_eq!(outcome, SendOutcome::Expired);
        assert!(sink.log.is_empty(), "no wire message after exp");
    }

    #[tokio::test(start_paused = true)]
    async fn send_frames_completes_when_not_expired() {
        let expiry = wait_for_session_expiry(now_unix() + 3600);
        tokio::pin!(expiry);
        let mut sink = MockSink {
            sent: 0,
            per_send: Duration::ZERO,
            fail_at: None,
        };
        let outcome = send_frames(&monitor_frames(5), expiry.as_mut(), &mut sink).await;
        assert_eq!(outcome, SendOutcome::Completed);
        assert_eq!(sink.sent, 5);
    }

    /// An already-expired cookie sends NOTHING and yields `Expired` (the caller
    /// closes the socket) — the timer is armed before the first send.
    #[tokio::test(start_paused = true)]
    async fn send_frames_with_expired_cookie_sends_nothing() {
        let expiry = wait_for_session_expiry(now_unix().saturating_sub(10));
        tokio::pin!(expiry);
        let mut sink = MockSink {
            sent: 0,
            per_send: Duration::ZERO,
            fail_at: None,
        };
        let outcome = send_frames(&monitor_frames(5), expiry.as_mut(), &mut sink).await;
        assert_eq!(outcome, SendOutcome::Expired);
        assert_eq!(sink.sent, 0, "no frame may be sent after exp");
    }

    /// A cookie expiring PART-WAY through a backpressured batch stops mid-stream
    /// with `Expired` rather than delivering frames past `exp`.
    #[tokio::test(start_paused = true)]
    async fn send_frames_expiring_mid_batch_stops_early() {
        let expiry = wait_for_session_expiry(now_unix() + 5);
        tokio::pin!(expiry);
        let mut sink = MockSink {
            sent: 0,
            per_send: Duration::from_secs(2),
            fail_at: None,
        };
        let outcome = send_frames(&monitor_frames(100), expiry.as_mut(), &mut sink).await;
        assert_eq!(outcome, SendOutcome::Expired);
        assert!(
            (1..100).contains(&sink.sent),
            "stopped mid-batch at exp (sent {})",
            sink.sent
        );
    }

    /// A peer that drops mid-batch surfaces `Failed` (caller returns).
    #[tokio::test(start_paused = true)]
    async fn send_frames_send_failure_short_circuits() {
        let expiry = wait_for_session_expiry(now_unix() + 3600);
        tokio::pin!(expiry);
        let mut sink = MockSink {
            sent: 0,
            per_send: Duration::ZERO,
            fail_at: Some(2),
        };
        let outcome = send_frames(&monitor_frames(5), expiry.as_mut(), &mut sink).await;
        assert_eq!(outcome, SendOutcome::Failed);
        assert_eq!(sink.sent, 2);
    }

    // -- (finding 3) the 4401 auth/expiry close frame -----------------------

    /// D7b R2 finding 3: EVERY expiry path closes with the EXPLICIT `4401` code the SPA
    /// recognizes as a session expiry (`ws.ts` `WS_AUTH_CLOSE`), NEVER an unclassified
    /// `Close(None)`. An unclassified close is read by the SPA as an abnormal blip and
    /// reconnected; only `4401` bounces it to login.
    #[test]
    fn auth_close_frame_carries_4401_code() {
        match auth_close_frame() {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, 4401, "the expiry close MUST be code 4401");
                assert_eq!(frame.code, WS_AUTH_CLOSE_CODE);
                assert!(!frame.reason.is_empty(), "a human-readable reason is set");
            }
            other => panic!("expected a Close(Some(_)) frame, got {other:?}"),
        }
    }

    #[test]
    fn broadcast_lag_close_is_explicitly_reconnectable() {
        match transient_close_frame("flow lag; resnapshot required") {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.code, 1013);
                assert_eq!(frame.reason.to_string(), "flow lag; resnapshot required");
            }
            other => panic!("expected explicit transient Close(Some), got {other:?}"),
        }
    }

    /// The 4401 close is NOT the unclassified `Close(None)` form (the exact bug: a
    /// no-code close is treated by the SPA as a transient drop, not an auth failure).
    #[test]
    fn auth_close_frame_is_not_an_unclassified_close() {
        assert_ne!(
            auth_close_frame(),
            Message::Close(None),
            "an unclassified Close(None) would be read as a blip, not an expiry"
        );
    }

    // -- (finding 4) inbound-close detection --------------------------------

    /// D7b R2 finding 4: an inbound `Close`, an EOF (`None`), or a read error all mean
    /// the peer hung up → the socket must tear down (the loop `return`s) rather than
    /// linger until `exp`. (Generic helper so it is testable without an `axum::Error`.)
    #[test]
    fn inbound_terminal_on_close_eof_or_error() {
        // Peer sent a Close frame.
        assert!(inbound_is_terminal::<std::io::Error>(&Some(Ok(
            Message::Close(None)
        ))));
        assert!(inbound_is_terminal::<std::io::Error>(&Some(Ok(
            Message::Close(Some(CloseFrame {
                code: 1000,
                reason: "bye".into(),
            }))
        ))));
        // Stream ended (EOF).
        assert!(inbound_is_terminal::<std::io::Error>(&None));
        // Transport error.
        assert!(inbound_is_terminal(&Some(Err(std::io::Error::other(
            "boom"
        )))));
    }

    /// A non-terminal inbound message (data / ping / pong) is IGNORED — the dashboard
    /// socket is server→client only, so we keep serving rather than tearing down.
    #[test]
    fn inbound_non_terminal_keeps_serving() {
        assert!(!inbound_is_terminal::<std::io::Error>(&Some(Ok(
            Message::Text("hi".into())
        ))));
        assert!(!inbound_is_terminal::<std::io::Error>(&Some(Ok(
            Message::Binary(vec![1, 2, 3].into())
        ))));
        assert!(!inbound_is_terminal::<std::io::Error>(&Some(Ok(
            Message::Ping(Vec::new().into())
        ))));
        assert!(!inbound_is_terminal::<std::io::Error>(&Some(Ok(
            Message::Pong(Vec::new().into())
        ))));
    }
}
