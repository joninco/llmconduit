/* eslint-disable */
/** Generated from Rust dashboard DTOs. Do not edit by hand. */

/**
 * Gap 03 — a BOUNDED, sanitized, taxonomic failure code for a failed attempt. This is
 * NOT raw upstream error text (which stays behind spec 05's separately-gated seam): it
 * is a fixed enum so the body-free summary can never become a backdoor for an
 * unbounded/secret-bearing upstream error body. `error_class` is `None` on the served
 * attempt (don't-lie-with-zeros for the success case). Serializes snake_case.
 */
export type AttemptErrorClass = "connect" | "http_status" | "timeout" | "stream" | "terminal" | "other";
/**
 * Gap 03 — a BOUNDED, sanitized, taxonomic reason a failed attempt triggered failover
 * to the next provider. Like [`AttemptErrorClass`], this is a fixed enum — never raw
 * upstream text — so it is safe on the body-free summary. `None` on the served attempt
 * (it did not fail over). Serializes snake_case.
 */
export type AttemptFailoverReason = "provider_failed" | "request_rejected" | "terminal_no_failover";
export type FlowDetailSource = "live" | "durable";
/**
 * Lifecycle status of a flow. `Open` at creation; D3 moves it to a terminal
 * state. Serializes snake_case for the dashboard REST/WS surface.
 */
export type FlowStatus = "open" | "completed" | "failed" | "cancelled";
/**
 * Gap 04 — the PROVENANCE of a flow's `client_label`: WHICH non-secret signal the
 * attribution was derived from. Tagged so the dashboard (spec 15) can render the
 * weaker User-Agent fallback DIFFERENTLY from the stronger key-hash / configured-id
 * attribution — a `user_agent`-sourced label is NOT an identity claim. Serializes
 * snake_case; `Deserialize` so the body-free [`SnapshotFlowSummary`] round-trips on
 * the WS/snapshot wire (AGENTS.md: no new wire field without a round-trip test).
 *
 * Priority order (strongest → weakest), honored by [`ClientAttribution::derive`]:
 * `KeyHash` → `ConfiguredHeader` → `UserAgent`. There is NO proxy auth-principal
 * source today (the proxy forwards keys, it does not authenticate a principal), so
 * one is deliberately absent until such a seam exists (spec 04 / Codex review).
 */
export type ClientSource = "key_hash" | "configured_header" | "user_agent";
/**
 * Confidence attached to a terminal-time price. This lives with the evict-safe
 * terminal payload (rather than the REST projection) so historical overview cuts keep
 * the rate table decision that was true when the request finished.
 */
export type TerminalCostConfidence = "confident" | "estimated" | "unavailable";
/**
 * Data quality for one instantaneous metric value. Percentiles are still emitted for
 * sparse intervals; `Partial` tells consumers that the nearest-rank estimate has fewer
 * than the recommended number of observations.
 */
export type InstantMetricQuality = "measured" | "partial" | "unavailable";
export type QuantileMethod = "log_histogram_nearest_rank";
/**
 * Cross-cutting quality tag for Overview values. `partial` is explicit whenever a
 * bounded slot/union folded dimensions into `__other__`; missing source facts are
 * `unavailable`, never fabricated zeroes.
 */
export type OverviewDataQuality = "measured" | "derived" | "partial" | "unavailable";
export type OverviewMetricScope = "global";
/**
 * Exact server-side Overview window. Kept to the three MetricsLayer ring spans so a
 * live and historical request always selects the same retained population.
 */
export type OverviewWindow = "m1" | "m5" | "h1";
export type DebugWsMessage =
  | {
      history_limit: number;
      history_retention_ms: number;
      protocol_version: number;
      type: "hello";
    }
  | {
      request: DebugRequest;
      type: "request_upsert";
    }
  | {
      response_id: string;
      segment: DebugSegment;
      type: "segment_append";
    }
  | {
      event: DebugTimelineEvent;
      response_id: string;
      type: "event_append";
    }
  | {
      completed_at_ms: number | null;
      error: string | null;
      response_id: string;
      status: DebugRequestStatus;
      type: "request_status";
    }
  | {
      reason: string;
      response_id: string;
      type: "request_remove";
    }
  | {
      cached: number;
      completion: number;
      prompt: number;
      reasoning: number;
      response_id: string;
      total: number;
      type: "usage";
    }
  | {
      type: "snapshot_done";
    };
export type DebugRequestStatus = "running" | "completed" | "failed";
export type DebugSegmentKind = "output" | "reasoning" | "tool";
export type BackendMetricsCoverage = "full" | "partial";
export type BackendEngineKind = "vllm" | "sglang" | "unknown";
export type BackendMetricsStatus = "warming" | "fresh" | "stale" | "unsupported" | "error";
/**
 * Per-provider serving status for the topology map (D4). `Cooling` while inside
 * the failure cooldown window; `Down` once a cooling provider has also crossed
 * [`DOWN_THRESHOLD`] consecutive failures; `Healthy` otherwise.
 */
export type ProviderStatus = "healthy" | "cooling" | "down";
/**
 * One dashboard payload. Internally `type`-tagged (snake_case) to match the
 * frozen contract. The `Monitor` arm NESTS the real (itself-tagged)
 * [`DebugWsMessage`] under `message` — it is NOT flattened (both carry `type`).
 * The `usage`/`flow_status` arms are keyed by `api_call_id` (authoritative) with
 * an optional secondary `response_id`.
 */
export type DashboardPayload =
  | {
      message: DebugWsMessage;
      type: "monitor";
    }
  | {
      api_call_id: string;
      /**
       * Cache-read prompt tokens; `Some(n)` measured (incl. a reported `0`), `None`
       * (absent on the wire) ⇒ the upstream did not report a cached breakdown.
       */
      cached?: number | null;
      completion: number;
      prompt: number;
      /**
       * Reasoning tokens; `Some(n)` measured (incl. a reported `0`), `None` (absent on
       * the wire) ⇒ the upstream did not report reasoning details.
       */
      reasoning?: number | null;
      response_id?: string | null;
      total: number;
      type: "usage";
    }
  | MetricTick
  | {
      api_call_id: string;
      /**
       * Gap 10b — the gap-03 per-attempt failover trace projected onto the row (spec 11's
       * stepper reads the whole list; spec 10 reads the served attempt's
       * `first_upstream_byte_ms`). Each [`Attempt`] is body-free scalar provenance + bounded
       * taxonomic codes — never a raw upstream error body. `skip_serializing_if =
       * Vec::is_empty` so a flow with no recorded attempt OMITS the key (the frontend's
       * `attempts?` is absent), matching the body-free summary's wire shape.
       */
      attempts?: Attempt[];
      cache_price_impact_usd?: number | null;
      /**
       * Gap 04 — the STABLE, NON-SECRET client attribution label (key-hash `key-<hex>`
       * display id / configured caller-id / User-Agent fallback), projected from the
       * flow record/summary. `skip_serializing_if` so an unattributed flow OMITS the key
       * (absent ⇒ renders `—`, never a fabricated id). Additive/optional: the frontend
       * ignores it until the client-attribution UI (gap 15). The raw key is never here —
       * only the one-way hash prefix ever existed as a label.
       */
      client_label?: string | null;
      /**
       * Gap 04 — the [`ClientSource`] the label was derived from (so the weak
       * `user_agent` fallback is distinguishable from a key-hash / configured-id). `None`
       * (absent) exactly when `client_label` is `None`.
       */
      client_source?: ClientSource | null;
      /**
       * USD cost of the flow (usage × the served model's [`ModelPrice`]). `null`
       * when no price is configured for `model_served` — never a fabricated zero.
       */
      cost: number | null;
      /**
       * Gap 07 — the [`CostConfidence`] of `cost`: `confident` (priced + every billed
       * class has a known rate), `estimated` (a class falls back to the default `0.0`
       * cached rate / cached unreported), or `unavailable` (unpriced ⇒ `cost: null`).
       * Always present so the frontend can label an `estimated` figure as such and
       * distinguish an `unavailable` cost from a measured `$0.00`.
       */
      cost_confidence: "confident" | "estimated" | "unavailable";
      effective_route_limit?: number | null;
      elapsed_ms?: number | null;
      /**
       * Terminal finalize — stamped when the flow reaches its terminal state
       * (`finalize`), for EVERY terminal (completed, failed, cancelled). Always `Some`
       * once the flow is terminal; the right edge of the waterfall.
       */
      finalize_ms?: number | null;
      finalize_offset_ms?: number | null;
      finished_ms?: number | null;
      /**
       * True TTFT — the wall-clock instant the FIRST canonical **content** SSE delta was
       * emitted to the client. NOT reasoning, tool-argument, refusal, or signature
       * deltas: a stream that emits reasoning/tool deltas before content does NOT stamp
       * this early (first-write-wins on the content arm only). `None` if the flow errored
       * before any content delta.
       */
      first_content_delta_ms?: number | null;
      first_content_delta_offset_ms?: number | null;
      /**
       * Gap 10b — the gap-03 flow-level wire time-to-first-byte (the served attempt's first
       * on-wire chunk). Distinct from `first_content_delta_ms` (the first content delta to
       * the CLIENT). `None` ⇒ absent ⇒ renders `—` downstream, NEVER `0`.
       */
      first_upstream_byte_ms?: number | null;
      /**
       * Request ingress — when the FlowStore first `open`ed the record (≈ `started_ms`).
       * Always `Some` once a record exists; the explicit phase value the waterfall
       * anchors the other phases against.
       */
      ingress_ms?: number | null;
      /**
       * Monotonic offset from flow ingress. Present on newly captured records; legacy
       * snapshots without offsets continue to use ordered epoch timestamps as fallback.
       */
      ingress_offset_ms?: number | null;
      method: string;
      model_requested?: string | null;
      model_served?: string | null;
      /**
       * Inbound→canonical normalization settled — stamped when the engine captures the
       * normalized canonical body (`set_normalized`). `None` if the flow errored before
       * normalization (an extractor/JSON rejection caught by the L0 guard).
       */
      normalization_done_ms?: number | null;
      normalization_done_offset_ms?: number | null;
      /**
       * Calculation-only corrected usage; raw provider values remain in `usage`.
       */
      normalized_usage: FlowUsage | null;
      phase: FlowMutationPhase;
      response_id?: string | null;
      /**
       * Per-flow optimistic-concurrency version from the authoritative FlowStore.
       */
      revision: number;
      /**
       * Upstream routing/lowering decision — stamped when the engine commits the actual
       * on-wire upstream request (`set_upstream` at the leaf). `None` if the flow never
       * reached the wire (pre-spawn lowering/budget failure, replay-only).
       */
      routing_decision_ms?: number | null;
      routing_decision_offset_ms?: number | null;
      started_ms: number;
      status: FlowStatus;
      /**
       * Stream completion — stamped when `run_turn` finishes emitting the terminal
       * `response.completed`/`response.incomplete`. `None` if the flow errored or was
       * cancelled mid-stream.
       */
      stream_end_ms?: number | null;
      stream_end_offset_ms?: number | null;
      terminal_reason?: string | null;
      type: "flow_status";
      upstream_target?: string | null;
      uri: string;
      usage: FlowUsage | null;
      usage_anomaly_count: number;
    }
  | {
      edges: TopologyEdge[];
      nodes: TopologyNode[];
      type: "topology_update";
    };
/**
 * Coarse lifecycle phase attached to every authoritative live-flow mutation.
 * The vocabulary is deliberately bounded: usage and all non-terminal enrichment
 * are `progress`, while the exactly-once final store mutation is `terminal`.
 */
export type FlowMutationPhase = "open" | "progress" | "terminal";
/**
 * The four per-domain cursors the dashboard tracks. Each [`DashboardFrame`]
 * carries exactly one, and the client dedups whole frames per-domain
 * (`seq <= last_seq[domain]` drops the batch). Serializes snake_case.
 */
export type Domain = "flow" | "metrics" | "topology" | "monitor";

/**
 * One schema document containing every named dashboard contract.  This root is
 * used only to generate TypeScript declarations; endpoint validators are
 * generated from the individual roots returned by [`root_schemas`].
 */
export interface DashboardContracts {
  bootstrap: DashboardBootstrap;
  catalog_response: CatalogEntry[];
  flow_detail: FlowDetailBody;
  flows_response: FlowsResponse;
  history_response: HistoryResponse;
  kill_response: KillResponse;
  login_request: LoginRequest;
  metrics_response: MetricsSnapshot;
  overview_response: OverviewResponse;
  snapshot_response: SnapshotResponse;
  topology_response: TopologySnapshot;
  ws_frame: DashboardFrame;
  ws_snapshot: SnapshotMessage;
}
/**
 * CSP-safe bootstrap injected into the authenticated dashboard shell.
 */
export interface DashboardBootstrap {
  authenticated: boolean;
  csrf_token: string;
  mutations_enabled: boolean;
  schema_version: number;
}
/**
 * One catalog entry (`GET /dashboard/api/catalog` — a BARE array, no cursor).
 * `{id, context_limit}` where `context_limit` is the per-model max-context window
 * surfaced from the upstream `/v1/models` snapshot (gap 06).
 *
 * NULLABLE end-to-end (gap 06 contract migration): `context_limit` is
 * `Option<i64>`, serialized as `null` when the upstream advertises no window —
 * distinct from a real `0`. Previously this DTO collapsed a missing window to a
 * non-null `0` (`unwrap_or(0)`), which lies-with-zeros: a `0` ceiling reads as
 * garbage/infinite utilization downstream (spec 09's context-window gauge).
 * `measured` when advertised; `unavailable`/`None` when the upstream omits it.
 * The frontend renders `—` on `null`, NEVER `0`. Derives `Deserialize` alongside
 * `Serialize` so the changed wire field round-trips in a test (AGENTS.md: no
 * changed wire field without a deserialize-then-serialize proof).
 */
export interface CatalogEntry {
  /**
   * The per-model max-context window (tokens), or `null`/absent when the
   * upstream advertises none. `skip_serializing_if` so an unavailable window
   * OMITS the key rather than emitting `null` — either is honest (the frontend
   * type is `number | null` with the field optional); both are distinct from a
   * real `0`.
   */
  context_limit?: number | null;
  id: string;
}
/**
 * `GET /dashboard/api/flows/:id` — the 3-pane inspector body. Carries the summary
 * fields, the three captured on-wire bodies (inbound, normalized, upstream —
 * ABSENT, not error, when the summary-byte quota evicted them), the inbound
 * headers, the replayed deltas, usage, the terminal, and cost. Mirrors the frozen
 * `FlowDetail` (`:id == api_call_id`). The three bodies, headers, and deltas are
 * the additive detail fields over a [`FlowRow`].
 */
export interface FlowDetailBody {
  api_call_id: string;
  /**
   * Gap 10b — the gap-03 per-attempt failover trace, projected onto the detail body
   * (spec 11's inspector stepper reads the whole list; spec 10 reads the served
   * attempt's `first_upstream_byte_ms` to enrich the upstream-wait segment). Each
   * [`Attempt`] is body-free scalar provenance + bounded taxonomic codes — never a raw
   * upstream error body. `skip_serializing_if = Vec::is_empty` so a flow with no recorded
   * attempt OMITS the key (matches the frontend's optional `attempts?`).
   */
  attempts?: Attempt[];
  cache_price_impact_usd?: number | null;
  /**
   * Every body section available from the durable per-turn artifact. This includes
   * successful upstream/served responses that were intentionally never retained in
   * the live FlowStore. Sections load lazily at the HTTP request, not into snapshots.
   */
  captured_sections?: CapturedSection[];
  cost: number | null;
  /**
   * Gap 07 — the [`CostConfidence`] of `cost` (confident/estimated/unavailable),
   * mirroring the flow-row tag so the inspector labels an `estimated` figure.
   */
  cost_confidence: "confident" | "estimated" | "unavailable";
  deltas: FlowDelta[];
  /**
   * Monitor-domain watermark of the single transcript snapshot used to build
   * `deltas`. [`FlowDelta::sequence`] remains a per-flow replay ordinal and is
   * intentionally NOT comparable to live `DebugUpdate.sequence` values. The SPA
   * appends only live monitor segments whose monitor sequence is strictly greater
   * than this watermark, so the replay/live seam neither duplicates nor drops
   * repeated or same-millisecond content.
   */
  deltas_through_monitor_seq: number;
  detail_source: FlowDetailSource;
  effective_route_limit?: number | null;
  elapsed_ms?: number | null;
  /**
   * Terminal finalize — stamped when the flow reaches its terminal state
   * (`finalize`), for EVERY terminal (completed, failed, cancelled). Always `Some`
   * once the flow is terminal; the right edge of the waterfall.
   */
  finalize_ms?: number | null;
  finalize_offset_ms?: number | null;
  finished_ms?: number | null;
  /**
   * True TTFT — the wall-clock instant the FIRST canonical **content** SSE delta was
   * emitted to the client. NOT reasoning, tool-argument, refusal, or signature
   * deltas: a stream that emits reasoning/tool deltas before content does NOT stamp
   * this early (first-write-wins on the content arm only). `None` if the flow errored
   * before any content delta.
   */
  first_content_delta_ms?: number | null;
  first_content_delta_offset_ms?: number | null;
  /**
   * Gap 10b — the gap-03 flow-level wire time-to-first-byte (the served attempt's first
   * on-wire chunk). Distinct from `first_content_delta_ms` (first content delta to the
   * CLIENT). `None` ⇒ absent ⇒ renders `—`, NEVER `0`.
   */
  first_upstream_byte_ms?: number | null;
  flow_seq: number;
  /**
   * The captured INBOUND request body (parsed JSON). Absent when evicted by the
   * D1 summary-byte quota; parsed back to a `Value` so the SPA renders the tree.
   */
  inbound_body?: {
    [k: string]: unknown;
  };
  inbound_headers?: {
    [k: string]: string;
  } | null;
  /**
   * Request ingress — when the FlowStore first `open`ed the record (≈ `started_ms`).
   * Always `Some` once a record exists; the explicit phase value the waterfall
   * anchors the other phases against.
   */
  ingress_ms?: number | null;
  /**
   * Monotonic offset from flow ingress. Present on newly captured records; legacy
   * snapshots without offsets continue to use ordered epoch timestamps as fallback.
   */
  ingress_offset_ms?: number | null;
  model_requested?: string | null;
  model_served?: string | null;
  /**
   * Inbound→canonical normalization settled — stamped when the engine captures the
   * normalized canonical body (`set_normalized`). `None` if the flow errored before
   * normalization (an extractor/JSON rejection caught by the L0 guard).
   */
  normalization_done_ms?: number | null;
  normalization_done_offset_ms?: number | null;
  /**
   * The captured CANONICAL/normalized body (D2), parsed. Absent when evicted.
   */
  normalized?: {
    [k: string]: unknown;
  };
  normalized_usage: FlowUsage | null;
  response_id?: string | null;
  revision: number;
  /**
   * Upstream routing/lowering decision — stamped when the engine commits the actual
   * on-wire upstream request (`set_upstream` at the leaf). `None` if the flow never
   * reached the wire (pre-spawn lowering/budget failure, replay-only).
   */
  routing_decision_ms?: number | null;
  routing_decision_offset_ms?: number | null;
  started_ms: number;
  status: FlowStatus;
  /**
   * Stream completion — stamped when `run_turn` finishes emitting the terminal
   * `response.completed`/`response.incomplete`. `None` if the flow errored or was
   * cancelled mid-stream.
   */
  stream_end_ms?: number | null;
  stream_end_offset_ms?: number | null;
  terminal_reason?: string | null;
  /**
   * The captured UPSTREAM on-wire chat body (D2), parsed. Absent when evicted.
   */
  upstream_body?: {
    [k: string]: unknown;
  };
  /**
   * Gap 05 — the captured upstream RESPONSE/ERROR body (parsed) + its `truncated`
   * flag, projected from the live record's
   * [`upstream_response`](crate::dashboard_flow::FlowRecord::upstream_response).
   * Present ONLY on this LIVE detail path (the diagnostic operator endpoint) when
   * response capture is armed AND the turn produced an upstream error body; absent
   * otherwise (capture off / no body / evicted by the byte quota). DELIBERATELY kept
   * OFF the body-free [`FlowRow`] list rows and
   * [`SnapshotFlowSummary`](crate::dashboard_flow::SnapshotFlowSummary) (the 135 GiB
   * body-free-snapshot invariant). `skip_serializing_if` so an absent body OMITS the
   * key. Consumed by gap 14 (failure taxonomy); the React app ignores it until then.
   */
  upstream_response?: FlowUpstreamResponse | null;
  upstream_target?: string | null;
  usage: FlowUsage | null;
  usage_anomaly_count: number;
}
/**
 * Gap 03 — one upstream dispatch attempt's full provenance: WHICH provider, WHAT model,
 * HOW LONG it took, WHEN the first wire byte arrived, and the OUTCOME. The failover loop
 * records one per provider it tries (failed ones + the served one); a non-failover
 * (bare-leaf / single-upstream / routing-with-no-fallback) flow records exactly one.
 *
 * All timestamps are MEASURED epoch-ms (`now_ms`). `first_upstream_byte_ms` is the wire
 * instant the attempt's FIRST chunk arrived — `None` when the attempt never received
 * response headers / a first chunk (a connect/timeout/non-2xx failure), so an unmeasured
 * first-byte time is `None`, NEVER `0` (don't-lie-with-zeros). `error_class` /
 * `failover_reason` are `None` on the served attempt and are BOUNDED taxonomic codes
 * (never raw upstream text) on a failed attempt — they ride the body-free summary, so
 * raw error bodies stay behind spec 05's gated seam. Snake_case + `skip_serializing_if`
 * so a `None` field is absent on the wire.
 */
export interface Attempt {
  /**
   * Monotonic attempt duration. New records always populate this; `None` identifies
   * legacy data that must fall back to an ordered epoch pair or stay unavailable.
   */
  duration_ms?: number | null;
  /**
   * Epoch-ms the attempt resolved (served first chunk, or failed). Always measured.
   */
  end_ms: number;
  /**
   * Bounded taxonomic failure code; `None` on the served attempt.
   */
  error_class?: AttemptErrorClass | null;
  /**
   * Bounded taxonomic failover reason; `None` on the served attempt.
   */
  failover_reason?: AttemptFailoverReason | null;
  /**
   * Epoch-ms the FIRST chunk arrived on the wire for this attempt. `None` when the
   * attempt never received a first chunk (failed before response headers) — NEVER `0`.
   */
  first_upstream_byte_ms?: number | null;
  /**
   * Monotonic response-header offset from attempt start. Unlike the display epoch,
   * this remains valid across wall-clock adjustments.
   */
  first_upstream_byte_offset_ms?: number | null;
  /**
   * The model actually sent on the wire for this attempt (post provider-remap), when
   * known. `None` when the attempt failed before the on-wire model was finalized.
   */
  model?: string | null;
  /**
   * The provider name this attempt dispatched to (the failover provider's name, the
   * routing route's name, or the synthetic `"primary"` for a bare single upstream).
   */
  provider?: string | null;
  /**
   * Epoch-ms the attempt began (the dispatch was issued). Always measured.
   */
  start_ms: number;
  /**
   * Served vs failed.
   */
  status: "served" | "failed";
}
export interface CapturedSection {
  bytes: number;
  content: unknown;
  encoding: string;
  name: string;
  partial: boolean;
}
/**
 * One streamed delta replayed into the inspector (from the MonitorHub snapshot,
 * filtered by the flow's `response_id`). Mirrors the frozen `FlowDelta`:
 * `{sequence, kind, payload?, ts_ms?}`. `payload` is the heterogeneous delta body
 * (a segment text, an event summary, a status, …); the SPA narrows at the use
 * site. `sequence` is a per-flow ordinal (the replay order), NOT a domain cursor.
 */
export interface FlowDelta {
  kind: string;
  payload?: unknown;
  sequence: number;
  ts_ms?: number | null;
}
/**
 * Token usage attached to a flow once the upstream response reports it.
 *
 * Gap 07 — usage CONFIDENCE. `prompt`/`completion`/`total` are the core counts the
 * upstream always reports. `cached`/`reasoning` are OPTIONAL token classes the
 * upstream may or may not break out: a `0` and "the upstream never reported this
 * class" are DIFFERENT facts. They are therefore `Option<i64>` — `Some(0)` is a
 * provider-reported zero (e.g. "0 cache hits this turn"), `None` is UNAVAILABLE
 * (the upstream omitted `prompt_tokens_details`/`completion_tokens_details`). Serialized
 * with `skip_serializing_if` so an unreported class is ABSENT on the wire (the
 * frontend renders `—`), never a fabricated `0` (don't-lie-with-zeros). The
 * distinction is load-bearing for cost confidence: a `cached` charge against a
 * model with no configured cache rate (or an unreported `cached`) is `estimated`,
 * not `confident`.
 */
export interface FlowUsage {
  /**
   * Cache-read prompt tokens. `Some(n)` measured (incl. a reported `0`); `None`
   * when the upstream did not report a cached breakdown (UNAVAILABLE, not `0`).
   */
  cached?: number | null;
  completion: number;
  prompt: number;
  /**
   * Reasoning (thinking) tokens. `Some(n)` measured (incl. a reported `0`); `None`
   * when the upstream did not report reasoning details (UNAVAILABLE, not `0`).
   */
  reasoning?: number | null;
  total: number;
}
/**
 * Gap 05 — the captured upstream RESPONSE/ERROR body projected onto the live
 * [`FlowDetailBody`] (the `/dashboard/api/flows/:id` detail path only — NEVER the
 * body-free list rows or snapshot summaries). `body` is the redacted, capped bytes
 * parsed back to a JSON `Value` (or a string `Value` for a non-JSON / `[redacted:
 * unparseable body …]` marker, mirroring the other captured bodies); `truncated`
 * flags that the cap truncated the raw body, so the dashboard shows a PARTIAL body
 * honestly rather than presenting it as complete. Present ONLY when capture is armed
 * AND the turn produced an upstream error body (the live record's
 * [`upstream_response`](crate::dashboard_flow::FlowRecord::upstream_response) is
 * `Some`); absent otherwise (capture off / no body / evicted by the byte quota).
 * Derives `Deserialize` (alongside `Serialize`) so the new wire field round-trips in a
 * test (AGENTS.md: no new wire field without a deserialize-then-serialize proof) — the
 * enclosing [`FlowDetailBody`] stays serialize-only (it is only ever a response), so the
 * round-trip is pinned on THIS self-contained sub-DTO. Consumed by gap 14 (failure
 * taxonomy).
 */
export interface FlowUpstreamResponse {
  /**
   * The redacted, capped upstream response/error body, parsed to JSON (or a string
   * `Value` for a non-JSON / marker body). An EMPTY captured body parses to a string
   * `""` — distinct from the whole field being ABSENT (capture off / no body).
   */
  body: {
    [k: string]: unknown;
  };
  /**
   * Whether the cap truncated the raw body (the retained bytes are a PREFIX). The
   * dashboard must flag a truncated body rather than presenting it as complete.
   */
  truncated: boolean;
}
/**
 * `GET /dashboard/api/flows` — the paged flow list + total + the FlowStore
 * domain cursor. Matches the frozen `FlowsResponse`.
 */
export interface FlowsResponse {
  flow_seq: number;
  flows: FlowRow[];
  /**
   * Total rows AFTER filtering but BEFORE paging (so the SPA can page).
   */
  total: number;
}
/**
 * One row in the flow table (`GET /dashboard/api/flows`) — the body-free
 * [`SnapshotFlowSummary`](crate::dashboard_flow::SnapshotFlowSummary) fields PLUS
 * the D13 `cost` roll-up (usage × the served model's price). Mirrors the frozen
 * `FlowSummary` (types.ts) exactly: the `Option` fields use `skip_serializing_if`
 * to match the frontend's optional-key validators, EXCEPT `usage` (serialized as
 * `null` when absent — the frontend accepts absent/null/usage) and `cost`
 * (`null`-not-absent when the served model has no configured price).
 */
export interface FlowRow {
  api_call_id: string;
  /**
   * Gap 10b — the gap-03 per-attempt failover trace projected onto the row (spec 11's
   * stepper reads the whole list; spec 10 reads the served attempt's
   * `first_upstream_byte_ms`). Each [`Attempt`] is body-free scalar provenance + bounded
   * taxonomic codes — never a raw upstream error body. `skip_serializing_if =
   * Vec::is_empty` so a flow with no recorded attempt OMITS the key (the frontend's
   * `attempts?` is absent), matching the body-free summary's wire shape.
   */
  attempts?: Attempt[];
  cache_price_impact_usd?: number | null;
  /**
   * Gap 04 — the STABLE, NON-SECRET client attribution label (key-hash `key-<hex>`
   * display id / configured caller-id / User-Agent fallback), projected from the
   * flow record/summary. `skip_serializing_if` so an unattributed flow OMITS the key
   * (absent ⇒ renders `—`, never a fabricated id). Additive/optional: the frontend
   * ignores it until the client-attribution UI (gap 15). The raw key is never here —
   * only the one-way hash prefix ever existed as a label.
   */
  client_label?: string | null;
  /**
   * Gap 04 — the [`ClientSource`] the label was derived from (so the weak
   * `user_agent` fallback is distinguishable from a key-hash / configured-id). `None`
   * (absent) exactly when `client_label` is `None`.
   */
  client_source?: ClientSource | null;
  /**
   * USD cost of the flow (usage × the served model's [`ModelPrice`]). `null`
   * when no price is configured for `model_served` — never a fabricated zero.
   */
  cost: number | null;
  /**
   * Gap 07 — the [`CostConfidence`] of `cost`: `confident` (priced + every billed
   * class has a known rate), `estimated` (a class falls back to the default `0.0`
   * cached rate / cached unreported), or `unavailable` (unpriced ⇒ `cost: null`).
   * Always present so the frontend can label an `estimated` figure as such and
   * distinguish an `unavailable` cost from a measured `$0.00`.
   */
  cost_confidence: "confident" | "estimated" | "unavailable";
  effective_route_limit?: number | null;
  elapsed_ms?: number | null;
  /**
   * Terminal finalize — stamped when the flow reaches its terminal state
   * (`finalize`), for EVERY terminal (completed, failed, cancelled). Always `Some`
   * once the flow is terminal; the right edge of the waterfall.
   */
  finalize_ms?: number | null;
  finalize_offset_ms?: number | null;
  finished_ms?: number | null;
  /**
   * True TTFT — the wall-clock instant the FIRST canonical **content** SSE delta was
   * emitted to the client. NOT reasoning, tool-argument, refusal, or signature
   * deltas: a stream that emits reasoning/tool deltas before content does NOT stamp
   * this early (first-write-wins on the content arm only). `None` if the flow errored
   * before any content delta.
   */
  first_content_delta_ms?: number | null;
  first_content_delta_offset_ms?: number | null;
  /**
   * Gap 10b — the gap-03 flow-level wire time-to-first-byte (the served attempt's first
   * on-wire chunk). Distinct from `first_content_delta_ms` (the first content delta to
   * the CLIENT). `None` ⇒ absent ⇒ renders `—` downstream, NEVER `0`.
   */
  first_upstream_byte_ms?: number | null;
  /**
   * Request ingress — when the FlowStore first `open`ed the record (≈ `started_ms`).
   * Always `Some` once a record exists; the explicit phase value the waterfall
   * anchors the other phases against.
   */
  ingress_ms?: number | null;
  /**
   * Monotonic offset from flow ingress. Present on newly captured records; legacy
   * snapshots without offsets continue to use ordered epoch timestamps as fallback.
   */
  ingress_offset_ms?: number | null;
  method: string;
  model_requested?: string | null;
  model_served?: string | null;
  /**
   * Inbound→canonical normalization settled — stamped when the engine captures the
   * normalized canonical body (`set_normalized`). `None` if the flow errored before
   * normalization (an extractor/JSON rejection caught by the L0 guard).
   */
  normalization_done_ms?: number | null;
  normalization_done_offset_ms?: number | null;
  /**
   * Calculation-only corrected usage; raw provider values remain in `usage`.
   */
  normalized_usage: FlowUsage | null;
  response_id?: string | null;
  /**
   * Per-flow optimistic-concurrency version from the authoritative FlowStore.
   */
  revision: number;
  /**
   * Upstream routing/lowering decision — stamped when the engine commits the actual
   * on-wire upstream request (`set_upstream` at the leaf). `None` if the flow never
   * reached the wire (pre-spawn lowering/budget failure, replay-only).
   */
  routing_decision_ms?: number | null;
  routing_decision_offset_ms?: number | null;
  started_ms: number;
  status: FlowStatus;
  /**
   * Stream completion — stamped when `run_turn` finishes emitting the terminal
   * `response.completed`/`response.incomplete`. `None` if the flow errored or was
   * cancelled mid-stream.
   */
  stream_end_ms?: number | null;
  stream_end_offset_ms?: number | null;
  terminal_reason?: string | null;
  upstream_target?: string | null;
  uri: string;
  usage: FlowUsage | null;
  usage_anomaly_count: number;
}
export interface HistoryResponse {
  database_bytes: number;
  dropped_writes: number;
  newest_at_ms: number | null;
  oldest_at_ms: number | null;
  points: HistoryPoint[];
  retained_cuts: number;
}
export interface HistoryPoint {
  at_ms: number;
  cursors: SeqCursors;
  cut_id: number;
  instant: InstantMetricSample;
}
/**
 * The four per-domain cursors carried on the initial [`SnapshotMessage`] — the
 * `{flow,metrics,topology,monitor}` sequences the SPA installs as its dedup
 * baseline (`commitSnapshot` in `dashboard-frontend/src/api/ws.ts`). Serializes
 * snake_case to the frozen `SeqCursors` contract.
 */
export interface SeqCursors {
  backend_metrics_seq: number;
  flow_seq: number;
  metrics_seq: number;
  monitor_seq: number;
  topology_seq: number;
}
/**
 * One reset-on-publish dashboard telemetry interval. Unlike [`WindowReport`], this
 * value contains only events observed since the previous publisher cut. Rates are
 * normalized by `interval_duration_ms`, so a delayed tick remains truthful.
 */
export interface InstantMetricSample {
  accepted_per_sec: number | null;
  accepted_requests: number;
  active_streams_now: number;
  cancellation_pct: number | null;
  cancellations: number;
  cost_confidence: TerminalCostConfidence;
  cost_per_min: number | null;
  failure_pct: number | null;
  failures: number;
  interval_duration_ms: number | null;
  latency_overflow_count: number;
  latency_samples: number;
  max_relative_error: number;
  p50_ms: number | null;
  p50_quality: InstantMetricQuality;
  p95_ms: number | null;
  p95_quality: InstantMetricQuality;
  p99_ms: number | null;
  p99_quality: InstantMetricQuality;
  priced_samples: number;
  quantile_method: QuantileMethod;
  ready: boolean;
  reported_tokens_per_sec: number | null;
  successes: number;
  terminal_per_sec: number | null;
  terminal_requests: number;
  usage_anomaly_count: number;
  usage_samples: number;
}
/**
 * Successful `POST /dashboard/api/flows/:id/kill` response.
 */
export interface KillResponse {
  api_call_id: string;
  killed: boolean;
}
export interface LoginRequest {
  token: string;
}
/**
 * The full `/api/metrics`-shaped snapshot body (the flat tile + the three
 * windows) PLUS its `metrics_seq` cursor — the snapshot-time analogue of a live
 * [`DashboardPayload::MetricTick`]. Mirrors the frontend `MetricsResponse`.
 */
export interface MetricsSnapshot {
  generated_at_ms: number;
  instant: InstantMetricSample;
  metrics_seq: number;
}
/**
 * `GET /dashboard/api/overview`: an immutable exact-window rollup. The aggregate is
 * flattened so the wire reads naturally (`totals`, `served_models`, `cost_series`, …)
 * while the calculation remains owned by MetricsLayer for both live and historical
 * cuts.
 */
export interface OverviewResponse {
  cancellations: OverviewDimensionRollup[];
  clients: OverviewDimensionRollup[];
  context: OverviewContextRollup;
  cost: OverviewCost;
  cost_series: OverviewCostPoint[];
  data_quality: OverviewDataQuality;
  failures: OverviewDimensionRollup[];
  generated_at_ms: number;
  lanes: OverviewLaneRollup[];
  metrics_seq: number;
  overflow: OverviewOverflowMetadata;
  provider_attempts_global: OverviewProviderAttempts;
  providers: OverviewDimensionRollup[];
  requested_models: OverviewDimensionRollup[];
  scope: OverviewScope;
  served_models: OverviewDimensionRollup[];
  tokens: OverviewTokens;
  totals: OverviewTotals;
}
export interface OverviewDimensionRollup {
  cost: OverviewCost;
  key: string;
  requests: number;
  tokens: OverviewTokens;
}
export interface OverviewCost {
  confidence: TerminalCostConfidence;
  samples: number;
  total_usd: number | null;
}
export interface OverviewTokens {
  cached: number | null;
  completion: number | null;
  prompt: number | null;
  reasoning: number | null;
  samples: number;
}
export interface OverviewContextRollup {
  average_pressure_pct: number | null;
  data_quality: OverviewDataQuality;
  effective_route_limit_min: number | null;
  input_tokens: number | null;
  samples: number;
  unavailable_samples: number;
}
export interface OverviewCostPoint {
  at_ms: number;
  cost: OverviewCost;
  data_quality: OverviewDataQuality;
  requests: number;
}
export interface OverviewLaneRollup {
  cost: OverviewCost;
  model: string;
  provider: string;
  requests: number;
  tokens: OverviewTokens;
}
export interface OverviewOverflowMetadata {
  aggregate_folded_samples: number;
  dimension_limit: number;
  overflowed: boolean;
  provider_folded_samples: number;
  slot_folded_samples: number;
  unattributable_requests: number;
}
export interface OverviewProviderAttempts {
  data_quality: OverviewDataQuality;
  providers: ProviderLatency[];
  scope: OverviewMetricScope;
}
/**
 * Gap 12 — the public per-provider latency + error-distribution DTO (additive on the
 * D4 topology node; consumed by spec 13). Percentiles are `derived` ms over the
 * provider's ATTEMPT-latency histogram (a FAILED primary's latency is included — spec
 * 12 — so final-served latency alone cannot hide an unhealthy provider). `error_rate`
 * is the percentage of the provider's attempts that FAILED. A provider with zero
 * in-window samples is ABSENT (don't-lie-with-zeros), so a present DTO always has
 * `samples >= 1`. All floats are finite (the frozen finite-number wire contract).
 */
export interface ProviderLatency {
  /**
   * DQ tag — always `derived` for a present entry (the `unavailable` case is absence).
   */
  data_quality: "derived" | "partial";
  /**
   * Percentage of attempts that failed (`failed / samples × 100`). A genuine MEASURED
   * `0.0` for an all-served provider (distinct from the `unavailable`/absent case).
   */
  error_rate: number;
  errors: ProviderErrorDistribution;
  /**
   * Of `samples`, the count that FAILED before serving (failed primaries included).
   */
  failed: number;
  /**
   * `derived` p50 attempt latency (ms).
   */
  p50: number | null;
  /**
   * `derived` p95 attempt latency (ms).
   */
  p95: number | null;
  /**
   * `derived` p99 attempt latency (ms).
   */
  p99: number | null;
  /**
   * The provider label these metrics are for (the bounded provider/route id, or the
   * `__other__` overflow bucket once the per-slot provider cap is exceeded).
   */
  provider: string;
  /**
   * Total ATTEMPTS to this provider in the window (served + failed) — the per-provider
   * measurability denominator. Always `>= 1` (a zero-sample provider is absent).
   */
  samples: number;
  /**
   * Of `samples`, the count that SERVED (produced the first chunk).
   */
  served: number;
}
/**
 * Bounded per-class failure tally (gap 03 taxonomy). Absent classes are omitted.
 */
export interface ProviderErrorDistribution {
  connect?: number;
  http_status?: number;
  other?: number;
  stream?: number;
  terminal?: number;
  timeout?: number;
}
export interface OverviewScope {
  client: string | null;
  cut_id?: number | null;
  model: string | null;
  requested_at_ms: number | null;
  selected_at_ms: number | null;
  status: string | null;
  upstream: string | null;
  window: OverviewWindow;
}
export interface OverviewTotals {
  cancellations: number;
  cost: OverviewCost;
  failures: number;
  requests: number;
  successes: number;
  tokens: OverviewTokens;
}
/**
 * `GET /dashboard/api/snapshot?at=<unix_ms>` — a body-free frozen cut. Mirrors
 * the frozen `SnapshotResponse`: the per-domain `cursors`, the cut instant, the
 * body-free flow summaries (priced), and the metrics/topology cuts reshaped into
 * their REST bodies (`null` when the cut is empty for that domain).
 */
export interface SnapshotResponse {
  at_ms: number;
  cursors: SeqCursors;
  /**
   * Stable durable-cut identifier. Equal to the coordinated cut's epoch-ms stamp;
   * absent only when no historical cut exists yet.
   */
  cut_id?: number | null;
  /**
   * Whether this selected cut dropped its oldest flow summaries to fit the quota.
   */
  flow_summaries_truncated: boolean;
  history: SnapshotHistoryMetadata;
  metrics: MetricsSnapshot | null;
  /**
   * Persisted monitor messages through this cut. Empty for legacy in-memory-only
   * cuts; durable cuts use them for historical flow timelines and Theater replay.
   */
  monitor_messages?: DebugWsMessage[];
  summaries: FlowRow[];
  topology: TopologySnapshot | null;
}
/**
 * Bounds and memory use of the retained historical-cut ring.
 */
export interface SnapshotHistoryMetadata {
  newest_at_ms: number | null;
  oldest_at_ms: number | null;
  quota_bytes: number;
  retained_bytes: number;
  retained_cuts: number;
}
export interface DebugRequest {
  completed_at_ms: number | null;
  error: string | null;
  model: string;
  response_id: string;
  started_at_ms: number;
  stats: DebugRequestStats;
  status: DebugRequestStatus;
  updated_at_ms: number;
  /**
   * D3: latest cumulative token usage for the flow (`None` until the first
   * usage-bearing chunk). Retained so `snapshot()` replays it to a late
   * subscriber after the `RequestUpsert`.
   */
  usage?: DebugUsage | null;
}
export interface DebugRequestStats {
  assistant_messages: number;
  developer_messages: number;
  function_calls: number;
  function_outputs: number;
  input_chars: number;
  input_items: number;
  instructions_chars: number;
  reasoning_items: number;
  system_messages: number;
  tool_count: number;
  tool_items: number;
  turn_count: number;
  user_messages: number;
}
/**
 * D3: the latest cumulative token usage retained on a [`DebugRequest`] so the
 * `/debug/ws` snapshot can replay it to a late subscriber.
 */
export interface DebugUsage {
  cached: number;
  completion: number;
  prompt: number;
  reasoning: number;
  total: number;
}
export interface DebugSegment {
  kind: DebugSegmentKind;
  text: string;
  timestamp_ms: number;
}
export interface DebugTimelineEvent {
  images: DebugEventImage[];
  kind: string;
  payload_preview: string | null;
  summary: string;
  timestamp_ms: number;
}
/**
 * Metadata about an image found in a request/response preview, surfaced to the
 * debug UI over `/debug/ws`. Carries ONLY non-sensitive descriptors — never the
 * raw image bytes or URL (G4 round-4 #4): `data:`/signed URLs must not leave the
 * process via the monitor broadcast. The UI renders a redacted placeholder card
 * from this metadata, not the image itself.
 */
export interface DebugEventImage {
  id: string;
  label: string;
  mime_type: string;
  path: string;
  size_bytes: number | null;
}
/**
 * The full `/api/topology`-shaped snapshot body (nodes + edges + the price table)
 * PLUS its `topology_seq` cursor. Mirrors the frontend `TopologyResponse`. The
 * price table is empty until D13 wires the price config; an empty map satisfies
 * the frontend `isPriceTable` guard (vacuously every value is a finite price).
 */
export interface TopologySnapshot {
  edges: TopologyEdge[];
  nodes: TopologyNode[];
  price_table: {
    [k: string]: ModelPrice;
  };
  topology_seq: number;
}
/**
 * A topology edge (gateway → provider). The aggregate throughput/token/cost
 * rates are D5/D13 roll-ups; until a price/throughput aggregation feeds them
 * they serialize as `0.0` (the contract requires the keys present + finite, not
 * a specific value), so the byte-shape is exact while the rich values land in
 * D13.
 */
export interface TopologyEdge {
  attempts_per_sec: number;
  from: string;
  reported_tokens_per_sec: number | null;
  terminal_cost_per_sec: number | null;
  terminal_flows_per_sec: number;
  to: string;
}
/**
 * A topology node — the D4 `ProviderHealth` shape, except `catalog_size` is
 * flattened from `Option<u64>` to a non-null `u64` (defaulting `None → 0`): the
 * frozen frontend contract validates `catalog_size` as a required unsigned int
 * (NOT nullable), unlike the other `Option` fields which serde emits as `null`.
 * Every other field mirrors `ProviderHealth` exactly (keys always present, the
 * nullable ones as JSON `null`).
 */
export interface TopologyNode {
  base_url: string;
  catalog_fetched_ms: number | null;
  /**
   * Flattened from `ProviderHealth::catalog_size: Option<u64>` to a required
   * non-null count (`None → 0`) per the frozen contract.
   */
  catalog_size: number;
  consecutive_failures: number;
  cooling_until_ms: number | null;
  /**
   * REST/snapshot-only normalized engine telemetry. Live WS topology frames
   * leave this absent, matching `per_provider`.
   */
  engine_metrics?: BackendProviderMetrics | null;
  failover_count: number;
  id: string;
  last_error: string | null;
  name: string;
  /**
   * Gap 12 — the ADDITIVE per-provider latency (p50/p95/p99) + error distribution for
   * this provider over the m1 window, aggregated off the evict-safe per-attempt trace
   * (spec 03), NOT the point-in-time `ProviderHealth` counters. `None`/ABSENT when the
   * provider had ZERO attempt samples in the window (don't-lie-with-zeros — the
   * frontend renders `—`, never a fabricated `0ms`/`0%`). `skip_serializing_if` keeps
   * the field off the wire entirely for a no-sample node, so the EXISTING frozen
   * `TopologyNode` contract (D9/D10/D12) is undisturbed; spec 13 reads this field. The
   * LIVE WS topology frame leaves it `None` (the WS frame does not join metrics, like
   * its `0.0` edge rates) — it is populated only on the REST `/topology` + `/snapshot`
   * reshape, which already join the m1 window.
   */
  per_provider?: ProviderLatency | null;
  route: string | null;
  served_count: number;
  status: ProviderStatus;
}
export interface BackendProviderMetrics {
  coverage: BackendMetricsCoverage;
  engine_kind: BackendEngineKind;
  instant: BackendInstantMetrics;
  last_error_class?: string | null;
  last_success_ms?: number | null;
  scraped_at_ms?: number | null;
  status: BackendMetricsStatus;
  windows: BackendMetricWindows;
}
export interface BackendInstantMetrics {
  engine_sleeping?: boolean | null;
  kv_cache_token_capacity?: number | null;
  kv_cache_utilization?: number | null;
  running_requests?: number | null;
  waiting_by_reason?: {
    [k: string]: number;
  };
  waiting_requests?: number | null;
}
export interface BackendMetricWindows {
  h1: BackendMetricsWindow;
  m1: BackendMetricsWindow;
  m5: BackendMetricsWindow;
}
export interface BackendMetricsWindow {
  cached_prompt_tokens_per_sec?: number | null;
  completed_requests_per_sec?: number | null;
  finish_reasons?: {
    [k: string]: number;
  };
  generated_tokens_per_sec?: number | null;
  histograms: BackendLatencyMetrics;
  preemptions_per_sec?: number | null;
  prefix_cache_hit_ratio?: number | null;
  prompt_token_cache_hit_ratio?: number | null;
  prompt_tokens_per_sec?: number | null;
  samples: number;
  speculative_acceptance_ratio?: number | null;
  speculative_accepted_tokens_per_sec?: number | null;
  speculative_draft_tokens_per_sec?: number | null;
}
export interface BackendLatencyMetrics {
  decode_ms?: BackendHistogramSummary | null;
  end_to_end_ms?: BackendHistogramSummary | null;
  generation_tokens?: BackendHistogramSummary | null;
  inference_ms?: BackendHistogramSummary | null;
  inter_token_ms?: BackendHistogramSummary | null;
  iteration_tokens?: BackendHistogramSummary | null;
  prefill_ms?: BackendHistogramSummary | null;
  prompt_tokens?: BackendHistogramSummary | null;
  queue_ms?: BackendHistogramSummary | null;
  time_per_output_token_ms?: BackendHistogramSummary | null;
  ttft_ms?: BackendHistogramSummary | null;
}
export interface BackendHistogramSummary {
  p50?: number | null;
  p95?: number | null;
  p99?: number | null;
  quantile_method: string;
  samples: number;
}
/**
 * One model's billing rates (T13/D13), per 1k tokens. Field names mirror the
 * FROZEN frontend `ModelPrice` contract (`dashboard-frontend/src/api/types.ts`)
 * byte-for-byte so the `/dashboard/api/topology` `price_table` validates. All
 * three rates are finite (the frontend `isModelPrice` guard rejects NaN/Inf);
 * `cached_per_1k` defaults to `0.0` when a config entry omits it. This is the
 * SINGLE `ModelPrice` definition for the crate — the dashboard WS topology
 * snapshot re-exports it so REST + WS agree on the wire shape.
 *
 * Gap 07 — cached-price PRESENCE seam. `cached_per_1k` keeps its existing numeric
 * type (`f64`, default `0.0`), but a `0.0` is AMBIGUOUS: "the provider charges 0
 * for cache reads" is indistinguishable from "the config entry omitted the rate".
 * The ADDITIVE `cached_price_configured` boolean records which it is — set `true`
 * only when the source actually carried a `cached_per_1k` key (decided in the
 * custom [`Deserialize`] below). Downstream cost-CONFIDENCE (`dashboard_api`)
 * consumes THIS flag, NOT the numeric `0.0`: a flow that billed cached tokens at a
 * CONFIGURED rate is `confident`, one that fell back to the default `0.0` is
 * `estimated`. The flag is serialized additively (the frontend `isModelPrice`
 * accepts it); `cached_per_1k` stays `number` so the topology/Sankey price table is
 * NOT a second contract migration (spec 07 item 3).
 */
export interface ModelPrice {
  /**
   * USD per 1k CACHED (cache-read) prompt tokens. Defaults to `0.0` when the
   * config entry omits it (a provider with no cache discount). PRESERVES its
   * numeric type (gap 07) — presence is carried by `cached_price_configured`, not
   * by nulling this field.
   */
  cached_per_1k: number;
  /**
   * Gap 07 — whether `cached_per_1k` was EXPLICITLY configured (a `cached_per_1k`
   * key was present in the source), distinguishing a real configured `0.0`
   * cache-read rate from an OMITTED one (which also defaults to `0.0`). The
   * cost-confidence seam reads this so a default-`0.0` cached charge is flagged
   * `estimated`, never silently `confident`. Additive on the wire.
   */
  cached_price_configured: boolean;
  /**
   * USD per 1k PROMPT (input) tokens.
   */
  input_per_1k: number;
  /**
   * USD per 1k COMPLETION (output) tokens.
   */
  output_per_1k: number;
}
/**
 * The batched WS envelope: ONE frame per source update (e.g. one `DebugUpdate`),
 * carrying the originating domain, that domain's sequence at the cut, and the
 * batch of payloads. Per-domain whole-frame dedup on the client drops the WHOLE
 * `batch` when `seq <= last_seq[domain]`, so a batched Monitor frame never loses
 * a sibling to dedup.
 */
export interface DashboardFrame {
  batch: DashboardPayload[];
  domain: Domain;
  seq: number;
}
/**
 * The flat `/api/metrics`-shaped metric tile (metrics domain).
 */
export interface MetricTick {
  type: "metric_tick";
}
/**
 * The INITIAL WS message: a `type:"snapshot"` envelope the SPA waits for BEFORE
 * it renders. The frontend (`dashboard-frontend/src/api/ws.ts`) BUFFERS every
 * live [`DashboardFrame`] until this lands (`snapshotApplied`), so it MUST be the
 * FIRST frame on a `/dashboard/ws` connection — else the dashboard never renders
 * (D7b R1 finding 1). It seeds the store's cursors + flow rows + metrics/topology
 * baseline in one atomic install (`restoreLiveSnapshot`); subsequent live frames
 * build on it. Internally tagged `type:"snapshot"` to match the frozen
 * `SnapshotFrame` discriminant.
 */
export interface SnapshotMessage {
  cursors: SeqCursors;
  /**
   * Wire-facing flow ROWS (gap 10b `FlowRow`), NOT raw `SnapshotFlowSummary`s: the SPA's
   * `isSnapshotFrame` validates every row with the same guard as `/flows` (gap 07 requires
   * `cost_confidence` on EVERY row), so the WS snapshot must carry the same projection as
   * the REST reads — a raw summary (no cost fields) fails validation and the SPA then
   * silently drops the snapshot and sits at `connecting` forever, shadow-buffering frames.
   */
  flows: FlowRow[];
  /**
   * Metrics baseline (or `null` when metrics are disabled).
   */
  metrics: MetricsSnapshot | null;
  /**
   * Dashboard contract version. The SPA verifies this before installing any
   * cursor or opening the live pipeline.
   */
  schema_version: number;
  /**
   * Topology baseline (or `null` when no providers are published yet).
   */
  topology: TopologySnapshot | null;
  /**
   * Discriminant — always `"snapshot"`; the SPA routes on it.
   */
  type: "snapshot";
}
