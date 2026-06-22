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

use crate::dashboard_flow::ClientSource;
use crate::dashboard_flow::FlowRecord;
use crate::dashboard_flow::FlowStatus;
use crate::dashboard_flow::FlowUsage;
use crate::dashboard_ws::MetricWindow;
use crate::dashboard_ws::MetricWindows;
use crate::dashboard_ws::MetricsSnapshot;
use crate::dashboard_ws::ModelPrice;
use crate::dashboard_ws::SeqCursors;
use crate::dashboard_ws::TopologyEdge;
use crate::dashboard_ws::TopologyNode;
use crate::dashboard_ws::TopologySnapshot;
use crate::engine::Gateway;
use crate::metrics::MetricsView;
use crate::metrics::StatusClass;
use crate::metrics::WindowReport;
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

/// Window lengths in SECONDS (the divisor for the per-window rate fields). Must
/// match the MetricsLayer ring spans (1m/5m/1h at 1 s resolution).
const WINDOW_1M_SECS: f64 = 60.0;
const WINDOW_5M_SECS: f64 = 300.0;
const WINDOW_1H_SECS: f64 = 3600.0;

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
#[derive(Debug, Clone, Serialize)]
pub struct FlowRow {
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
}

impl FlowRow {
    /// Build a row from a live [`FlowRecord`], pricing it via the gateway's price
    /// table keyed by the SERVED model (the backend that actually answered).
    fn from_record(record: &FlowRecord, gateway: &Gateway) -> Self {
        let cost = flow_cost(record.model_served.as_deref(), record.usage, gateway);
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
            // Gap 04: thread the attribution (label + source) onto the row — body-free
            // scalar metadata; the raw key is never here (only the one-way hash prefix).
            client_label: record.client_label.clone(),
            client_source: record.client_source,
            cost,
        }
    }

    /// Build a row from a body-free snapshot summary (the `/snapshot` summaries),
    /// pricing it the same way. The snapshot summary has no live `FlowRecord`, so
    /// this prices off its own `model_served` + `usage`.
    fn from_summary(
        summary: &crate::dashboard_flow::SnapshotFlowSummary,
        gateway: &Gateway,
    ) -> Self {
        let cost = flow_cost(summary.model_served.as_deref(), summary.usage, gateway);
        Self {
            api_call_id: summary.api_call_id.clone(),
            response_id: summary.response_id.clone(),
            method: summary.method.clone(),
            uri: summary.uri.clone(),
            model_requested: summary.model_requested.clone(),
            model_served: summary.model_served.clone(),
            upstream_target: summary.upstream_target.clone(),
            usage: summary.usage,
            status: summary.status,
            started_ms: summary.started_ms,
            finished_ms: summary.finished_ms,
            elapsed_ms: summary.elapsed_ms,
            terminal_reason: summary.terminal_reason.clone(),
            // Gap 04: same attribution projection from the body-free snapshot summary.
            client_label: summary.client_label.clone(),
            client_source: summary.client_source,
            cost,
        }
    }
}

/// `GET /dashboard/api/flows` — the paged flow list + total + the FlowStore
/// domain cursor. Matches the frozen `FlowsResponse`.
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
pub struct FlowDelta {
    pub sequence: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u128>,
}

/// `GET /dashboard/api/flows/:id` — the 3-pane inspector body. Carries the summary
/// fields, the three captured on-wire bodies (inbound, normalized, upstream —
/// ABSENT, not error, when the summary-byte quota evicted them), the inbound
/// headers, the replayed deltas, usage, the terminal, and cost. Mirrors the frozen
/// `FlowDetail` (`:id == api_call_id`). The three bodies, headers, and deltas are
/// the additive detail fields over a [`FlowRow`].
#[derive(Debug, Clone, Serialize)]
pub struct FlowDetailBody {
    pub flow_seq: u64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_requested: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_served: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_target: Option<String>,
    pub usage: Option<FlowUsage>,
    pub status: FlowStatus,
    pub deltas: Vec<FlowDelta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    pub started_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
    pub cost: Option<f64>,
}

/// One catalog entry (`GET /dashboard/api/catalog` — a BARE array, no cursor).
/// Mirrors the frozen `CatalogEntry`: `{id, context_limit}` (a non-null count; an
/// upstream that reports no window collapses to `0`).
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    pub id: String,
    pub context_limit: i64,
}

/// `GET /dashboard/api/snapshot?at=<unix_ms>` — a body-free frozen cut. Mirrors
/// the frozen `SnapshotResponse`: the per-domain `cursors`, the cut instant, the
/// body-free flow summaries (priced), and the metrics/topology cuts reshaped into
/// their REST bodies (`null` when the cut is empty for that domain).
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotResponse {
    pub cursors: SeqCursors,
    pub at_ms: u128,
    pub summaries: Vec<FlowRow>,
    pub metrics: Option<MetricsSnapshot>,
    pub topology: Option<TopologySnapshot>,
}

/// Query param for `GET /dashboard/api/snapshot` — the wall-clock instant (unix
/// ms) to time-travel to. Absent ⇒ the latest cut. Typed `u64` (NOT `u128`): the
/// axum/serde QUERY deserializer does not support `u128`, and unix-ms fits `u64`
/// for ~580 million years; the handler widens it to the `u128` `snapshot_at` key.
#[derive(Debug, Default, Deserialize)]
pub struct SnapshotQuery {
    pub at: Option<u64>,
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
    let cached = usage.cached.max(0) as f64;
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

/// A JSON-safe float: the value if finite, else `0.0`. `serde_json` REFUSES to
/// serialize NaN/±∞ (it errors), so every float that reaches a response body — cost
/// roll-ups, per-second rates — is passed through this so a degenerate input can
/// never turn a read into a 500. (The inputs are operator-configured prices, not
/// attacker data, but a typo'd 1e308 rate should degrade gracefully, not 500.)
fn finite(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

/// Price a flow: `Some(cost)` when BOTH a served model and usage are present AND a
/// price is configured for that model; `None` otherwise (the row shows `cost:null`).
fn flow_cost(
    model_served: Option<&str>,
    usage: Option<FlowUsage>,
    gateway: &Gateway,
) -> Option<f64> {
    let model = model_served?;
    let usage = usage?;
    let price = gateway.price_for(model)?;
    Some(cost_for_usage(usage, price))
}

/// The total token throughput of one window (prompt + completion + cached +
/// reasoning across every bucket) — the numerator for `tokens_per_sec`.
fn window_total_tokens(report: &WindowReport) -> i64 {
    report
        .buckets
        .values()
        .map(|counts| {
            counts
                .prompt_tokens
                .saturating_add(counts.completion_tokens)
                .saturating_add(counts.cached_tokens)
                .saturating_add(counts.reasoning_tokens)
        })
        .fold(0i64, i64::saturating_add)
}

/// The total USD cost of one window: every bucket's tokens priced by its OWN
/// served model (`BucketKey.model`). Buckets whose model has no configured price
/// contribute nothing. The basis for `cost_per_min` (this ÷ window minutes).
fn window_total_cost(report: &WindowReport, prices: &HashMap<String, ModelPrice>) -> f64 {
    report
        .buckets
        .iter()
        .filter_map(|(key, counts)| {
            price_lookup(prices, &key.model).map(|price| {
                cost_for_usage(
                    FlowUsage {
                        prompt: counts.prompt_tokens,
                        completion: counts.completion_tokens,
                        cached: counts.cached_tokens,
                        reasoning: counts.reasoning_tokens,
                        total: 0,
                    },
                    price,
                )
            })
        })
        .sum()
}

/// Exact-then-case-insensitive price lookup over a raw price map, mirroring
/// [`crate::config::Config::price_for`] (used where only the map is in hand, e.g.
/// pricing a snapshot cut's metrics buckets).
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
fn rest_window_tile(
    report: &WindowReport,
    window_secs: f64,
    active_streams: u64,
    prices: &HashMap<String, ModelPrice>,
) -> MetricWindow {
    let percentiles = report.percentiles();
    let total = report.total_count();
    let errors: u64 = report
        .buckets
        .iter()
        .filter(|(key, _)| key.status == StatusClass::Error)
        .map(|(_, counts)| counts.count)
        .fold(0u64, u64::saturating_add);
    let error_pct = if total > 0 {
        (errors as f64) / (total as f64) * 100.0
    } else {
        0.0
    };
    let reqs_per_sec = total as f64 / window_secs;
    let tokens_per_sec = window_total_tokens(report) as f64 / window_secs;
    let cost_per_min = window_total_cost(report, prices) / (window_secs / 60.0);
    // Per-metric measurability denominators (gap 01 review round 1, finding 3): token
    // and cost availability are SEPARATE from latency/error. `usage_samples` counts
    // terminal flows that reported usage; `priced_samples` the subset whose served
    // model has a configured price (derived HERE, where the price table lives, so
    // `metrics.rs` stays price-agnostic). A window can have `samples > 0` (latency
    // measured) yet `usage_samples == 0` (no flow reported tokens) → `tokens_per_sec`
    // renders `—`; or `usage_samples > 0` yet `priced_samples == 0` (only unpriced
    // models) → `cost_per_min` renders `—`, distinguishing "unpriced" from `$0.00`.
    let usage_samples = report.usage_sample_count();
    let priced_samples = report.priced_sample_count(|model| price_lookup(prices, model).is_some());
    // Every float is `finite`-guarded: a non-finite value would make
    // `serde_json::to_vec` error and 500 the `/metrics` read.
    MetricWindow {
        reqs_per_sec: finite(reqs_per_sec),
        active_streams,
        error_pct: finite(error_pct),
        p50: finite(percentiles.p50),
        p95: finite(percentiles.p95),
        p99: finite(percentiles.p99),
        tokens_per_sec: finite(tokens_per_sec),
        cost_per_min: finite(cost_per_min),
        // `total` is the count of TERMINAL flows in the window — the latency/error
        // measured/unavailable signal. `0` here ≠ "zero throughput"; it means NO
        // finalized flow fed the latency/error fields, so the frontend renders those
        // `—` (while `reqs_per_sec`'s genuine `0` stays a `0`). Token/cost availability
        // use the separate denominators above.
        samples: total,
        usage_samples,
        priced_samples,
    }
}

/// Build the full `/metrics`-shaped [`MetricsSnapshot`] body from a collapsed
/// [`MetricsView`] (+ its `metrics_seq`), the live open-flow count, and the price
/// table. The headline tile repeats the `m1` window (the dashboard's headline) and
/// nests all three windows under `windows`. Shared by the live `/metrics` read AND
/// the `/snapshot` metrics reshape so both emit byte-identical shapes.
pub fn metrics_body(
    view: &MetricsView,
    metrics_seq: u64,
    active_streams: u64,
    prices: &HashMap<String, ModelPrice>,
) -> MetricsSnapshot {
    let m1 = rest_window_tile(&view.window_1m, WINDOW_1M_SECS, active_streams, prices);
    let m5 = rest_window_tile(&view.window_5m, WINDOW_5M_SECS, active_streams, prices);
    let h1 = rest_window_tile(&view.window_1h, WINDOW_1H_SECS, active_streams, prices);
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
        samples: m1.samples,
        usage_samples: m1.usage_samples,
        priced_samples: m1.priced_samples,
        windows: MetricWindows { m1, m5, h1 },
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
) -> TopologySnapshot {
    let nodes: Vec<TopologyNode> = snapshot
        .providers
        .iter()
        .map(TopologyNode::from_health)
        .collect();
    let edges: Vec<TopologyEdge> = snapshot
        .providers
        .iter()
        .map(|provider| {
            let (reqs, tokens, cost) = upstream_edge_rates(&provider.id, window_1m, prices);
            TopologyEdge {
                from: "gateway".to_string(),
                to: provider.id.clone(),
                throughput: reqs,
                tokens_per_sec: tokens,
                cost_per_sec: cost,
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
    prices: &HashMap<String, ModelPrice>,
) -> (f64, f64, f64) {
    let mut reqs = 0u64;
    let mut tokens = 0i64;
    let mut cost = 0.0f64;
    for (key, counts) in &window_1m.buckets {
        if key.upstream != upstream_id {
            continue;
        }
        reqs = reqs.saturating_add(counts.count);
        tokens = tokens
            .saturating_add(counts.prompt_tokens)
            .saturating_add(counts.completion_tokens)
            .saturating_add(counts.cached_tokens)
            .saturating_add(counts.reasoning_tokens);
        if let Some(price) = price_lookup(prices, &key.model) {
            cost += cost_for_usage(
                FlowUsage {
                    prompt: counts.prompt_tokens,
                    completion: counts.completion_tokens,
                    cached: counts.cached_tokens,
                    reasoning: counts.reasoning_tokens,
                    total: 0,
                },
                price,
            );
        }
    }
    (
        finite(reqs as f64 / WINDOW_1M_SECS),
        finite(tokens as f64 / WINDOW_1M_SECS),
        finite(cost / WINDOW_1M_SECS),
    )
}

/// Count the flows currently OPEN (live streams) in the FlowStore — the
/// `active_streams` tile value (the metrics rings count terminals, not liveness).
/// `pub(crate)` so the live `/dashboard/ws` tick + initial snapshot derive the SAME
/// open-flow count as the REST `/metrics` read (gap 01 — one source, no drift).
pub(crate) fn active_stream_count(gateway: &Gateway) -> u64 {
    gateway
        .flow_store()
        .list()
        .iter()
        .filter(|record| record.status == FlowStatus::Open)
        .count() as u64
}

/// Count the OPEN flows in a FROZEN snapshot cut's body-free summaries — the
/// `active_streams` value for a historical `/snapshot?at=` (D13 R1 HIGH). Reading the
/// live FlowStore for a time-travel cut would report NOW's open count, not the cut's;
/// the summaries are the cut's own consistent flow projection, so counting their open
/// status keeps the whole snapshot frozen to one instant.
fn cut_active_stream_count(summaries: &[crate::dashboard_flow::SnapshotFlowSummary]) -> u64 {
    summaries
        .iter()
        .filter(|summary| summary.status == FlowStatus::Open)
        .count() as u64
}

// ---------------------------------------------------------------------------
// Delta replay (MonitorHub snapshot, filtered by response_id)
// ---------------------------------------------------------------------------

/// Replay the streamed deltas for a flow from the MonitorHub snapshot, filtered by
/// the flow's `response_id` (the monitor keys transcript messages by the engine's
/// response id, NOT the `api_call_id`). Returns an empty `Vec` when the flow has no
/// linked `response_id` yet (nothing to correlate). Each matching `SegmentAppend`/
/// `EventAppend`/`RequestStatus` becomes a [`FlowDelta`] in monitor order, with a
/// per-flow `sequence` ordinal. `RequestUpsert`/`Usage`/`RequestRemove`/`Hello`/
/// `SnapshotDone` are not per-token deltas (the row already carries usage/status),
/// so they are skipped — the inspector wants the segment/event timeline.
fn replay_deltas(response_id: Option<&str>, gateway: &Gateway) -> Vec<FlowDelta> {
    let Some(response_id) = response_id else {
        return Vec::new();
    };
    let snapshot = gateway.debug_snapshot();
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
    deltas
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
    let flow_seq = gateway.flow_store().flow_seq();
    let status_filter = query.status.as_deref().and_then(parse_status_filter);
    let model_filter = query.model.as_deref().map(str::to_ascii_lowercase);
    let upstream_filter = query.upstream.as_deref().map(str::to_ascii_lowercase);

    let rows: Vec<FlowRow> = gateway
        .flow_store()
        .list()
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
) -> Response {
    // Capture the record AND its own mutation watermark in one lock hold so the
    // detail's `flow_seq` is the record's own cursor (D7b R1 finding 3), not a
    // later global value bumped by unrelated flows.
    let Some((record, flow_seq)) = gateway.flow_store().detail_with_seq(&id) else {
        return json_no_store(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "no flow for that id" }),
        );
    };
    let cost = flow_cost(
        record.model_served.as_deref(),
        record.usage,
        gateway.as_ref(),
    );
    let deltas = replay_deltas(record.response_id.as_deref(), gateway.as_ref());
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
    let body = FlowDetailBody {
        flow_seq,
        api_call_id: record.api_call_id.clone(),
        response_id: record.response_id.clone(),
        inbound_body: record.inbound_body.as_ref().map(parse_captured_body),
        inbound_headers,
        normalized: record.normalized.as_ref().map(parse_captured_body),
        upstream_body: record.upstream_body.as_ref().map(parse_captured_body),
        model_requested: record.model_requested.clone(),
        model_served: record.model_served.clone(),
        upstream_target: record.upstream_target.clone(),
        usage: record.usage,
        status: record.status,
        deltas,
        terminal_reason: record.terminal_reason.clone(),
        started_ms: record.started_ms,
        finished_ms: record.finished_ms,
        elapsed_ms: record.elapsed_ms,
        cost,
    };
    json_no_store(StatusCode::OK, &body)
}

/// `GET /dashboard/api/metrics` — the live stats tiles (D5 view) + the metrics
/// domain `metrics_seq` + the live open-flow `active_streams` count + the priced
/// `cost_per_min`. Per-window TRUE per-second rates (D13 divides by the window
/// seconds). The view + its cursor are captured in ONE metrics-lock hold so the
/// body and `metrics_seq` are consistent.
pub async fn dashboard_metrics(State(gateway): State<Arc<Gateway>>) -> Response {
    let (view, metrics_seq) = gateway.metrics().view_with_seq();
    let active = active_stream_count(gateway.as_ref());
    let body = metrics_body(&view, metrics_seq, active, gateway.price_table());
    json_no_store(StatusCode::OK, &body)
}

/// `GET /dashboard/api/topology` — the provider topology (D4 nodes + edges) + the
/// price table + the topology domain `topology_seq`. Edges carry per-upstream
/// per-second request/token/cost rates rolled up from the live `m1` metrics window.
pub async fn dashboard_topology(State(gateway): State<Arc<Gateway>>) -> Response {
    let snapshot = gateway.provider_health_publisher().latest();
    let view = gateway.metrics().view();
    let body = topology_body(&snapshot, gateway.price_table(), &view.window_1m);
    json_no_store(StatusCode::OK, &body)
}

/// `GET /dashboard/api/catalog` — the model catalog as a BARE array `[{id,
/// context_limit}]` (no cursor; a static-ish read). Sourced from the upstream
/// `/v1/models` snapshot via the `UpstreamClient` (ids + per-model context
/// window); an upstream that reports no window collapses `context_limit` to `0`.
/// An upstream catalog-fetch failure yields an empty array (the dashboard simply
/// shows no catalog) rather than a 5xx that would blank the whole view.
pub async fn dashboard_catalog(State(gateway): State<Arc<Gateway>>) -> Response {
    let entries = match gateway.upstream_client().supported_model_catalog().await {
        Ok(catalog) => catalog
            .into_iter()
            .map(|entry| CatalogEntry {
                id: entry.id,
                context_limit: entry.context_limit.unwrap_or(0),
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
    let cut = match at_query {
        Some(at) => gateway.metrics().snapshot_at(at),
        None => gateway.metrics().latest_snapshot(),
    };
    let Some(cut) = cut else {
        // No cut yet (the 5 s task has not run, or every cut is newer than `at`):
        // a contract-valid empty snapshot, not a 404.
        return json_no_store(
            StatusCode::OK,
            &SnapshotResponse {
                cursors: SeqCursors::default(),
                at_ms: at_query.unwrap_or(0),
                summaries: Vec::new(),
                metrics: None,
                topology: None,
            },
        );
    };

    let prices = gateway.price_table();
    let summaries: Vec<FlowRow> = cut
        .summaries
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
    let active = cut_active_stream_count(&cut.summaries);
    let metrics = Some(metrics_body(
        &cut.metrics,
        cut.cursors.metrics_seq,
        active,
        prices,
    ));
    let topology = Some(topology_body(&cut.topology, prices, &cut.metrics.window_1m));
    json_no_store(
        StatusCode::OK,
        &SnapshotResponse {
            cursors: SeqCursors {
                flow_seq: cut.cursors.flow_seq,
                metrics_seq: cut.cursors.metrics_seq,
                topology_seq: cut.cursors.topology_seq,
                monitor_seq: cut.cursors.monitor_seq,
            },
            at_ms: cut.taken_at_ms,
            summaries,
            metrics,
            topology,
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

    fn price(input: f64, output: f64, cached: f64) -> ModelPrice {
        ModelPrice {
            input_per_1k: input,
            output_per_1k: output,
            cached_per_1k: cached,
        }
    }

    fn usage(prompt: i64, completion: i64, cached: i64) -> FlowUsage {
        FlowUsage {
            prompt,
            completion,
            cached,
            reasoning: 0,
            total: prompt + completion,
        }
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
    /// charge — the uncached prompt floors at 0, so the whole prompt bills at the
    /// (cheaper) cache rate rather than producing a negative number.
    #[test]
    fn cost_for_usage_clamps_cached_over_prompt() {
        let cost = cost_for_usage(usage(10, 0, 50), price(2.0, 6.0, 0.5));
        // uncached = max(10 - 50, 0) = 0; cached billed = 50/1000*0.5 = 0.025.
        assert!(
            (cost - 0.025).abs() < 1e-9,
            "cost {cost} == 0.025 (no negative)"
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

    /// Gap 01 (the honest strip): a window fed by a real TERMINAL flow reports a
    /// non-zero `samples` count AND real `tokens_per_sec`/`cost_per_min` (priced) +
    /// the passed-in live `active_streams` — NOT the hard-coded zeros the live WS
    /// tile used to ship. This is the "a live-flow is counted in the strip" proof,
    /// exercised through the SAME `metrics_body` builder the live tick now uses.
    #[test]
    fn metrics_body_counts_a_terminal_flow_with_real_rates() {
        use crate::dashboard_flow::FlowStatus as FS;
        use crate::metrics::MetricsLayer;
        let metrics = MetricsLayer::new();
        // One completed flow on a priced model: 1000 prompt + 500 completion tokens.
        metrics.record_terminal(
            FS::Completed,
            Some("glm-5.1"),
            "/v1/responses",
            Some("vllm-a"),
            1200,
            Some(usage(1000, 500, 0)),
        );
        let (view, seq) = metrics.view_with_seq();
        let mut prices = HashMap::new();
        prices.insert("glm-5.1".to_string(), price(2.0, 6.0, 0.5));
        // Live open-flow count threaded in (3 streams currently in flight).
        let body = metrics_body(&view, seq, 3, &prices);

        // The terminal flow IS counted — the measured/unavailable signal is non-zero.
        assert_eq!(body.samples, 1, "the finalized flow counts as one sample");
        assert_eq!(body.windows.m1.samples, 1);
        // The flow reported usage on a PRICED model → both per-metric denominators are
        // non-zero (gap 01 finding 3): tok/s and $/min are both measurable here.
        assert_eq!(
            body.usage_samples, 1,
            "the usage-bearing flow is a usage sample"
        );
        assert_eq!(body.windows.m1.usage_samples, 1);
        assert_eq!(body.priced_samples, 1, "priced model → a priced sample");
        assert_eq!(body.windows.m1.priced_samples, 1);
        // active_streams carries the live count (was hard-coded 0 on the WS tile).
        assert_eq!(body.active_streams, 3, "live open-flow count is carried");
        // tokens/s + cost/min are REAL (priced), not 0.0. 1500 tok / 60 s = 25 tok/s.
        assert!(
            (body.tokens_per_sec - 25.0).abs() < 1e-9,
            "tok/s {} == 1500/60",
            body.tokens_per_sec
        );
        // cost = 1000 prompt @2.0/1k + 500 completion @6.0/1k = 2.0 + 3.0 = 5.0 over
        // 1 minute → cost_per_min ≈ 5.0.
        assert!(
            (body.cost_per_min - 5.0).abs() < 1e-9,
            "cost/min {} == 5.0",
            body.cost_per_min
        );
        // req/s is a TRUE per-second rate (1 req / 60 s), not the raw count.
        assert!(
            (body.reqs_per_sec - (1.0 / 60.0)).abs() < 1e-9,
            "req/s {} == 1/60",
            body.reqs_per_sec
        );
    }

    /// Gap 01 (don't lie with zeros): an EMPTY window (no finalized flow) reports
    /// `samples == 0` — the signal the frontend reads to render latency/tok-s/cost as
    /// `unavailable` (`—`) — while `reqs_per_sec` stays a genuine measured `0` (legit
    /// zero traffic). The two cases are thus distinguishable on the wire.
    #[test]
    fn metrics_body_empty_window_reports_zero_samples_not_a_fake_zero() {
        use crate::metrics::MetricsView;
        let body = metrics_body(&MetricsView::default(), 0, 0, &HashMap::new());
        assert_eq!(
            body.samples, 0,
            "no finalized flow → zero samples (unavailable)"
        );
        assert_eq!(body.windows.m1.samples, 0);
        assert_eq!(body.windows.m5.samples, 0);
        assert_eq!(body.windows.h1.samples, 0);
        // The per-metric denominators are zero too → tok/s + $/min are unavailable.
        assert_eq!(body.usage_samples, 0);
        assert_eq!(body.priced_samples, 0);
        assert_eq!(body.windows.m1.usage_samples, 0);
        assert_eq!(body.windows.m1.priced_samples, 0);
        // req/s is a genuine measured zero (idle), distinguishable from the unavailable
        // latency/tok-s/cost above precisely BECAUSE samples == 0.
        assert_eq!(body.reqs_per_sec, 0.0);
        assert_eq!(body.tokens_per_sec, 0.0);
        assert_eq!(body.cost_per_min, 0.0);
    }

    /// Gap 01 finding 3 (per-metric availability): a window can have measured LATENCY
    /// (`samples > 0`) yet UNMEASURABLE tokens/cost. Two terminal flows finalize — one
    /// with usage on a PRICED model, one with NO usage at all and one with usage on an
    /// UNPRICED model — so `samples` (latency) and the two token/cost denominators
    /// diverge. The frontend reads each denominator independently to decide `—` vs a
    /// number, so this asserts they are emitted independently and correctly.
    #[test]
    fn metrics_body_per_metric_denominators_diverge() {
        use crate::dashboard_flow::FlowStatus as FS;
        use crate::metrics::MetricsLayer;
        let metrics = MetricsLayer::new();
        // (a) usage on a PRICED model → counts toward samples + usage + priced.
        metrics.record_terminal(
            FS::Completed,
            Some("glm-5.1"),
            "/v1/responses",
            Some("vllm-a"),
            900,
            Some(usage(1000, 500, 0)),
        );
        // (b) NO usage (e.g. an upstream that omitted it) → samples only.
        metrics.record_terminal(
            FS::Completed,
            Some("glm-5.1"),
            "/v1/responses",
            Some("vllm-a"),
            900,
            None,
        );
        // (c) usage on an UNPRICED model → samples + usage, but NOT priced.
        metrics.record_terminal(
            FS::Completed,
            Some("free-model"),
            "/v1/responses",
            Some("vllm-a"),
            900,
            Some(usage(10, 5, 0)),
        );
        let (view, seq) = metrics.view_with_seq();
        let mut prices = HashMap::new();
        prices.insert("glm-5.1".to_string(), price(2.0, 6.0, 0.5));
        let body = metrics_body(&view, seq, 0, &prices);

        // Latency is measurable for all three finalized flows.
        assert_eq!(
            body.samples, 3,
            "three finalized flows → latency measurable"
        );
        // Two of the three reported usage → tok/s measurable, but distinct from samples.
        assert_eq!(
            body.usage_samples, 2,
            "two usage-bearing flows → tok/s measurable (≠ samples)"
        );
        // Only one of those two is on a priced model → cost measurable for exactly one.
        assert_eq!(
            body.priced_samples, 1,
            "only the priced-model usage flow → $/min measurable (≠ usage_samples)"
        );
        // The headline mirrors the m1 window's per-metric denominators.
        assert_eq!(body.windows.m1.samples, 3);
        assert_eq!(body.windows.m1.usage_samples, 2);
        assert_eq!(body.windows.m1.priced_samples, 1);
    }

    /// Round-trip (AGENTS.md: no new wire fields without a round-trip test): the new
    /// `usage_samples`/`priced_samples` wire fields survive a serialize → JSON → re-parse
    /// cycle at BOTH the headline and the per-window level, with the exact values the
    /// `metrics_body` builder produced. This pins the byte contract the frozen frontend
    /// validators (`isMetricWindow`/`isMetricsResponse`) decode.
    #[test]
    fn metrics_body_new_sample_fields_round_trip_through_json() {
        use crate::dashboard_flow::FlowStatus as FS;
        use crate::metrics::MetricsLayer;
        let metrics = MetricsLayer::new();
        metrics.record_terminal(
            FS::Completed,
            Some("glm-5.1"),
            "/v1/responses",
            Some("vllm-a"),
            900,
            Some(usage(1000, 500, 0)),
        );
        let (view, seq) = metrics.view_with_seq();
        let mut prices = HashMap::new();
        prices.insert("glm-5.1".to_string(), price(2.0, 6.0, 0.5));
        let body = metrics_body(&view, seq, 2, &prices);

        // Serialize → JSON bytes → re-parse: the fields must survive intact.
        let json = serde_json::to_string(&body).expect("serialize metrics body");
        let value: serde_json::Value = serde_json::from_str(&json).expect("re-parse");
        // Headline mirrors.
        assert_eq!(value["usage_samples"], serde_json::json!(1));
        assert_eq!(value["priced_samples"], serde_json::json!(1));
        // Per-window (m1 fed the terminal; m5/h1 share the same epoch ⇒ same counts).
        for window in ["m1", "m5", "h1"] {
            assert_eq!(value["windows"][window]["samples"], serde_json::json!(1));
            assert_eq!(
                value["windows"][window]["usage_samples"],
                serde_json::json!(1)
            );
            assert_eq!(
                value["windows"][window]["priced_samples"],
                serde_json::json!(1)
            );
        }
    }

    /// 1-based paging: page 2 with limit 2 over 5 rows yields rows 3..=4; a limit
    /// of 0 (or absent) returns all rows; an out-of-range page yields empty.
    #[test]
    fn apply_paging_pages_1_based() {
        let rows = |n: usize| -> Vec<FlowRow> {
            (0..n)
                .map(|i| FlowRow {
                    api_call_id: format!("api_{i}"),
                    response_id: None,
                    method: "POST".to_string(),
                    uri: "/v1/responses".to_string(),
                    model_requested: None,
                    model_served: None,
                    upstream_target: None,
                    usage: None,
                    status: FlowStatus::Completed,
                    started_ms: 0,
                    finished_ms: None,
                    elapsed_ms: None,
                    terminal_reason: None,
                    client_label: None,
                    client_source: None,
                    cost: None,
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
            api_call_id: "api_x".to_string(),
            response_id: None,
            method: "POST".to_string(),
            uri: "/v1/responses".to_string(),
            model_requested: None,
            model_served: None,
            upstream_target: None,
            usage: None,
            status: FlowStatus::Open,
            started_ms: 0,
            finished_ms: None,
            elapsed_ms: None,
            terminal_reason: None,
            client_label: None,
            client_source: None,
            cost: None,
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
}
