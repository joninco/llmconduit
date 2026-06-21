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
//! ## Domain routing (the contract reconciliation)
//! The frozen wire contract keys `usage`/`flow_status` payloads by `api_call_id`
//! (the authoritative flow key) plus an optional `response_id`. The monitor's
//! own `DebugWsMessage::Usage` / `RequestStatus` carry ONLY a `response_id`, so a
//! raw monitor message cannot satisfy the contract directly. The frame builder
//! therefore SPLITS each `DebugUpdate`:
//! - `DebugWsMessage::Usage` → a flow-domain [`DashboardPayload::Usage`] (the
//!   `api_call_id` + `model_served` recovered from the [`crate::dashboard_flow::DashboardFlowStore`]
//!   by `response_id` via its link index).
//! - `DebugWsMessage::RequestStatus` → a flow-domain [`DashboardPayload::FlowStatus`]
//!   (same FlowStore lookup for the authoritative key + served identity + usage).
//! - every OTHER `DebugWsMessage` → a monitor-domain [`DashboardPayload::Monitor`]
//!   (the real message NESTED under `message`, itself still `type`-tagged).
//!
//! If the FlowStore cannot resolve a `response_id` (debug UI's store disabled, or
//! the flow already evicted), the Usage/RequestStatus message falls back to a
//! monitor-domain `Monitor` payload so no transcript data is dropped — the
//! dedicated flow arms are an enrichment, never a lossy filter.
//!
//! ## Sourcing each `DashboardPayload` arm
//! - `Monitor` ← `MonitorHub` (`DebugUpdate` batch), 1:1, nested + tagged.
//! - `Usage` ← the monitor `Usage` message, keyed via the FlowStore (D1/D3).
//! - `FlowStatus` ← the monitor `RequestStatus` message, joined to the FlowStore
//!   record (D1) for `api_call_id`/`model_served`/`usage`/timing.
//! - `MetricTick` ← a periodic tick off the [`crate::metrics::MetricsLayer`] view
//!   (D5), `seq = metrics_seq`, flattened to the `/api/metrics` shape.
//! - `TopologyUpdate` ← `Gateway::provider_health_publisher().latest()` (D4),
//!   `seq = ProviderHealthSnapshot.version`.
//!
//! ## `/debug/ws` is UNCHANGED
//! The bare `DebugWsMessage` contract on `/debug/ws` (debug_ui.rs) is untouched —
//! the batched envelope is dashboard-only.

use crate::dashboard_flow::DashboardFlowStore;
use crate::dashboard_flow::FlowRecord;
use crate::dashboard_flow::FlowStatus;
use crate::dashboard_flow::FlowUsage;
use crate::engine::Gateway;
use crate::metrics::MetricsView;
use crate::metrics::WindowReport;
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

/// How often the Metrics domain emits a [`DashboardPayload::MetricTick`]. One per
/// second mirrors the dashboard's live stats cadence; the frame is skipped when
/// the metrics sequence has not advanced (no new samples), so an idle gateway
/// does not spam identical ticks.
const METRIC_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How often the topology poller checks `provider_health_publisher().latest()`
/// for a new version. The publisher has no broadcast channel, so the socket polls
/// its monotonic `version`; a frame is emitted ONLY when the version advanced
/// (per-domain dedup makes a duplicate harmless, but skipping saves a send).
const TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Wire envelope — the BATCHED DashboardFrame (matches the D9 golden fixtures
// in dashboard-frontend/src/api/ws.fixtures.ts byte-for-byte)
// ---------------------------------------------------------------------------

/// The four per-domain cursors the dashboard tracks. Each [`DashboardFrame`]
/// carries exactly one, and the client dedups whole frames per-domain
/// (`seq <= last_seq[domain]` drops the batch). Serializes snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
pub struct DashboardFrame {
    pub domain: Domain,
    pub seq: u64,
    pub batch: Vec<DashboardPayload>,
}

/// The four per-domain cursors carried on the initial [`SnapshotMessage`] — the
/// `{flow,metrics,topology,monitor}` sequences the SPA installs as its dedup
/// baseline (`commitSnapshot` in `dashboard-frontend/src/api/ws.ts`). Serializes
/// snake_case to the frozen `SeqCursors` contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
    pub windows: MetricWindows,
}

/// The full `/api/topology`-shaped snapshot body (nodes + edges + the price table)
/// PLUS its `topology_seq` cursor. Mirrors the frontend `TopologyResponse`. The
/// price table is empty until D13 wires the price config; an empty map satisfies
/// the frontend `isPriceTable` guard (vacuously every value is a finite price).
#[derive(Debug, Clone, Serialize)]
pub struct TopologySnapshot {
    pub topology_seq: u64,
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
    pub price_table: std::collections::BTreeMap<String, ModelPrice>,
}

/// One model's price row (`/api/topology` `price_table` value). All three rates
/// are finite (the frontend `isModelPrice` guard rejects NaN/Inf). Populated by
/// D13's price config; absent entries simply do not appear.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelPrice {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
    pub cached_per_1k: f64,
}

/// The INITIAL WS message: a `type:"snapshot"` envelope the SPA waits for BEFORE
/// it renders. The frontend (`dashboard-frontend/src/api/ws.ts`) BUFFERS every
/// live [`DashboardFrame`] until this lands (`snapshotApplied`), so it MUST be the
/// FIRST frame on a `/dashboard/ws` connection — else the dashboard never renders
/// (D7b R1 finding 1). It seeds the store's cursors + flow rows + metrics/topology
/// baseline in one atomic install (`restoreLiveSnapshot`); subsequent live frames
/// build on it. Internally tagged `type:"snapshot"` to match the frozen
/// `SnapshotFrame` discriminant.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotMessage {
    /// Discriminant — always `"snapshot"`; the SPA routes on it.
    #[serde(rename = "type")]
    pub kind: SnapshotTag,
    pub cursors: SeqCursors,
    pub flows: Vec<crate::dashboard_flow::SnapshotFlowSummary>,
    /// Metrics baseline (or `null` when metrics are disabled).
    pub metrics: Option<MetricsSnapshot>,
    /// Topology baseline (or `null` when no providers are published yet).
    pub topology: Option<TopologySnapshot>,
}

/// The literal `"snapshot"` tag for [`SnapshotMessage::kind`] (a unit enum so the
/// value is fixed at the type level and serializes to exactly that string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotTag {
    Snapshot,
}

/// One dashboard payload. Internally `type`-tagged (snake_case) to match the
/// frozen contract. The `Monitor` arm NESTS the real (itself-tagged)
/// [`DebugWsMessage`] under `message` — it is NOT flattened (both carry `type`).
/// The `usage`/`flow_status` arms are keyed by `api_call_id` (authoritative) with
/// an optional secondary `response_id`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DashboardPayload {
    /// One per `DebugWsMessage` in the originating `DebugUpdate` batch; the real
    /// message is nested under `message` (itself `type`-tagged).
    Monitor { message: DebugWsMessage },
    /// Per-flow cumulative token usage (flow domain). Keyed by `api_call_id`;
    /// `response_id` is an optional secondary correlation.
    Usage {
        api_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        prompt: i64,
        completion: i64,
        total: i64,
        cached: i64,
        reasoning: i64,
    },
    /// The flat `/api/metrics`-shaped metric tile (metrics domain).
    MetricTick(MetricTick),
    /// Per-flow lifecycle status (flow domain). Keyed by `api_call_id`; carries
    /// the served identity + cumulative usage + timing.
    FlowStatus {
        api_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        status: FlowStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        model_requested: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model_served: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        upstream_target: Option<String>,
        usage: Option<FlowUsage>,
        started_ms: u128,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u128>,
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
#[derive(Debug, Clone, Serialize)]
pub struct MetricTick {
    pub reqs_per_sec: f64,
    pub active_streams: u64,
    pub error_pct: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub tokens_per_sec: f64,
    pub cost_per_min: f64,
    pub windows: MetricWindows,
}

/// The three sliding windows (`m1`/`m5`/`h1`) of a [`MetricTick`].
#[derive(Debug, Clone, Serialize)]
pub struct MetricWindows {
    pub m1: MetricWindow,
    pub m5: MetricWindow,
    pub h1: MetricWindow,
}

/// One sliding-window metric tile. Same fields as the headline tile.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricWindow {
    pub reqs_per_sec: f64,
    pub active_streams: u64,
    pub error_pct: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub tokens_per_sec: f64,
    pub cost_per_min: f64,
}

/// A topology node — the D4 `ProviderHealth` shape, except `catalog_size` is
/// flattened from `Option<u64>` to a non-null `u64` (defaulting `None → 0`): the
/// frozen frontend contract validates `catalog_size` as a required unsigned int
/// (NOT nullable), unlike the other `Option` fields which serde emits as `null`.
/// Every other field mirrors `ProviderHealth` exactly (keys always present, the
/// nullable ones as JSON `null`).
#[derive(Debug, Clone, Serialize)]
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
}

impl TopologyNode {
    fn from_health(health: &crate::upstream::ProviderHealth) -> Self {
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
        }
    }
}

/// A topology edge (gateway → provider). The aggregate throughput/token/cost
/// rates are D5/D13 roll-ups; until a price/throughput aggregation feeds them
/// they serialize as `0.0` (the contract requires the keys present + finite, not
/// a specific value), so the byte-shape is exact while the rich values land in
/// D13.
#[derive(Debug, Clone, Serialize)]
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

/// Build the dashboard frames for ONE monitor [`DebugUpdate`]. The update's
/// `sequence` is the Monitor domain cursor: a SINGLE monitor frame carries EVERY
/// original [`DebugWsMessage`] sibling under that one `seq`, so whole-frame dedup
/// drops a stale WHOLE update, never a live sibling.
///
/// ## Sibling-no-drop (D7b R1 finding 2): enrichment is ADDITIVE, not a move
/// EVERY original message ALWAYS rides the monitor batch as a
/// [`DashboardPayload::Monitor`] — `Usage`/`RequestStatus` are NOT removed from it.
/// On top of that, a resolvable `Usage`/`RequestStatus` ALSO yields a flow-domain
/// enrichment payload (`usage`/`flow_status` keyed by `api_call_id` recovered from
/// `flow_store`). So a `DebugUpdate` still becomes ONE Monitor frame containing all
/// its siblings, PLUS any additive flow-domain enrichment frame.
///
/// ## Event-time flow seq + per-record coalescing (D7b R1 finding 3, hardened R3)
/// The flow frame is stamped with the FlowStore mutation `seq` read ATOMICALLY with
/// the record (`detail_with_seq`), coalesced to the MAX across the resolved records
/// — never a separately-read send-time `flow_seq()`. A queued/replayed older update
/// would otherwise pick up a newer global cursor (bumped by unrelated flows in the
/// gap) and dedup-drop a genuinely newer flow frame on the client.
///
/// R3 (event-time capture, no same-record leapfrog): the `{record COW snapshot,
/// record_seq}` pair for a record is read EXACTLY ONCE — AFTER the message walk —
/// and BOTH that record's `usage` and `flow_status` enrichment payloads are built
/// from that ONE snapshot. So two payloads for one record never carry data from two
/// different mutation snapshots while sharing a seq, and an OLDER event in the batch
/// can never inherit a LATER same-record mutation seq read at a different instant.
/// Multiple events for the SAME record (a `Usage` AND a `RequestStatus`, or repeats)
/// COALESCE onto that single snapshot, carrying the record's LATEST state at its
/// current `record_seq`. The monitor token values still ride the per-record `usage`
/// payload (the record's cumulative `usage` is set on a separate seam), keeping the
/// enrichment additive over what the monitor reported.
///
/// Returns the frames in batch order: the flow enrichment frame (when any) followed
/// by the single monitor frame (always, when the update has any messages).
pub fn frames_for_update(
    update: &DebugUpdate,
    flow_store: &DashboardFlowStore,
) -> Vec<DashboardFrame> {
    let mut monitor_batch: Vec<DashboardPayload> = Vec::new();
    // Per-`response_id` enrichment INTENTS gathered during the message walk, in
    // first-seen order. The actual `{record, seq}` read happens ONCE per response_id
    // AFTER the walk (R3), so an older event cannot read a different (newer) same-
    // record seq than a later event for that record.
    let mut intents: Vec<FlowEnrichIntent> = Vec::new();
    let mut intent_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for message in &update.messages {
        // EVERY original message ALWAYS stays in the monitor batch (finding 2 —
        // sibling-no-drop). The flow arms below are ADDITIVE enrichment.
        monitor_batch.push(DashboardPayload::Monitor {
            message: message.clone(),
        });
        match message {
            DebugWsMessage::Usage {
                response_id,
                prompt,
                completion,
                total,
                cached,
                reasoning,
            } => {
                let intent = intent_for(&mut intents, &mut intent_index, response_id);
                // Latest Usage wins for a repeated response_id within one update.
                intent.usage = Some(UsageTokens {
                    prompt: *prompt,
                    completion: *completion,
                    total: *total,
                    cached: *cached,
                    reasoning: *reasoning,
                });
            }
            DebugWsMessage::RequestStatus {
                response_id,
                completed_at_ms,
                ..
            } => {
                let intent = intent_for(&mut intents, &mut intent_index, response_id);
                // Latest status fallback timestamp wins for a repeated response_id.
                intent.status_completed_at_ms = Some(*completed_at_ms);
            }
            _ => {}
        }
    }

    // Resolve each intent to its record ONCE (R3: one atomic `{record, seq}` read per
    // response_id) and build that record's payloads from the single snapshot. Coalesce
    // by the RESOLVED record identity so two response_ids mapping to one record do not
    // double-emit or stamp two seqs — the second occurrence folds onto the first.
    let mut flow_batch: Vec<DashboardPayload> = Vec::new();
    let mut flow_seq: Option<u64> = None;
    let mut seen_records: std::collections::HashSet<String> = std::collections::HashSet::new();
    for intent in &intents {
        let Some((record, seq)) = flow_store.detail_with_seq(&intent.response_id) else {
            continue;
        };
        // Coalesce by the authoritative record key: a record already enriched (via an
        // earlier response_id that resolved to it) is not emitted twice.
        if !seen_records.insert(record.api_call_id.clone()) {
            continue;
        }
        flow_seq = Some(flow_seq.map_or(seq, |cur| cur.max(seq)));
        if let Some(tokens) = intent.usage {
            flow_batch.push(DashboardPayload::Usage {
                api_call_id: record.api_call_id.clone(),
                response_id: Some(intent.response_id.clone()),
                prompt: tokens.prompt,
                completion: tokens.completion,
                total: tokens.total,
                cached: tokens.cached,
                reasoning: tokens.reasoning,
            });
        }
        if let Some(completed_at_ms) = intent.status_completed_at_ms {
            flow_batch.push(flow_status_payload(&record, completed_at_ms));
        }
    }

    let mut frames = Vec::new();
    if let Some(seq) = flow_seq {
        // Stamp with the event-time FlowStore seq (finding 3), not a send-time
        // global `flow_seq()`. Per-domain dedup is the client's job; the server
        // just stamps the correct `{domain, seq}`.
        frames.push(DashboardFrame {
            domain: Domain::Flow,
            seq,
            batch: flow_batch,
        });
    }
    if !monitor_batch.is_empty() {
        frames.push(DashboardFrame {
            domain: Domain::Monitor,
            seq: update.sequence,
            batch: monitor_batch,
        });
    }
    frames
}

/// The five monitor-reported cumulative token counts carried by a `usage` enrichment
/// payload (the same fields as a [`DebugWsMessage::Usage`]).
#[derive(Debug, Clone, Copy)]
struct UsageTokens {
    prompt: i64,
    completion: i64,
    total: i64,
    cached: i64,
    reasoning: i64,
}

/// A pending flow-domain enrichment for ONE `response_id`, accumulated across the
/// messages of a single `DebugUpdate` BEFORE the record is read (R3). Both arms
/// COALESCE onto the same intent so the record is resolved exactly once and its
/// `usage` + `flow_status` payloads share one `{record, seq}` snapshot.
#[derive(Debug, Clone)]
struct FlowEnrichIntent {
    response_id: String,
    /// The latest monitor `Usage` token counts seen for this response_id (if any).
    usage: Option<UsageTokens>,
    /// The latest monitor `RequestStatus` completion stamp (if a status was seen);
    /// the inner `Option` is the message's own `completed_at_ms` (may be `None`).
    status_completed_at_ms: Option<Option<u128>>,
}

/// Get the mutable [`FlowEnrichIntent`] for `response_id`, creating it (preserving
/// first-seen order) on first sight. Keyed so repeated messages for one response_id
/// fold onto a single intent.
fn intent_for<'a>(
    intents: &'a mut Vec<FlowEnrichIntent>,
    index: &mut std::collections::HashMap<String, usize>,
    response_id: &str,
) -> &'a mut FlowEnrichIntent {
    let pos = *index.entry(response_id.to_string()).or_insert_with(|| {
        intents.push(FlowEnrichIntent {
            response_id: response_id.to_string(),
            usage: None,
            status_completed_at_ms: None,
        });
        intents.len() - 1
    });
    &mut intents[pos]
}

/// Build a flow-domain `FlowStatus` payload from the authoritative FlowStore
/// [`FlowRecord`] (D1): the `api_call_id` (authoritative key), the served identity,
/// cumulative usage, timing, AND the lifecycle `status` all come from the record.
///
/// The status is taken from `record.status` (the FlowStore [`FlowStatus`], which
/// HAS a `Cancelled` variant) rather than re-derived from the monitor message's
/// `DebugRequestStatus` (which has only running/completed/failed) — D7b R1 finding
/// 4: a client hang-up the FlowStore finalized `Cancelled` must serialize
/// `cancelled`, not be flattened to `failed`. The monitor `completed_at_ms` is used
/// ONLY as a fallback to derive `elapsed_ms` when the record has not finalized its
/// own measured elapsed yet.
fn flow_status_payload(record: &FlowRecord, completed_at_ms: Option<u128>) -> DashboardPayload {
    // Prefer the record's measured elapsed; fall back to a wall-clock delta from
    // the monitor's completion stamp (when the record has not finalized yet).
    let elapsed_ms = record
        .elapsed_ms
        .or_else(|| completed_at_ms.map(|done| done.saturating_sub(record.started_ms)));
    DashboardPayload::FlowStatus {
        api_call_id: record.api_call_id.clone(),
        response_id: record.response_id.clone(),
        status: record.status,
        model_requested: record.model_requested.clone(),
        model_served: record.model_served.clone(),
        upstream_target: record.upstream_target.clone(),
        usage: record.usage,
        started_ms: record.started_ms,
        elapsed_ms,
    }
}

/// Build a metrics-domain `MetricTick` frame from a collapsed [`MetricsView`]
/// (D5). The headline tile repeats the `m1` window. Cost rates require a price
/// table (D13) not yet wired, so they are `0.0` here (the contract requires the
/// fields present + finite); the shape is exact.
pub fn metric_tick_frame(view: &MetricsView, seq: u64) -> DashboardFrame {
    let m1 = window_tile(&view.window_1m);
    DashboardFrame {
        domain: Domain::Metrics,
        seq,
        batch: vec![DashboardPayload::MetricTick(MetricTick {
            reqs_per_sec: m1.reqs_per_sec,
            active_streams: m1.active_streams,
            error_pct: m1.error_pct,
            p50: m1.p50,
            p95: m1.p95,
            p99: m1.p99,
            tokens_per_sec: m1.tokens_per_sec,
            cost_per_min: m1.cost_per_min,
            windows: MetricWindows {
                m1,
                m5: window_tile(&view.window_5m),
                h1: window_tile(&view.window_1h),
            },
        })],
    }
}

/// Collapse one [`WindowReport`] into a flat metric tile. `reqs_per_sec`/
/// `tokens_per_sec`/`error_pct` are derived from the window's aggregate counts;
/// `active_streams`/`cost_per_min` are D5/D13 roll-ups not yet wired (0.0 — shape
/// exact, values land in D13). The window length is unknown to this view, so the
/// rate fields report the window's totals; D13's REST aggregation refines them.
fn window_tile(report: &WindowReport) -> MetricWindow {
    let percentiles = report.percentiles();
    let total: u64 = report.total_count();
    let errors: u64 = report
        .buckets
        .iter()
        .filter(|(key, _)| key.status == crate::metrics::StatusClass::Error)
        .map(|(_, counts)| counts.count)
        .fold(0u64, u64::saturating_add);
    let error_pct = if total > 0 {
        (errors as f64) / (total as f64) * 100.0
    } else {
        0.0
    };
    MetricWindow {
        // Raw counts; D13's REST view divides by the true window seconds. The
        // contract only requires a finite number here.
        reqs_per_sec: total as f64,
        active_streams: 0,
        error_pct,
        p50: percentiles.p50,
        p95: percentiles.p95,
        p99: percentiles.p99,
        tokens_per_sec: 0.0,
        cost_per_min: 0.0,
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
/// [`MetricsView`] (D5) + its `metrics_seq`. Same flat tile + three windows as a
/// live [`DashboardPayload::MetricTick`], with the cursor attached.
fn metrics_snapshot(view: &MetricsView, metrics_seq: u64) -> MetricsSnapshot {
    let m1 = window_tile(&view.window_1m);
    MetricsSnapshot {
        metrics_seq,
        reqs_per_sec: m1.reqs_per_sec,
        active_streams: m1.active_streams,
        error_pct: m1.error_pct,
        p50: m1.p50,
        p95: m1.p95,
        p99: m1.p99,
        tokens_per_sec: m1.tokens_per_sec,
        cost_per_min: m1.cost_per_min,
        windows: MetricWindows {
            m1,
            m5: window_tile(&view.window_5m),
            h1: window_tile(&view.window_1h),
        },
    }
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
/// cuts. The cursors come from the SAME reads that built `flows`/`metrics`/
/// `topology` so the client's dedup baseline matches the seeded rows.
fn snapshot_message(
    flows: Vec<crate::dashboard_flow::SnapshotFlowSummary>,
    flow_seq: u64,
    metrics: Option<MetricsSnapshot>,
    topology: Option<TopologySnapshot>,
    monitor_seq: u64,
) -> SnapshotMessage {
    SnapshotMessage {
        kind: SnapshotTag::Snapshot,
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
/// replay the retained monitor transcript as batched frames, then multiplex the live
/// monitor broadcast, the periodic metric tick, and the topology poller — all racing
/// the cookie-`exp` close timer so nothing is delivered past expiry.
/// `session_exp == u64::MAX` (dev-open) yields an effectively-infinite timer.
async fn dashboard_socket(socket: WebSocket, gateway: Arc<Gateway>, session_exp: u64) {
    let flow_store = gateway.flow_store().clone();
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
    // watermarks the loop below resumes from, so the next periodic tick is the first
    // NEW frame (no redundant baseline frame, no self-dedup). The monitor cursor is
    // 0: the snapshot body carries NO transcript, so the retained-transcript replay
    // below (seq = `last_sequence`) is ACCEPTED, seeding the inspector history.
    //
    // (finding 2) Each domain's body + its dedup cursor are captured ATOMICALLY (one
    // lock hold per store), so the snapshot never pairs an older body with a newer
    // cursor — which would permanently dedup-drop that mutation's own live frame:
    //  - metrics:  `view_with_seq()`         (view + metrics_seq under one metrics lock)
    //  - flows:    `snapshot_summaries_with_seq()` (summaries + flow_seq under one lock)
    //  - topology: ONE `latest()` read       (the `version` lives INSIDE the snapshot)
    let (metrics_view, metrics_seq) = gateway.metrics().view_with_seq();
    let mut last_metrics_seq = metrics_seq;
    let metrics = Some(metrics_snapshot(&metrics_view, metrics_seq));
    let topo = gateway.provider_health_publisher().latest();
    let mut last_topology_version = topo.version;
    let topology = Some(topology_snapshot(&topo));
    let (flow_summaries, flow_seq) = flow_store.snapshot_summaries_with_seq();
    let initial = snapshot_message(
        flow_summaries,
        flow_seq,
        metrics,
        topology,
        // monitor baseline 0 — the transcript rides the replay frame below.
        0,
    );
    // Replay the retained monitor transcript as ONE batched monitor frame (its
    // messages share `snapshot.last_sequence`). Enrichment flow frames it emits are
    // stamped at event-time seqs ≤ the snapshot's `flow_seq`, so the client dedups
    // them (the snapshot already carries those flows) — only the monitor-domain
    // transcript frame advances the (snapshot-0) monitor cursor.
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

    let mut metric_ticker = tokio::time::interval(METRIC_TICK_INTERVAL);
    metric_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `interval` tick fires immediately; consume it so the loop's first
    // metric frame is a genuinely NEW sample, not an instant re-send of the metrics
    // baseline already carried by the initial snapshot (its `metrics_seq` seeds
    // `last_metrics_seq`, so the loop emits only once the seq advances).
    metric_ticker.tick().await;
    let mut topology_ticker = tokio::time::interval(TOPOLOGY_POLL_INTERVAL);
    topology_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    topology_ticker.tick().await;

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
                    Err(broadcast::error::RecvError::Lagged(_)) => return,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = metric_ticker.tick() => {
                // Atomic view + seq (finding 2): pair the tile body with its own cursor.
                let (view, seq) = gateway.metrics().view_with_seq();
                if seq != last_metrics_seq {
                    last_metrics_seq = seq;
                    let frame = metric_tick_frame(&view, seq);
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
            _ = topology_ticker.tick() => {
                let snapshot = gateway.provider_health_publisher().latest();
                if snapshot.version != last_topology_version {
                    last_topology_version = snapshot.version;
                    let frame = topology_frame(&snapshot);
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

/// Send the [`auth_close_frame`] on EVERY expiry path (finding 3). Best-effort: the
/// peer may already be gone, and errors are ignored since we are tearing down anyway.
async fn send_auth_close(sink: &mut SplitSink<WebSocket, Message>) {
    let _ = sink.send(auth_close_frame()).await;
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
/// `exp` (≤ 1 h) is always far below this.
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

    /// With a live FlowStore record (linked `response_id → api_call_id`), a monitor
    /// `Usage`/`RequestStatus` yields an ADDITIVE flow-domain `usage`/`flow_status`
    /// enrichment payload keyed by `api_call_id` — WITHOUT being removed from the
    /// monitor batch (D7b R1 finding 2: the monitor frame still carries ALL THREE
    /// original siblings, sibling-no-drop). The flow `status` is the record's
    /// `FlowStatus` (here `Open` — the record was opened, never finalized), not the
    /// monitor message's `Completed` (finding 4).
    #[test]
    fn usage_status_enrich_flow_domain_additively_without_dropping_monitor_siblings() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_001".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            Some(capture_body(b"{}")),
        );
        store.link("resp_001".to_string(), "api_001".to_string());

        let update = DebugUpdate {
            sequence: 4,
            messages: vec![
                DebugWsMessage::SegmentAppend {
                    response_id: "resp_001".to_string(),
                    segment: DebugSegment {
                        timestamp_ms: 1,
                        kind: DebugSegmentKind::Output,
                        text: "hi".to_string(),
                    },
                },
                DebugWsMessage::Usage {
                    response_id: "resp_001".to_string(),
                    prompt: 812,
                    completion: 240,
                    total: 1052,
                    cached: 128,
                    reasoning: 0,
                },
                DebugWsMessage::RequestStatus {
                    response_id: "resp_001".to_string(),
                    status: DebugRequestStatus::Completed,
                    completed_at_ms: Some(1718900000003),
                    error: None,
                },
            ],
        };
        let frames = frames_for_update(&update, &store);
        // An additive flow frame (usage + status) AND a monitor frame (ALL 3 originals).
        assert_eq!(frames.len(), 2);
        let flow = frames
            .iter()
            .find(|f| f.domain == Domain::Flow)
            .expect("flow frame");
        assert_eq!(flow.batch.len(), 2, "usage + flow_status enrichment");
        match &flow.batch[0] {
            DashboardPayload::Usage {
                api_call_id,
                response_id,
                total,
                cached,
                ..
            } => {
                assert_eq!(
                    api_call_id, "api_001",
                    "keyed by api_call_id, not response_id"
                );
                assert_eq!(response_id.as_deref(), Some("resp_001"));
                assert_eq!(*total, 1052);
                assert_eq!(*cached, 128);
            }
            other => panic!("expected usage payload, got {other:?}"),
        }
        match &flow.batch[1] {
            DashboardPayload::FlowStatus {
                api_call_id,
                status,
                ..
            } => {
                assert_eq!(api_call_id, "api_001");
                // Record status (Open), NOT the monitor message's Completed (finding 4).
                assert_eq!(*status, FlowStatus::Open);
            }
            other => panic!("expected flow_status payload, got {other:?}"),
        }
        let monitor = frames
            .iter()
            .find(|f| f.domain == Domain::Monitor)
            .expect("monitor frame");
        assert_eq!(
            monitor.batch.len(),
            3,
            "sibling-no-drop: ALL three originals stay in the monitor batch"
        );
        // The enrichment is ADDITIVE — the usage + status messages are STILL present
        // in the monitor batch, not moved out of it.
        let monitor_kinds: Vec<&DebugWsMessage> = monitor
            .batch
            .iter()
            .map(|p| match p {
                DashboardPayload::Monitor { message } => message,
                other => panic!("monitor batch must hold only Monitor payloads, got {other:?}"),
            })
            .collect();
        assert!(
            monitor_kinds
                .iter()
                .any(|m| matches!(m, DebugWsMessage::Usage { .. })),
            "the Usage sibling is retained in the monitor batch"
        );
        assert!(
            monitor_kinds
                .iter()
                .any(|m| matches!(m, DebugWsMessage::RequestStatus { .. })),
            "the RequestStatus sibling is retained in the monitor batch"
        );
    }

    /// D7b R1 finding 4: a flow the FlowStore finalized `Cancelled` (client hang-up)
    /// serializes `status: "cancelled"`, NOT flattened to `failed`. The monitor
    /// `RequestStatus` only carries `failed`/`completed`/`running`, so the payload
    /// MUST take its status from the record's `FlowStatus` (which has `Cancelled`).
    #[test]
    fn cancelled_flow_serializes_cancelled_not_failed() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_cxl".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            Some(capture_body(b"{}")),
        );
        store.link("resp_cxl".to_string(), "api_cxl".to_string());
        // The FlowStore finalizes the flow Cancelled (the D3 client-hangup terminal).
        store.finalize(
            "api_cxl",
            FlowStatus::Cancelled,
            Some("client-hangup".to_string()),
            None,
        );

        let update = DebugUpdate {
            sequence: 7,
            messages: vec![DebugWsMessage::RequestStatus {
                response_id: "resp_cxl".to_string(),
                // The monitor's closest status is Failed — but the record says Cancelled.
                status: DebugRequestStatus::Failed,
                completed_at_ms: Some(20),
                error: Some("client-hangup".to_string()),
            }],
        };
        let frames = frames_for_update(&update, &store);
        let flow = frames
            .iter()
            .find(|f| f.domain == Domain::Flow)
            .expect("flow frame");
        let status = flow
            .batch
            .iter()
            .find_map(|p| match p {
                DashboardPayload::FlowStatus { status, .. } => Some(*status),
                _ => None,
            })
            .expect("flow_status payload");
        assert_eq!(
            status,
            FlowStatus::Cancelled,
            "record's Cancelled wins over the monitor's Failed"
        );
        // And it serializes to the snake_case wire string the frontend expects.
        let value = serde_json::to_value(flow).expect("serialize");
        let wire_status = &value["batch"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["type"] == "flow_status")
            .unwrap()["status"];
        assert_eq!(*wire_status, serde_json::json!("cancelled"));
    }

    /// D7b R1 finding 3: the flow frame is stamped with the FlowStore seq AS OF THE
    /// EVENT (read atomically with the record via `detail_with_seq`), NEVER a
    /// send-time global `flow_seq()`. So a LATER-processed update whose record was
    /// resolved earlier cannot carry a cursor that leapfrogs — and, crucially, an
    /// out-of-order replay does not stamp an OLD event with a NEWER cursor that would
    /// dedup-drop the genuinely newer flow frame on the client.
    #[test]
    fn flow_frame_seq_is_event_time_not_send_time_global() {
        let store = DashboardFlowStore::new();
        // Flow A opened + linked, then finalized → its record's seq is some value Sa.
        store.open(
            "api_a".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            Some(capture_body(b"{}")),
        );
        store.link("resp_a".to_string(), "api_a".to_string());
        // Capture the FlowStore seq at which flow A's record is current.
        let (_record_a, seq_a) = store.detail_with_seq("resp_a").expect("flow A resolves");

        // Now MANY unrelated flows mutate the store, bumping the GLOBAL flow_seq far
        // past `seq_a` (this models the gap between A's event and its update being
        // processed off the broadcast channel).
        for i in 0..10 {
            let id = format!("api_other_{i}");
            store.open(
                id.clone(),
                "POST".to_string(),
                "/v1/responses".to_string(),
                redact_headers(&HeaderMap::new()),
                Some(capture_body(b"{}")),
            );
        }
        let global_now = store.flow_seq();
        assert!(
            global_now > seq_a,
            "the global cursor advanced past flow A's event seq"
        );

        // Build the frame for flow A's (now older) status update.
        let update = DebugUpdate {
            sequence: 3,
            messages: vec![DebugWsMessage::RequestStatus {
                response_id: "resp_a".to_string(),
                status: DebugRequestStatus::Completed,
                completed_at_ms: Some(5),
                error: None,
            }],
        };
        let frames = frames_for_update(&update, &store);
        let flow = frames
            .iter()
            .find(|f| f.domain == Domain::Flow)
            .expect("flow frame");
        // The frame's seq is the record's CURRENT event-time seq (read in this build),
        // NOT a value leapfrogged by the unrelated flows beyond what the record needs.
        // Re-read the record's seq now to assert exact equality.
        let (_again, seq_a_now) = store.detail_with_seq("resp_a").expect("flow A resolves");
        assert_eq!(
            flow.seq, seq_a_now,
            "flow frame seq == flow A's event-time record seq, not the global send-time cursor"
        );
        // It is NOT stamped with the (larger) send-time global flow_seq() — that is
        // the exact bug: a separate send-time read would over-advance the cursor.
        assert!(
            flow.seq <= global_now,
            "event-time seq never exceeds the global cursor"
        );
    }

    /// D7b R3 HIGH: multiple events for the SAME record within one `DebugUpdate`
    /// (a `Usage` AND a `RequestStatus`) COALESCE onto ONE flow frame whose payloads
    /// are built from a SINGLE `{record, seq}` snapshot — so an OLDER event in the
    /// batch can never inherit a LATER same-record mutation seq read at a different
    /// instant, and the two payloads never carry data from two different snapshots
    /// while sharing one seq. The frame's seq is the record's CURRENT (latest-mutation)
    /// `record_seq`, captured once after the message walk. Drive two distinct mutations
    /// on the record (open+link, then record_usage) so its `record_seq` has advanced,
    /// then build a frame from an update carrying both a Usage and a RequestStatus for
    /// that record.
    #[test]
    fn same_record_events_coalesce_to_one_frame_at_current_record_seq() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_a".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            Some(capture_body(b"{}")),
        );
        store.link("resp_a".to_string(), "api_a".to_string());
        // A SECOND mutation on the same record advances its record_seq past the open.
        store.record_usage(
            "api_a",
            FlowUsage {
                prompt: 10,
                completion: 5,
                total: 15,
                cached: 0,
                reasoning: 0,
            },
        );
        // The record's CURRENT event-time seq — the value the coalesced frame must carry.
        let (_rec, record_seq_now) = store.detail_with_seq("resp_a").expect("flow A resolves");

        // One update carrying BOTH a Usage and a RequestStatus for the SAME record.
        let update = DebugUpdate {
            sequence: 3,
            messages: vec![
                DebugWsMessage::Usage {
                    response_id: "resp_a".to_string(),
                    prompt: 11,
                    completion: 6,
                    total: 17,
                    cached: 1,
                    reasoning: 0,
                },
                DebugWsMessage::RequestStatus {
                    response_id: "resp_a".to_string(),
                    status: DebugRequestStatus::Completed,
                    completed_at_ms: Some(99),
                    error: None,
                },
            ],
        };
        let frames = frames_for_update(&update, &store);
        // Exactly ONE flow frame (coalesced), plus the monitor frame.
        let flow_frames: Vec<&DashboardFrame> =
            frames.iter().filter(|f| f.domain == Domain::Flow).collect();
        assert_eq!(
            flow_frames.len(),
            1,
            "same-record events coalesce to ONE frame"
        );
        let flow = flow_frames[0];
        // Its seq is the record's CURRENT record_seq — NOT leapfrogged, NOT a per-event
        // re-read that could differ between the Usage and the RequestStatus.
        assert_eq!(
            flow.seq, record_seq_now,
            "frame seq is the record's current event-time record_seq"
        );
        // Both the usage AND the flow_status enrichment ride that single frame, keyed by
        // the authoritative api_call_id — derived from the one snapshot, sharing one seq.
        assert_eq!(
            flow.batch.len(),
            2,
            "usage + flow_status, both for the one record"
        );
        assert!(
            flow.batch
                .iter()
                .any(|p| matches!(p, DashboardPayload::Usage { api_call_id, .. } if api_call_id == "api_a")),
            "the usage payload is present, keyed by api_call_id"
        );
        assert!(
            flow.batch
                .iter()
                .any(|p| matches!(p, DashboardPayload::FlowStatus { api_call_id, .. } if api_call_id == "api_a")),
            "the flow_status payload is present, keyed by api_call_id"
        );
    }

    /// D7b R3 HIGH (the leapfrog/drop regression): an OLDER queued event for a record
    /// does NOT inherit a NEWER mutation's seq, and does NOT cause the newer/final frame
    /// to be dropped. Build the OLDER event's flow frame FIRST (it is stamped at the
    /// record's seq AT THAT TIME), THEN mutate the record again and build the FINAL
    /// event's frame: the final frame must carry a STRICTLY GREATER seq, so the client's
    /// per-domain dedup (`seq <= last_seq` drops) ACCEPTS it rather than dropping it.
    #[test]
    fn older_queued_event_does_not_inherit_newer_seq_or_drop_final_frame() {
        let store = DashboardFlowStore::new();
        store.open(
            "api_a".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            redact_headers(&HeaderMap::new()),
            Some(capture_body(b"{}")),
        );
        store.link("resp_a".to_string(), "api_a".to_string());

        // The OLDER event is built/stamped NOW, at the record's current seq.
        let older_update = DebugUpdate {
            sequence: 1,
            messages: vec![DebugWsMessage::Usage {
                response_id: "resp_a".to_string(),
                prompt: 1,
                completion: 1,
                total: 2,
                cached: 0,
                reasoning: 0,
            }],
        };
        let older_frames = frames_for_update(&older_update, &store);
        let older_seq = older_frames
            .iter()
            .find(|f| f.domain == Domain::Flow)
            .expect("older flow frame")
            .seq;

        // The record mutates AGAIN (a newer, genuinely-later flow event) — advancing its
        // own record_seq strictly past the older-event stamp.
        store.finalize(
            "api_a",
            FlowStatus::Completed,
            Some("done".to_string()),
            None,
        );
        let final_update = DebugUpdate {
            sequence: 2,
            messages: vec![DebugWsMessage::RequestStatus {
                response_id: "resp_a".to_string(),
                status: DebugRequestStatus::Completed,
                completed_at_ms: Some(50),
                error: None,
            }],
        };
        let final_frames = frames_for_update(&final_update, &store);
        let final_seq = final_frames
            .iter()
            .find(|f| f.domain == Domain::Flow)
            .expect("final flow frame")
            .seq;

        // The crux: the older event did NOT inherit the newer mutation's seq (it was
        // stamped at the record's THEN-current seq), so the final frame is STRICTLY
        // newer and the client's `seq <= last_seq` dedup ACCEPTS it (no drop).
        assert!(
            final_seq > older_seq,
            "the newer/final flow frame's seq ({final_seq}) is strictly greater than the \
             older queued event's seq ({older_seq}) — so it is not dedup-dropped"
        );
    }

    // -- byte-for-byte fixture parity (the D9 golden fixtures) -------------

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
    /// `api_call_id` + `response_id` + the five token fields, under domain `flow`.
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
                cached: 128,
                reasoning: 0,
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

    /// The `flow_status` payload matches `GOLDEN_FLOW_STATUS_FRAME_JSON`:
    /// `type:"flow_status"`, `api_call_id`, `response_id`, `status`,
    /// `model_requested`/`model_served`/`upstream_target`, a nested `usage`,
    /// `started_ms`, `elapsed_ms`.
    #[test]
    fn flow_status_frame_matches_golden_fixture_bytes() {
        let frame = DashboardFrame {
            domain: Domain::Flow,
            seq: 5,
            batch: vec![DashboardPayload::FlowStatus {
                api_call_id: "api_001".to_string(),
                response_id: Some("resp_001".to_string()),
                status: FlowStatus::Completed,
                model_requested: Some("gpt-4o".to_string()),
                model_served: Some("llama-3.1-70b".to_string()),
                upstream_target: Some("vllm-a".to_string()),
                usage: Some(FlowUsage {
                    prompt: 812,
                    completion: 512,
                    total: 1324,
                    cached: 128,
                    reasoning: 0,
                }),
                started_ms: 1718900000000,
                elapsed_ms: Some(3100),
            }],
        };
        let got: serde_json::Value = serde_json::to_value(&frame).expect("serialize");
        let want: serde_json::Value = serde_json::json!({
            "domain": "flow",
            "seq": 5,
            "batch": [
                {
                    "type": "flow_status",
                    "api_call_id": "api_001",
                    "response_id": "resp_001",
                    "status": "completed",
                    "model_requested": "gpt-4o",
                    "model_served": "llama-3.1-70b",
                    "upstream_target": "vllm-a",
                    "usage": { "prompt": 812, "completion": 512, "total": 1324, "cached": 128, "reasoning": 0 },
                    "started_ms": 1718900000000u64,
                    "elapsed_ms": 3100
                }
            ]
        });
        assert_eq!(got, want);
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
                    "windows": {
                        "m1": { "reqs_per_sec": 4.2, "active_streams": 3, "error_pct": 1.1, "p50": 180.0, "p95": 920.0, "p99": 1840.0, "tokens_per_sec": 142.0, "cost_per_min": 0.21 },
                        "m5": { "reqs_per_sec": 3.8, "active_streams": 3, "error_pct": 1.0, "p50": 175.0, "p95": 900.0, "p99": 1800.0, "tokens_per_sec": 128.0, "cost_per_min": 0.19 },
                        "h1": { "reqs_per_sec": 2.9, "active_streams": 2, "error_pct": 0.8, "p50": 160.0, "p95": 850.0, "p99": 1700.0, "tokens_per_sec": 100.0, "cost_per_min": 0.15 }
                    }
                }
            ]
        });
        assert_eq!(got, want);
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
        );
        store.finalize("api_001", FlowStatus::Completed, None, None);
        let flows = store.snapshot_summaries();
        let flow_seq = store.flow_seq();

        let metrics = Some(metrics_snapshot(&MetricsView::default(), 7));
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
        // flows: an array of body-free summaries keyed by api_call_id (no body keys).
        let flows = value["flows"].as_array().expect("flows is an array");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0]["api_call_id"], serde_json::json!("api_001"));
        assert_eq!(flows[0]["status"], serde_json::json!("completed"));
        assert!(
            flows[0].get("inbound_body").is_none(),
            "summaries are body-free"
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
