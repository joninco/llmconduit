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
use crate::monitor::DebugRequestStatus;
use crate::monitor::DebugUpdate;
use crate::monitor::DebugWsMessage;
use crate::upstream::ProviderHealthSnapshot;
use axum::extract::State;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::broadcast;

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
/// `sequence` is the Monitor domain cursor: a SINGLE monitor frame carries every
/// non-flow message under that one `seq`, so whole-frame dedup never drops a
/// sibling. `Usage`/`RequestStatus` messages are routed OUT to the flow domain
/// (keyed by `api_call_id` recovered from `flow_store`), each as its own
/// flow-domain frame stamped with the FlowStore's `flow_seq` at this instant.
///
/// Returns the frames in batch order: any flow frames (usage/status) followed by
/// the single monitor frame (when it has any messages). A `Usage`/`RequestStatus`
/// whose `response_id` does not resolve in the FlowStore falls back into the
/// monitor batch, so no message is ever dropped.
pub fn frames_for_update(
    update: &DebugUpdate,
    flow_store: &DashboardFlowStore,
) -> Vec<DashboardFrame> {
    let mut monitor_batch: Vec<DashboardPayload> = Vec::new();
    let mut flow_batch: Vec<DashboardPayload> = Vec::new();

    for message in &update.messages {
        match message {
            DebugWsMessage::Usage {
                response_id,
                prompt,
                completion,
                total,
                cached,
                reasoning,
            } => match flow_store.detail(response_id) {
                Some(record) => flow_batch.push(DashboardPayload::Usage {
                    api_call_id: record.api_call_id.clone(),
                    response_id: Some(response_id.clone()),
                    prompt: *prompt,
                    completion: *completion,
                    total: *total,
                    cached: *cached,
                    reasoning: *reasoning,
                }),
                // Unresolved (store disabled / flow evicted): keep it in the
                // monitor batch so the transcript usage frame is never dropped.
                None => monitor_batch.push(DashboardPayload::Monitor {
                    message: message.clone(),
                }),
            },
            DebugWsMessage::RequestStatus {
                response_id,
                status,
                completed_at_ms,
                error: _,
            } => match flow_store.detail(response_id) {
                Some(record) => {
                    flow_batch.push(flow_status_payload(&record, *status, *completed_at_ms))
                }
                None => monitor_batch.push(DashboardPayload::Monitor {
                    message: message.clone(),
                }),
            },
            other => monitor_batch.push(DashboardPayload::Monitor {
                message: other.clone(),
            }),
        }
    }

    let mut frames = Vec::new();
    if !flow_batch.is_empty() {
        // The flow domain's cursor is the FlowStore mutation sequence at this
        // instant (the status/usage transitions that produced these payloads
        // already bumped it). Per-domain dedup is the client's job; the server
        // just stamps the correct `{domain, seq}`.
        frames.push(DashboardFrame {
            domain: Domain::Flow,
            seq: flow_store.flow_seq(),
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

/// Build a flow-domain `FlowStatus` payload by joining the monitor status
/// transition to the authoritative FlowStore [`FlowRecord`] (D1): the
/// `api_call_id` (authoritative key), the served identity, cumulative usage, and
/// timing come from the record; the live `status`/`completed_at_ms` come from the
/// monitor message. The monitor's `DebugRequestStatus` maps to the FlowStore
/// `FlowStatus`; an unmapped value defaults to the record's own status.
fn flow_status_payload(
    record: &FlowRecord,
    monitor_status: DebugRequestStatus,
    completed_at_ms: Option<u128>,
) -> DashboardPayload {
    let status = match monitor_status {
        DebugRequestStatus::Running => FlowStatus::Open,
        DebugRequestStatus::Completed => FlowStatus::Completed,
        DebugRequestStatus::Failed => FlowStatus::Failed,
    };
    // Prefer the record's measured elapsed; fall back to a wall-clock delta from
    // the monitor's completion stamp (when the record has not finalized yet).
    let elapsed_ms = record
        .elapsed_ms
        .or_else(|| completed_at_ms.map(|done| done.saturating_sub(record.started_ms)));
    DashboardPayload::FlowStatus {
        api_call_id: record.api_call_id.clone(),
        response_id: record.response_id.clone(),
        status,
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

/// Drive one `/dashboard/ws` connection: replay the retained monitor snapshot as
/// batched frames, then multiplex the live monitor broadcast, the periodic metric
/// tick, and the topology poller — all racing the cookie-`exp` close timer so no
/// frame is delivered past expiry. `session_exp == u64::MAX` (dev-open) yields an
/// effectively-infinite timer.
async fn dashboard_socket(mut socket: WebSocket, gateway: Arc<Gateway>, session_exp: u64) {
    let flow_store = gateway.flow_store().clone();
    let mut monitor_rx = gateway.subscribe_monitor();
    let snapshot = gateway.debug_snapshot();

    // Arm the expiry timer BEFORE any send so a near-/already-expired cookie
    // closes the socket even mid-replay.
    let expiry = wait_for_session_expiry(session_exp);
    tokio::pin!(expiry);

    // Replay the retained monitor snapshot as ONE batched monitor frame (its
    // messages share `snapshot.last_sequence`), routing usage/status to the flow
    // domain exactly like the live path. The snapshot is a point-in-time cut, so
    // a single update with `sequence = snapshot.last_sequence` models it.
    let snapshot_update = DebugUpdate {
        sequence: snapshot.last_sequence,
        messages: snapshot.messages.clone(),
    };
    let snapshot_frames = frames_for_update(&snapshot_update, &flow_store);
    match send_frames(&snapshot_frames, expiry.as_mut(), &mut socket).await {
        SendOutcome::Completed => {}
        SendOutcome::Expired => {
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
        SendOutcome::Failed => return,
    }

    // Send an initial metric + topology frame so a fresh client has a baseline
    // before the first periodic tick.
    let mut last_metrics_seq = gateway.metrics().metrics_seq();
    let mut last_topology_version = {
        let snapshot = gateway.provider_health_publisher().latest();
        let frame = topology_frame(&snapshot);
        let version = snapshot.version;
        match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut socket).await {
            SendOutcome::Completed => {}
            SendOutcome::Expired => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            SendOutcome::Failed => return,
        }
        version
    };
    {
        let frame = metric_tick_frame(&gateway.metrics().view(), last_metrics_seq);
        match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut socket).await {
            SendOutcome::Completed => {}
            SendOutcome::Expired => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            SendOutcome::Failed => return,
        }
    }

    let mut metric_ticker = tokio::time::interval(METRIC_TICK_INTERVAL);
    metric_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `interval` tick fires immediately; consume it so the baseline
    // frame above is not duplicated on the very next loop.
    metric_ticker.tick().await;
    let mut topology_ticker = tokio::time::interval(TOPOLOGY_POLL_INTERVAL);
    topology_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    topology_ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            // Session expired mid-connection: close the socket.
            _ = &mut expiry => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
            received = monitor_rx.recv() => {
                match received {
                    // Dedup at the source against the replayed snapshot: an update
                    // already covered by the snapshot's last_sequence is skipped
                    // (the client would whole-frame-dedup it anyway).
                    Ok(update) if update.sequence <= snapshot.last_sequence => {}
                    Ok(update) => {
                        let frames = frames_for_update(&update, &flow_store);
                        match send_frames(&frames, expiry.as_mut(), &mut socket).await {
                            SendOutcome::Completed => {}
                            SendOutcome::Expired => {
                                let _ = socket.send(Message::Close(None)).await;
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
                let seq = gateway.metrics().metrics_seq();
                if seq != last_metrics_seq {
                    last_metrics_seq = seq;
                    let frame = metric_tick_frame(&gateway.metrics().view(), seq);
                    match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut socket).await {
                        SendOutcome::Completed => {}
                        SendOutcome::Expired => {
                            let _ = socket.send(Message::Close(None)).await;
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
                    match send_frames(std::slice::from_ref(&frame), expiry.as_mut(), &mut socket).await {
                        SendOutcome::Completed => {}
                        SendOutcome::Expired => {
                            let _ = socket.send(Message::Close(None)).await;
                            return;
                        }
                        SendOutcome::Failed => return,
                    }
                }
            }
        }
    }
}

/// A sink for one dashboard frame. Abstracts the WS socket so the send/expiry
/// race ([`send_frames`]) is unit-testable with a mock sink — an `axum`
/// `WebSocket` can't be constructed off a real upgrade in a unit test.
trait FrameSink {
    /// Send one frame; `false` means the peer is gone (sending should stop).
    fn send_frame(&mut self, frame: &DashboardFrame) -> impl Future<Output = bool>;
}

impl FrameSink for WebSocket {
    fn send_frame(&mut self, frame: &DashboardFrame) -> impl Future<Output = bool> {
        send_one(self, frame)
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

/// Serialize + send one frame as a WS text message. A serialization failure is
/// treated as a no-op success (skip the frame) rather than tearing down the
/// socket, mirroring `/debug/ws`.
async fn send_one(socket: &mut WebSocket, frame: &DashboardFrame) -> bool {
    let Ok(payload) = serde_json::to_string(frame) else {
        return true;
    };
    socket.send(Message::Text(payload.into())).await.is_ok()
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
    /// keyed by `api_call_id`, so it FALLS BACK into the monitor batch rather than
    /// being dropped (no transcript data lost).
    #[test]
    fn unresolved_usage_status_fall_back_to_monitor_batch() {
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
            "both fall back to monitor, none dropped"
        );
    }

    /// With a live FlowStore record (linked `response_id → api_call_id`), a
    /// monitor `Usage` routes OUT to a flow-domain `usage` payload keyed by
    /// `api_call_id`, and `RequestStatus` to a flow-domain `flow_status`. The
    /// remaining messages stay in a monitor frame.
    #[test]
    fn usage_and_status_route_to_flow_domain_keyed_by_api_call_id() {
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
        // A flow frame (usage + status) and a monitor frame (the segment).
        assert_eq!(frames.len(), 2);
        let flow = frames
            .iter()
            .find(|f| f.domain == Domain::Flow)
            .expect("flow frame");
        assert_eq!(flow.batch.len(), 2, "usage + flow_status");
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
                assert_eq!(*status, FlowStatus::Completed);
            }
            other => panic!("expected flow_status payload, got {other:?}"),
        }
        let monitor = frames
            .iter()
            .find(|f| f.domain == Domain::Monitor)
            .expect("monitor frame");
        assert_eq!(
            monitor.batch.len(),
            1,
            "only the non-flow message stays in monitor"
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
}
