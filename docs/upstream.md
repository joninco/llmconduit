# Upstream Client

## Component Overview

| Component | Line | Purpose |
|-|-|-|
| `UpstreamClient` trait | 555 | Async interface: `wire_api_for_model`, `stream_responses_native_with_timeout`, `stream_chat_completion` (+`_with_timeout`), `list_models`, `backend_metrics_targets`, `proxy_metrics`, `proxy_completions`, `count_tokens`, `response_body_idle_timeout`, `supported_model_catalog`, `candidate_backend_models`, `backend_candidate_plan`, `responses_capability_plan`, `provider_health`. |
| `ReqwestUpstreamClient` | 710 | Leaf HTTP client — one `reqwest::Client` per upstream base_url. POSTs finalized+sanitized requests, handles G1 shrink-and-retry, captures D2/F1d on-wire bodies. Implements `UpstreamClient` at line 2638. |
| `FailoverUpstreamClient` | 824 | Provider-level failover across a list of `FailoverUpstreamProvider`s, plus the resilience layer (same-provider retry, circuit breaker, bulkhead). Tries each provider in order; wraps first-chunk race via `prefetch_first_chunk`. Implements `UpstreamClient` at line 4812. |
| `RoutingUpstreamClient` | 1012 | Model-routing layer: loads `/v1/models` catalog from each primary provider (with stale-on-error degraded cache), resolves request model → provider+target, dispatches to the nested `FailoverUpstreamClient`. Implements `UpstreamClient` at line 5020. |
| `ServingToken` | 5884 | Interior-mutable shared identity per flow. Routing sets `route`; failover sets `provider`; leaf sets `model_served_final`. Carries per-attempt trace (`attempts`), scratch `attempt_header_byte_ms`, staged `pending_response_body`. |
| `ProviderStatus` | 282 | Healthy / Cooling / Down enum for the D4 topology map. |
| `ProviderHealth` | 295 | Serializable per-upstream health DTO (status, cooldown deadline, last error, counters, catalog meta). |
| `ProviderHealthSnapshot` | 414 | Versioned, immutable snapshot container. Published atomically via `ProviderHealthPublisher` on a coalesced 1 s tick. |
| `ProviderHealthPublisher` | 432 | `Mutex<Arc<ProviderHealthSnapshot>>` + monotonic version counter. `publish()` swaps in a new snapshot; `latest()` cheap-clones the `Arc`. |
| `ProviderMetrics` | 333 | Per-provider atomics: `served_count`, `failover_count`, `consecutive_failures`, `retry_attempt_count`, `retry_exhausted_count`. |
| `BackendResponsesRequest` | 492 | DTO for a native Responses dispatch (body, model, generated UUIDs, serving token, capture handle, capability allowlist) — never lowered through Chat Completions. |
| `UpstreamResponsesStream` | 485 | Native Responses event stream type (`engine::SseEvent` items, canonical event names/JSON payloads preserved). |
| `BackendFinalizationPolicies` | 5687 | Per-model-effort map, `template_family` override, `upstream_chat_kwargs` (global base + per-model). Built once from config, shared via `Arc`. Applied at the leaf. |
| `BackendChatRequest` | 6112 | DTO carrying the request + `response_id`, `serving` token, `capture` handle, `thinking_override`, `capability_allowlist`. Cloned across failover/route rebuilds. |
| `finalize_request_for_backend` | 6276 | Leaf request finalization: resolve per-model kwargs, reasoning effort (map→fragment or clamp), family `chat_template_kwargs`, effort fragment, profile thinking kwarg. |

---

## ReqwestUpstreamClient

`Line 710`, struct `ReqwestUpstreamClient`

The HTTP leaf. One instance per upstream `base_url` + `api_key` (the key itself may come from the environment via the config's `upstream_api_key_env`). Owns the `reqwest::Client`, request logger, SSE frame cap, and D2/F1d capture handles.

| Method | Line | Description |
|-|-|-|
| `fn new` | 1577 | Construct with defaults (delegates to `with_options`). |
| `fn with_options` | 1600 | Full constructor: `client`, `base_url`, `api_key`, `request_log_path`, `flatten_content`, `min_completion_tokens`, `max_sse_frame_bytes`. |
| `fn with_request_log_body_mode` | 1659 | Select the JSONL request-log body mode. |
| `fn with_request_timeout` | 1670 | Set the response-header / idle-gap deadline. |
| `fn with_finalization_policies` | 1636 | Attach per-model finalization policies post-construction. |
| `fn with_flow_store` | 1680 | Attach dashboard FlowStore handle (D2 capture seam). |
| `fn into_bare_primary` | 1693 | Mark this leaf as the direct engine upstream (synthesizes `provider = "primary"`). |
| `fn base_url_string` | 1701 | The configured upstream base URL, for D4 `ProviderHealth`. |
| `fn effective_responses_capabilities` | 1705 | Merge provider-layer Responses capability overrides for a served model. |
| `fn with_auth` | 1713 | Attach Bearer auth to a request builder if `api_key` is set. |
| `fn send_for_headers` | 1724 | Issue a request and await response headers under `request_timeout`. |
| `fn endpoint_url` | 1743 | Join a path onto `base_url` (handles trailing-slash normalization). |
| `fn server_root_endpoint_url` | 1755 | Build a server-root URL (metrics exposition). |
| `fn tokenize_url` | 1775 | Build `/tokenize` URL (strips `/v1` prefix). |
| `fn metrics_url` | 1779 | Build the backend metrics URL. |
| `fn backend_metrics_target` | 1786 | Build one debug-only backend-telemetry scrape target. |
| `fn send_chat_request` | 1800 | POST JSON to upstream URL with auth. No logging, no capture. |
| `fn logged_send_chat_request` | 1842 | Log to JSONL (if enabled), capture D2/F1d on-wire body, then POST. |
| `fn capture_upstream_body` | 1899 | D2: store the sanitized on-wire request body into the FlowStore record via the capped + redacting serializer. |
| `fn read_upstream_error_body` | 1925 | Read a failed response body under `UPSTREAM_ERROR_BODY_READ_CAP` (128 KiB) with idle timeout. |
| `fn capture_upstream_response_body` | 1973 | Gap-05/F1e: stage a failed-attempt's error body onto the `ServingToken` (capped + redacted). |
| `fn record_served_attempt_on_first_byte` | 2031 | Gap-03 bare-leaf path: wrap stream to record exactly one attempt at first-chunk yield. |
| `fn failed_bare_attempt` | 2104 | Build a FAILED `Attempt` with bounded taxonomic error codes. |
| `fn tokenize_is_known_unsupported` | 2134 | Negative cache of definitive `/tokenize` misses (404/405/501) per final model id. |
| `fn remember_tokenize_unsupported` | 2141 | Insert into the negative cache (bounded at 256 models). |
| `fn tokenize_sanitized_count` | 2159 | POST the sanitized request to `/tokenize` and read the count. |
| `fn dispatch_chat_stream` | 2246 | Core leaf dispatch: POST, handle G1 context-overflow shrink-and-retry, parse SSE stream. Resets per-attempt capture state. |
| `fn dispatch_responses_stream` | 2538 | Native-Responses dispatch for `wire_api = CodexResponses` providers. |
| `fn wire_api_for_model` (trait impl) | 2639 | Report the configured wire API. |
| `fn stream_responses_native_with_timeout` (trait impl) | 2643 | Native Responses dispatch with header + stream timeouts. |
| `fn stream_chat_completion` (trait impl) | 2657 | Trait entry: finalize via `finalize_request_for_backend` (line 2674), sanitize, dispatch. Handles bare-leaf attempt recording. |
| `fn count_tokens` (trait impl) | 2799 | Finalize, then POST to `/tokenize`; returns `None` on known-unsupported models. |
| `fn response_body_idle_timeout` (trait impl) | 2806 | The configured request timeout, for bounded non-streaming body reads. |
| `fn list_models` (trait impl) | 2810 | GET `/v1/models`. |
| `fn responses_capability_plan` (trait impl) | 2830 | Single-candidate capability plan for the bare leaf. |
| `fn proxy_metrics` (trait impl) | 2845 | Raw Prometheus exposition passthrough. |
| `fn backend_metrics_targets` (trait impl) | 2854 | This leaf as the only scrape target. |
| `fn provider_health` (trait impl) | 2858 | Empty vector — the bare leaf owns no provider metrics. |
| `fn proxy_completions` (trait impl) | 2878 | Raw `/v1/completions` proxy passthrough (no finalization). |

**Key invariants:**
- The leaf is the single point that sees the FINAL provider-model after routing/failover/exposed-alias remap — so it applies `finalization_policies` keyed by `request.model` (line 2674).
- G1 shrink-and-retry (inside `dispatch_chat_stream`, line 2246; bounded by `CONTEXT_OVERFLOW_MAX_ATTEMPTS = 4` at line 6750) happens BEFORE any SSE chunk is parsed — the routing/failover layers never see a context-limit error as a provider failure.
- D2 bare-leaf marker (`tag_primary_provider` at line 761) prevents a leaf nested inside failover/routing from clobbering the real provider name.
- Request-intrinsic 400/413/415/422 failures are terminal for that selected primary (`status_is_request_intrinsic_4xx`, line 5666): they neither cool the provider nor try a fallback. Retryable 408/429/500/502/503/504 statuses carry `safe_same_provider_retry` (same-provider retry, then failover); transport and stream failures remain failover-eligible but are never retried on the same provider.
- Provider proxy forwarding uses an allowlist (`should_proxy_request_header`, line 6727). Client authorization, API keys, cookies, proxy credentials, and dashboard/session headers are excluded; each provider uses its configured credential. The separate [native Anthropic transport](anthropic-subscription-proxy.md) selectively forwards subscription bearer authorization to its validated Anthropic origin before this provider layer.
- **Env-backed credentials:** config may set `upstream_api_key_env` instead of `upstream_api_key` (`src/config.rs` lines 1707/1753; validated at 3116–3140 — the named variable must exist, be non-empty, and not be co-set with the literal key). When a credential is configured, a failed upstream's error body is never retained verbatim: `capture_upstream_response_body` replaces it with the fixed `CREDENTIALLED_UPSTREAM_ERROR_MARKER` (line 82, applied at line 1985) because a backend that received our `Authorization` value can echo it under an ordinary field where key-based JSON redaction cannot identify it.
- Responses capability filtering is scoped to the already-selected primary chain. An incapable primary rejects the request, while incapable nested fallbacks are removed without cooldown; routing does not jump to another primary for capability acquisition.

---

## Failover

### ProviderCooldownState → ProviderCircuitRuntime

The historical `ProviderCooldownState` (`cooling_until` + `last_error`) is replaced by `ProviderCircuitRuntime` (line 1145), which wraps a `ProviderCircuitState` (line 1132: `Closed` / `Open { until, backoff_level }` / `HalfOpen { probe_in_flight }`), the `half_open_backoff_level` copied out of `Open`, and a bounded `last_error` (an `AttemptErrorClass` name only — never an `AppError` or body).

### ProviderMetrics

`Line 333`, struct `ProviderMetrics`

Five atomics behind an `Arc`: `served_count`, `failover_count`, `consecutive_failures`, `retry_attempt_count`, `retry_exhausted_count`. Lock-free reads. `consecutive_failures` is reset to 0 on `record_success` (line 349); `record_health_failure` (line 357) bumps it; `record_retry_attempt`/`record_retry_exhausted` (lines 365/369) count same-provider retries.

### FailoverUpstreamProvider

`Line 781`, struct `FailoverUpstreamProvider`

Wraps one `ReqwestUpstreamClient` with its `upstream_model` rewrite, `exposed_model` alias, `upstream_chat_kwargs`, `Arc<ProviderMetrics>`, and an optional `UpstreamResilienceConfig`.

| Method | Line | Description |
|-|-|-|
| `fn new` | 799 | Construct from name, client, model rewrites, kwargs. |
| `fn with_resilience` | 817 | Install the resolved retry/circuit/bulkhead policy for this provider. |

### FailoverUpstreamClient

`Line 824`, struct `FailoverUpstreamClient`

List of `FailoverUpstreamProvider`s plus `Arc<Mutex<Vec<ProviderCircuitRuntime>>>` (one circuit state per provider), `Arc<Vec<ProviderBulkhead>>`, and a shared `Arc<dyn JitterSource>`.

| Method | Line | Description |
|-|-|-|
| `fn new` | 2909 | Initialize providers, per-provider circuit states, bulkheads from each provider's resolved resilience policy, and `SystemJitter`. Providers without an explicit policy get `legacy_resilience(cooldown)`. |
| `fn legacy_resilience` | 2937 | Compatibility policy from a legacy cooldown `Duration`: retry disabled, circuit with `initial_open_ms = max_open_ms = cooldown_ms`, single half-open probe, no bulkhead. |
| `fn provider_resilience` | 2950 | The resolved `UpstreamResilienceConfig` for a provider index. |
| `fn with_jitter_source` | 2958 | Test-only jitter injection. |
| `fn provider_upstream_model` | 2966 | The `upstream_model` rewrite for a given provider index, if any. |
| `fn responses_capability_candidate` | 2972 | Capability candidate (provider + effective model + capabilities) for one index. |
| `fn available_provider_indices` | 2991 | Indices whose circuit is Closed, whose Open deadline expired, or whose HalfOpen probe is free. Grabs the `Mutex`. |
| `fn available_provider_indices_for_request` | 3011 | All indices filtered by capability-allowlist compatibility. |
| `fn available_provider_indices_for_responses` | 3020 | All indices filtered by Responses-wire + capability compatibility. |
| `fn provider_is_responses_compatible` | 3032 | Provider is Responses-native and allowed by the allowlist. |
| `fn provider_is_capability_compatible` | 3044 | Provider+model passes the capability allowlist. |
| `fn provider_is_available` | 3058 | Single-provider circuit availability check. |
| `fn provider_health_with_route` | 3084 | Build `Vec<ProviderHealth>` for the chain, stamping `route` and `catalog_meta`. Snapshots circuit states under one short lock hold. |
| `fn cooldown_error` | 3149 | Build the "all providers unavailable" error with the minimum remaining `retry_after` across Open circuits and busy half-open probes. |
| `fn temporary_unavailability_error` | 3171 | Aggregate error across circuit-unavailable and bulkhead-rejected providers (minimum `retry_after`). |
| `fn acquire_circuit` | 3184 | Acquire a `CircuitPermit` for one provider (see Resilience section). |
| `fn circuit_open_interval` | 3254 | Open duration for a backoff level: `initial_open_ms << level`, capped at `max_open_ms`. |
| `fn mark_provider_success_with_permit` | 3266 | Close the circuit, clear state, reset consecutive failures, bump served count. |
| `fn close_circuit_without_served_turn` | 3291 | Close the circuit after a request-shaped (non-provider) rejection proved reachability — a half-open probe must not strand the circuit. |
| `fn mark_failure_with_permit` | 3314 | Record a provider failure: escalate backoff level (half-open failure escalates), transition Closed→Open or re-Open, record `last_error`, bump failure counters, optionally count `retry_exhausted`. |
| `fn mark_failure` | 3377 | Test-only wrapper around `mark_failure_with_permit`. |
| `fn mark_provider_success` | 3388 | Test-only wrapper around `mark_provider_success_with_permit`. |
| `fn next_retry_delay` | 3398 | Same-provider retry delay for an error, or `None` (see Resilience section). |
| `fn retry_status_class` | 3438 | Bounded logging class: `http_408` / `http_429` / `http_5xx` / `other`. |
| `fn request_for_provider` | 3450 | Clone the `BackendChatRequest` and apply the provider's `upstream_model` rewrite + `upstream_chat_kwargs`. |
| `fn responses_request_for_provider` | 3475 | Same for a `BackendResponsesRequest`. |
| `fn prefetch_first_chunk` | 3501 | Race the first SSE chunk against `request_timeout`. See dedicated section below. |
| `fn stream_after_prefetch` | 3532 | Wrap the remaining stream: per-chunk timeout, hold the bulkhead permit for the stream's life, mark mid-stream failure on error/timeout. |
| `fn mark_midstream_failure` | 3593 | Static: after output began, open the circuit at backoff level 0 (if Closed and policy non-zero) and bump counters. Runs inside the spawned stream, not through `&self`. |
| `fn stream_responses_with_timeout_from_provider` | 3633 | Native-Responses dispatch to a single known-good index (routing fallback targets). |
| `fn stream_responses_with_provider_indices` | 3657 | Native-Responses failover loop with circuit/bulkhead gates and same-provider retry. |
| `fn stream_chat_completion_with_timeout_from_provider` | 3859 | Dispatch to a single known-good index (used by routing fallback targets). |
| `fn stream_chat_completion_with_provider_indices` | 3887 | Core failover loop (see below). |
| `fn take_attempt_header_byte` | 4113 | Read the wire-header-byte time off the shared `ServingToken`. |
| `fn take_attempt_header_byte_offset` | 4120 | Same, as an offset from attempt start. |
| `fn record_attempt` | 4138 | Push one `Attempt` onto the flow's serving token. |
| `fn count_tokens_from_provider` | 4188 | Token counting to a single provider. |
| `fn count_tokens_with_provider_indices` | 4202 | Token counting across providers (first success wins). |
| `fn proxy_completions_from_provider` | 4223 | Raw `/v1/completions` proxy to one provider. |
| `fn proxy_completions_with_provider_indices` | 4241 | Proxy completions with failover across indices (circuit-gated at line 4251; no same-provider retry). |

**Failover loop** (`stream_chat_completion_with_provider_indices`, lines 3887–4107):

1. For each provider index in `provider_indices`:
   - Build the provider-specific request via `request_for_provider` (model rewrite + kwargs).
   - **Circuit gate**: `acquire_circuit` (line 3900). Unavailable (Open, or half-open probe busy) → record the reason, `continue`.
   - **Bulkhead gate**: `bulkheads[index].acquire()` (line 3913). Rejected (in-flight cap + queue full/timeout) → record `retry_after`, `continue`.
   - **Same-provider retry loop** (lines 3940–3994): up to `retry.max_attempts` sends of the exact same finalized request, sleeping `next_retry_delay` between them. Each on-wire attempt re-arms the scratch capture/header state. A successful dispatch breaks out to `prefetch_first_chunk`.
   - On success → `prefetch_first_chunk`:
     - **First chunk arrives** → `mark_provider_success_with_permit`, tag serving provider, clear staged body, return the stream (wrapped via `stream_after_prefetch`, which holds the bulkhead permit for the stream's life and applies mid-stream timeouts).
     - **Error/empty/timeout** → fall into the failure arm.
   - On failure → `mark_failure_with_permit` if the disposition is `Failover` (opens/re-opens the circuit), else `close_circuit_without_served_turn` (request-shaped rejection proves reachability). Then record the attempt and, by disposition:
     - **Terminal** → return the error immediately (no cooldown, no failover).
     - **FailoverNoCooldown** / **Failover** → defer `record_failover` to the next iteration's dispatch seam, set `last_error`, `continue`.
2. If all providers exhausted, return `last_error`, or `temporary_unavailability_error` when every provider was skipped at the circuit/bulkhead gates.

---

## Resilience: Same-Provider Retry, Circuit Breaker, Bulkhead

A per-provider resilience policy (retry + circuit breaker + bulkhead) layers onto the failover loop. Policy types live in `src/config.rs`; runtime state lives in `src/upstream.rs`.

### Policy configuration (`src/config.rs`)

| Type | Line | Fields / defaults |
|-|-|-|
| `UpstreamResilienceConfig` | 932 | Composite: `retry`, `circuit_breaker`, `bulkhead`. Global defaults with sparse per-provider overlays applied via `apply_overrides` (line 939), which re-validates the resolved policy. |
| `UpstreamRetryConfig` | 648 | `enabled` (default `true`), `max_attempts` (3, validated 1–10), `initial_backoff_ms` (500), `max_backoff_ms` (4 000), `total_budget_ms` (10 000, validated 1–60 000), `honor_retry_after` (true), `max_retry_after_secs` (15, ≤ 60). `legacy_disabled()` (line 683) is the direct-construction compatibility policy. |
| `UpstreamRetryOverride` | 738 | Sparse per-provider overlay; missing fields inherit the global policy. |
| `UpstreamCircuitBreakerConfig` | 759 | `initial_open_ms` (2 000), `max_open_ms` (30 000, ≤ 300 000), `half_open_max_probes` (must be 1 — single-probe semantics). `max_open_ms == 0` is the compatibility spelling for a disabled legacy cooldown. `from_legacy_cooldown_secs` (line 779) maps the old `upstream_failure_cooldown_secs`. |
| `UpstreamCircuitBreakerOverride` | 829 | Sparse per-provider overlay. |
| `UpstreamBulkheadConfig` | 843 | `max_in_flight` (`None` = unlimited), `max_queue` (`None` means zero-length queue when a limit is set — bounded rejection, never an unbounded waiter list), `queue_timeout_ms` (5 000). |
| `UpstreamBulkheadOverride` | 906 | Sparse overlay; nested `Option<Option<usize>>` distinguishes an omitted field (inherit) from an explicit YAML `null` (clear a global limit for this provider). |

Wiring: the DI root resolves one policy per provider and installs it via `FailoverUpstreamProvider::with_resilience` (line 817), `RoutingUpstreamProvider::new_with_resilience` (line 860), or `RouteUpstreamProvider::new_with_resilience` (line 950). Providers constructed without a policy fall back to `FailoverUpstreamClient::legacy_resilience(cooldown)` (line 2937): retries disabled, fixed cooldown-length circuit, no bulkhead — the pre-resilience behavior.

### Error taxonomy (`src/error.rs`)

Retry and circuit policy never parse error messages. Each upstream-failure `AppError` carries an internal, never-serialized `UpstreamFailureMetadata` (line 61): `original_status`, `retry_class`, `retry_after`, `response_headers_received`, `upstream_chunk_accepted`, and `safe_same_provider_retry`. `UpstreamRetryClass` (line 47) classifies the failure; `FailoverDisposition` (line 32) — `Failover` / `FailoverNoCooldown` / `Terminal` — decides the failover layer's move. `UpstreamFailureMetadata::http` (line 71) marks 408/429/500/502/503/504 as `safe_same_provider_retry = true`; request-intrinsic 400/413/415/422, authentication, transport, and stream failures are not same-provider-retryable. `parse_retry_after` (upstream.rs line 5569, via `parse_retry_after_at` 5573) parses only the two standardized `Retry-After` spellings (seconds or HTTP-date); the raw header is never retained.

### Same-provider retry

`next_retry_delay` (line 3398) returns `Some(delay)` only when ALL of:

- retry is `enabled` and `completed_attempts < max_attempts`;
- the error is not `Terminal`;
- the error metadata says `safe_same_provider_retry` (explicit transient HTTP statuses only — transport and stream failures are never repeated on the same provider);
- no upstream chunk was accepted (`upstream_chunk_accepted` is false);
- the cumulative sleep stays inside `total_budget_ms`.

The delay is exponential with full jitter: `cap = min(initial_backoff_ms << (attempts−1), max_backoff_ms)`, jittered via `JitterSource::full_jitter`, then `max(jitter, retry_after)` — when `honor_retry_after` and the upstream sent one — clamped to `max_retry_after_secs`, and finally clamped to the remaining budget. Attempts sleep inline in the failover loop (lines 3970–3989) and reuse the exact same finalized request. `retry_status_class` (line 3438) emits a bounded log class (`http_408`/`http_429`/`http_5xx`/`other`); `ProviderMetrics::record_retry_attempt` counts each retry and `record_retry_exhausted` counts a policy-quitter (set in the failover loop at lines 4043–4046 and passed into `mark_failure_with_permit`).

`JitterSource` (line 1154) is the injectable jitter trait; `SystemJitter` (line 1159) is the xorshift-based production implementation (atomic state, seeded from the wall clock).

### Circuit breaker

State machine per provider (`ProviderCircuitState`, line 1132): **Closed** → **Open** → **HalfOpen** → Closed or Open again.

- `acquire_circuit` (line 3184): under one lock hold — Closed grants a `Closed` permit; an expired Open transitions to HalfOpen with `probe_in_flight = true` and grants a `HalfOpen` permit carrying the level; an unexpired Open refuses with `retry_after`; a busy HalfOpen probe refuses (single-probe semantics — `half_open_max_probes` is pinned to 1). A `max_open_ms == 0` policy always grants a `Closed` permit (compatibility mode).
- `CircuitPermit` (line 1280): ownership token. Its `Drop` (line 1297) releases a half-open probe that was never completed, so a cancelled/panicking caller cannot strand the circuit in `HalfOpen { probe_in_flight: true }` forever.
- `circuit_open_interval` (line 3254): escalating open window — `initial_open_ms << level`, saturating, capped at `max_open_ms`.
- `mark_failure_with_permit` (line 3314): a Closed-state failure opens at level 0; a HalfOpen (probe) failure re-opens at `level + 1`, so each failed probe doubles the next open window up to the cap. Only the permit owner (half-open) or a Closed circuit owns the transition. A zero interval (disabled policy) keeps the state Closed.
- `mark_provider_success_with_permit` (line 3266): first chunk served → circuit Closed, `half_open_backoff_level` reset, `last_error` cleared, metrics success. A successful half-open probe logs the `half_open_to_closed` transition.
- `close_circuit_without_served_turn` (line 3291): a request-shaped rejection (e.g. a 400) during a half-open probe proves the provider is reachable — the circuit closes without counting a provider failure.
- `mark_midstream_failure` (line 3593): a failure after output began opens a Closed circuit at level 0 (the response already started, so no failover is possible — only the next request sees the open circuit).

Availability (`available_provider_indices`, line 2991): Closed is available; Open is available once `until` passes; HalfOpen is available when no probe is in flight. `cooldown_error` (line 3149) and `temporary_unavailability_error` (line 3171) surface the minimum remaining wait as a sanitized `Retry-After`.

### Bulkhead

`ProviderBulkhead` (line 1206): optional `Semaphore` (`max_in_flight`), a bounded queue (`max_queue`, default 0 = immediate rejection), and `queue_timeout`. `acquire` (line 1225) tries an immediate permit, else reserves a queue slot via a CAS loop (`queued` atomic; `QueueSlot` at line 1260 decrements on drop) and awaits a permit under `queue_timeout`. Rejections carry a `retry_after` (`BulkheadUnavailable`, line 1269) that feeds the aggregate unavailability error. The permit is held for the whole response stream (`stream_after_prefetch` takes ownership, line 3538), so in-flight counts reflect live streams, not just header arrival.

### Interaction with failover

Per provider attempt, the gates apply in order: circuit permit → bulkhead permit → same-provider retry loop (bounded by attempts AND budget) → prefetch first chunk → failover disposition. Same-provider retry is deliberately narrower than failover: only explicit transient HTTP statuses repeat on the same provider, and only before any output. Everything else advances to the next provider (or terminates). The native-Responses loop (`stream_responses_with_provider_indices`, line 3657) and the raw proxy path (`proxy_completions_with_provider_indices`, line 4241) are circuit/bulkhead-gated the same way; the proxy path does not same-provider-retry.

---

## Routing

### RoutingUpstreamProvider

`Line 832`, struct `RoutingUpstreamProvider`

One routing provider: its `primary_client` (the `ReqwestUpstreamClient`), `primary_upstream_model`, `fallback_exposed_models` (alias→failover-provider-index mappings), and a nested `FailoverUpstreamClient` that wraps primary + fallbacks.

| Method | Line | Description |
|-|-|-|
| `fn new` | 841 | Legacy constructor from a cooldown `Duration`. |
| `fn new_with_resilience` | 860 | Constructor with a resolved `UpstreamResilienceConfig` for the primary. |
| `fn new_with_optional_resilience` | 880 | Shared body: primary becomes index 0 of the failover chain; fallbacks appended; extracts `fallback_exposed_models`. |
| `fn failover_provider_model` | 929 | Effective backend model of a nested failover provider by index (for G4 native-vision gating). |

### RouteUpstreamProvider

`Line 940`, struct `RouteUpstreamProvider`

Synthetic single-model upstream for ad-hoc routes (G7). Wraps one `FailoverUpstreamClient` with a single `FailoverUpstreamProvider`. Never enumerated in `/v1/models`. Constructors: `new` (line 946, legacy cooldown) and `new_with_resilience` (line 950).

### ModelRouteSpec

`Line 980`, struct `ModelRouteSpec`

A compiled ad-hoc route: `name` (literal or glob), compiled `Regex`, index into `route_providers`, optional upstream model rewrite.

### RoutingModelCatalog

`Line 1041`, struct `RoutingModelCatalog`

Union of all provider `/v1/models` catalogs + route specs. Built by `refresh_catalog`. Carries `union_context_limit_by_id` (per-model context windows from the same snapshot), `ids_by_key` canonical-key index, `routes`, and stale-on-error state: `provider_entries` (last-known-good rows per provider, served when a live fetch fails) and `stale_providers` (names whose rows are stale — a non-empty list marks the catalog degraded).

| Method | Line | Description |
|-|-|-|
| `fn resolve` | 4589 | Resolve request model → `(RoutingResolution, MatchKind)`. Precedence: exact id, exact route name, glob route, canonical-key match, default (first catalog model). |
| `fn match_route` | 4646 | Match against ad-hoc routes: exact (case-insensitive) beats any glob; first glob declared wins. |
| `fn default_candidate` | 4662 | First model of the first non-empty provider catalog. |
| `fn union_body` | 4674 | Build `/v1/models`-style JSON response from `union_entries`. |

### RoutingUpstreamClient

`Line 1012`, struct `RoutingUpstreamClient`

Owns a `Vec<RoutingUpstreamProvider>`, `Vec<RouteUpstreamProvider>`, `Vec<ModelRouteSpec>`, a cached `RoutingModelCatalog` behind an `AsyncMutex` (plus a `catalog_refresh` single-flight lock), and `Arc<Mutex<Arc<CatalogMeta>>>` for lock-free health reads.

| Method | Line | Description |
|-|-|-|
| `fn new` | 4297 | Construct without routes (delegates to `with_routes`). |
| `fn with_routes` | 4304 | Construct with ad-hoc routes. |
| `fn route_provider` | 4321 | Look up a synthetic route provider by index. |
| `fn routed_request` | 4331 | Clone request and apply upstream-model rewrite, preserving `client_chat_template_kwargs`/`serving`/`capture`. |
| `fn routed_responses_request` | 4359 | Same for a native `BackendResponsesRequest`. |
| `fn load_catalog` | 4387 | Return the cached catalog if fresh, else single-flight `refresh_catalog()`. No catalog mutex is held across provider network requests. |
| `fn fresh_cached_catalog` | 4409 | Cached-candidate check. Clean catalog TTL 300 s (`ROUTING_MODEL_CATALOG_TTL_SECS`, line 517); degraded (stale-on-error) TTL 30 s (`ROUTING_MODEL_CATALOG_DEGRADED_TTL_SECS`, line 522). |
| `fn refresh_catalog` | 4428 | Fetch `/v1/models` from each routing provider's primary client, serve stored entries for failed fetches, build the union catalog, register fallback exposed models, set `catalog_meta`. |

Trait implementation methods (in `impl UpstreamClient for RoutingUpstreamClient`, line 5020): `wire_api_for_model` 5021, `stream_responses_native_with_timeout` 5065, `stream_chat_completion` 5126, `backend_candidate_plan` 5151, `responses_capability_plan` 5229, `provider_health` 5289, `stream_chat_completion_with_timeout` 5312, `count_tokens` 5387, `list_models` 5430, `proxy_metrics` 5435, `backend_metrics_targets` 5447, `proxy_completions` 5464, `supported_model_catalog` 5517.

**Routing dispatch** (`stream_chat_completion_with_timeout`, lines 5312–5385):

1. Load catalog (with TTL cache; degraded catalogs expire early).
2. Resolve `request.model` through `catalog.resolve()`.
3. If **Route** resolution → dispatch to `RouteUpstreamProvider`'s nested `FailoverUpstreamClient`.
4. If **Catalog** resolution with `RoutingModelTarget::Primary` → dispatch to the full nested `FailoverUpstreamClient`.
5. If **Catalog** resolution with `RoutingModelTarget::Fallback` → dispatch to a specific provider inside the nested failover chain via `stream_chat_completion_with_timeout_from_provider`.

The routing layer tags `route` on the serving token before dispatching (first-writer-wins); the nested failover layer owns `provider`.

---

## Request Finalization (finalize_request_for_backend)

`Line 6276`, `pub fn finalize_request_for_backend`

Called by the leaf (`ReqwestUpstreamClient::stream_chat_completion`, line 2674) and by `count_tokens` (line 2799). Applied to `BackendChatRequest` with the FINAL `request.model` after all routing/failover/exposed-alias remaps.

Steps in order:

1. **upstream_chat_kwargs** (line 6288): `merge_chat_kwargs_gap_fill` with per-model kwargs resolved from `BackendFinalizationPolicies::resolve_chat_kwargs` (global base + per-model). Gap-fill, request-wins. This replaces the engine's pre-routing defaults: the leaf resolves kwargs against the FINAL model, so a routed/failover cross-family target gets its OWN kwargs.

2. **Reasoning effort** (lines 6290–6317):
   - If an `effort` policy has an explicit `reasoning_effort_fragment` → set `request.reasoning_effort = None`, apply the fragment at step 4.
   - Else if the policy has `upstream_reasoning` → preserve the raw effort (it will be mapped upstream).
   - Else → `clamp_reasoning_effort` (none/low pass through, everything else → high).
   - `thinking_override` resolved: disabled if effort is "none" or the fragment disables thinking.

3. **Family chat_template_kwargs** (line 6318): `apply_family_chat_template_kwargs` — inject family-specific kwargs from `template_family` override or model-id sniffing.

4. **Effort fragment** (line 6321): `apply_reasoning_effort_fragment` — deep-merge the fragment into `extra_body` (overrides configured/family defaults), then re-assert client `chat_template_kwargs` so client still wins.

5. **Profile thinking kwarg** (line 6328): `apply_profile_thinking_kwarg` — set `chat_template_kwargs.enable_thinking` based on `thinking_override`. Custom param name/value from `upstream_reasoning` config if set; Kimi skips (always enabled).

**Supporting functions:**

| Function | Line | Description |
|-|-|-|
| `fragment_disables_thinking` | 6331 | True if fragment sets `reasoning_effort: "none"` or `chat_template_kwargs.{enable_thinking,thinking}: false`. |
| `apply_profile_thinking_kwarg` | 6345 | Set thinking kwarg in `chat_template_kwargs` from policy config or default `enable_thinking`. |
| `merge_chat_kwargs_gap_fill` | 6394 | Merge defaults into `extra_body` with request-wins semantics. Skip max-token aliases if client expressed any. |
| `clamp_reasoning_effort` | 6434 | Default clamp for mapless backends: none/low pass through, everything else → high. |
| `reasoning_effort_fragment` | 6448 | Resolve effort fragment from per-model map: exact, then canonical-key, then `"*"` wildcard. |
| `apply_reasoning_effort_fragment` | 6473 | Deep-merge fragment into `extra_body` (prefer-source over family/config), re-assert client kwargs. |
| `apply_family_chat_template_kwargs` | 6511 | Inject `chat_template_kwargs` from family template (Anthropic/DeepSeek/Kimi/OpenAI). |

---

## Cooldown and provider status

The legacy fixed-length cooldown is now the circuit breaker's Open state; "cooling" is derived from it.

### Downs vs. Cooling

`Line 59`, const `DOWN_THRESHOLD = 3`

`provider_health_with_route` (line 3084) derives status from the circuit state:

- Circuit `Closed` → status `Healthy`.
- Circuit `Open` or `HalfOpen` → status `Cooling` when `consecutive_failures < 3`, `Down` when `consecutive_failures >= DOWN_THRESHOLD` (3).

`cooling_until_ms` surfaces the epoch-ms Open deadline only while it is actually in the future (line 3125).

### Legacy cooldown mapping

`FailoverUpstreamClient::new(providers, cooldown)` still accepts a `Duration`; `legacy_resilience` (line 2937) converts it to a circuit with `initial_open_ms = max_open_ms = cooldown_ms` and `half_open_max_probes = 1` — one fixed-length window, single probe, retry disabled. `max_open_ms == 0` disables the circuit entirely (a `Closed` permit is always granted; `mark_failure_with_permit` leaves the state Closed but still records `last_error` and bumps counters).

### Lifecycle

| Event | What happens |
|-|-|
| Provider attempt fails (Failover disposition, after retry exhaustion) | `mark_failure_with_permit` opens the circuit for `circuit_open_interval(policy, level)`, records `last_error`, bumps `consecutive_failures`; `retry_exhausted` also bumps `retry_exhausted_count`. |
| Open window expires, next request arrives | `acquire_circuit` transitions Open→HalfOpen and grants the single probe permit. |
| Half-open probe fails | Circuit re-opens at `backoff_level + 1` (escalating window). |
| Provider succeeds (first chunk arrives) | `mark_provider_success_with_permit` closes the circuit, clears `last_error`, resets `consecutive_failures` to 0, bumps `served_count`. |
| Request-shaped rejection during a half-open probe | `close_circuit_without_served_turn` — reachable, so Closed, without counting a failure. |
| Mid-stream failure (after output began) | `mark_midstream_failure` opens a Closed circuit at level 0. |

---

## Prefetch (failover race, NOT availability warming)

`Line 3501`, `fn prefetch_first_chunk`

```rust
async fn prefetch_first_chunk(
    mut stream: UpstreamStream,
    request_timeout: Duration,
) -> AppResult<(ChatCompletionChunk, UpstreamStream)>
```

Called by the chat failover loop (line 3960) **after** the leaf returned a successful HTTP response (2xx, SSE stream established). The native-Responses loop inlines the same first-event race (lines 3713–3731) rather than calling this function.

What it does:

1. Awaits `stream.next()` with a `request_timeout`.
2. If the first chunk arrives within timeout → return `Ok((chunk, remaining_stream))`.
3. If the stream ends empty → error "upstream stream ended before the first chunk".
4. If the first chunk is an error (`Some(Err(...))`) → propagate that error, tagging `UpstreamFailureMetadata::stream` when it carries none.
5. If the stream times out (no chunk within `request_timeout`) → error "upstream stream timed out".

This is **not** availability warming. It is a **failover race**: the failover loop considers a provider "available" based solely on its circuit state (lines 2991–3009). The loop tries available providers in order, dispatches to each, and only declares a winner after `prefetch_first_chunk` returns a valid `ChatCompletionChunk`. If the first chunk fails (timeout, stream-ends, parse error), the failover loop marks that provider failed and tries the next one. This prevents a provider that establishes an HTTP connection but produces no usable first chunk from holding up the response.

The first-chunk race is the **sole** point at which the failover loop can continue past a 2xx response — all earlier failures (connect, non-2xx status) are handled by the leaf's `dispatch_chat_stream` and the error dispositions in the failover loop. Prefetch failures are stream-class, never same-provider-retryable, so they advance straight to the next provider.

---

## Health Checking

### ProviderHealth

`Line 295`, struct `ProviderHealth`

Serializable DTO with: `id`, `name`, `route`, `base_url`, `status` (Healthy/Cooling/Down), `cooling_until_ms`, `last_error`, `served_count`, `failover_count`, `consecutive_failures`, `catalog_fetched_ms`, `catalog_size`.

All fields serialize unconditionally (no `skip_serializing_if`) — frontend D9/D10/D12 model validates this exact shape.

### ProviderHealthSnapshot

`Line 414`, struct `ProviderHealthSnapshot`

Immutable versioned container: `version: u64` + `providers: Vec<ProviderHealth>`. Built inside `publish()`.

### ProviderHealthPublisher

`Line 432`, struct `ProviderHealthPublisher`

Holds `Mutex<Arc<ProviderHealthSnapshot>>` + atomic version counter.

- `hydrate_sequence(floor)` (line 448): restore the persisted topology watermark before the initial publication.
- `publish(providers)` (line 456): bump version, build new `Arc<ProviderHealthSnapshot>`, atomically swap into the `Mutex`.
- `latest()` (line 467): cheap `Arc::clone` of the current snapshot.

Published on a coalesced 1 s tick AND a cooldown-deadline wake (so an idle `Cooling → Healthy` transition flips without traffic).

### Health data sources

| Layer | Method | Line | What it reports |
|-|-|-|-|
| `FailoverUpstreamClient` | `provider_health_with_route` | 3084 | Per-provider status from circuit states + `ProviderMetrics` atomics. Snapshot circuit states under one short lock hold. |
| `FailoverUpstreamClient` | `provider_health` (trait) | 5014 | Bare chain: `provider_health_with_route(None, default)`. |
| `RoutingUpstreamClient` | `provider_health` (trait) | 5289 | Aggregates every routing provider's failover chain (stamped with `route` + catalog meta) + every route provider's chain. |

Health is **derived from circuit state + metrics atomics** — there is no active health-check polling (no pings, no probes). A provider's status reflects its last dispatch outcome: a failure opens the circuit; a success closes it. Between dispatch events the status remains at whatever the last outcome left it (idle providers stay Healthy until the next failure).
