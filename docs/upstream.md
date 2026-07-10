# Upstream Client

## Component Overview

| Component | Line | Purpose |
|-|-|-|
| `UpstreamClient` trait | 407 | Async interface: `stream_chat_completion`, `count_tokens`, `list_models`, `proxy_completions`, `backend_candidate_plan`, `provider_health`, `supported_model_catalog` |
| `ReqwestUpstreamClient` | 503 | Leaf HTTP client — one `reqwest::Client` per upstream base_url. POSTs finalized+sanitized requests, handles G1 shrink-and-retry, captures D2/F1d on-wire bodies. Implements `UpstreamClient` at line 1496. |
| `FailoverUpstreamClient` | 575 | Provider-level failover across a list of `FailoverUpstreamProvider`s. Tries each in order; marks failures with cooldown; wraps first-chunk race via `prefetch_first_chunk`. Implements `UpstreamClient` at line 2776. |
| `RoutingUpstreamClient` | 705 | Model-routing layer: loads `/v1/models` catalog from each primary provider, resolves request model → provider+target, dispatches to the nested `FailoverUpstreamClient`. Implements `UpstreamClient` at line 2879. |
| `ServingToken` | 3428 | Interior-mutable shared identity per flow. Routing sets `route`; failover sets `provider`; leaf sets `model_served_final`. Carries per-attempt trace (`attempts`), scratch `attempt_header_byte_ms`, staged `pending_response_body`. |
| `ProviderHealth` | 209 | Serializable per-upstream health DTO (status, cooldown deadline, last error, counters, catalog meta). |
| `ProviderHealthSnapshot` | 301 | Versioned, immutable snapshot container. Published atomically via `ProviderHealthPublisher` on a coalesced 1 s tick. |
| `ProviderHealthPublisher` | 319 | `Mutex<Arc<ProviderHealthSnapshot>>` + monotonic version counter. `publish()` swaps in a new snapshot; `latest()` cheap-clones the `Arc`. |
| `BackendFinalizationPolicies` | 3268 | Per-model-effort map, `template_family` override, `upstream_chat_kwargs` (global base + per-model). Built once from config, shared via `Arc`. Applied at the leaf. |
| `BackendChatRequest` | 3646 | DTO carrying the request + `response_id`, `serving` token, `capture` handle, `thinking_override`. Cloned across failover/route rebuilds. |
| `finalize_request_for_backend` | 3786 | Leaf request finalization: resolve per-model kwargs, reasoning effort (map→fragment or clamp), family `chat_template_kwargs`, effort fragment, profile thinking kwarg. |

---

## ReqwestUpstreamClient

`Line 503`, struct `ReqwestUpstreamClient`

The HTTP leaf. One instance per upstream `base_url` + `api_key`. Owns the `reqwest::Client`, request logger, SSE frame cap, and D2/F1d capture handles.

| Method | Line | Description |
|-|-|-|
| `fn new` | 945 | Construct with defaults (delegates to `with_options`). |
| `fn with_options` | 968 | Full constructor: `client`, `base_url`, `api_key`, `request_log_path`, `flatten_content`, `min_completion_tokens`, `max_sse_frame_bytes`. |
| `fn with_finalization_policies` | 999 | Attach per-model finalization policies post-construction. |
| `fn with_flow_store` | 1012 | Attach dashboard FlowStore handle (D2 capture seam). |
| `fn into_bare_primary` | 1025 | Mark this leaf as the direct engine upstream (synthesizes `provider = "primary"`). |
| `fn base_url_string` | 1033 | The configured upstream base URL, for D4 `ProviderHealth`. |
| `fn with_auth` | 1037 | Attach Bearer auth to a request builder if `api_key` is set. |
| `fn endpoint_url` | 1044 | Join a path onto `base_url` (handles trailing-slash normalization). |
| `fn tokenize_url` | 1056 | Build `/tokenize` URL (strips `/v1` prefix). |
| `fn send_chat_request` | 1076 | POST JSON to upstream URL with auth. No logging, no capture. |
| `fn logged_send_chat_request` | 1111 | Log to JSONL (if enabled), capture D2/F1d on-wire body, then POST. |
| `fn capture_upstream_body` | 1154 | D2: store the sanitized on-wire request body into the FlowStore record via the capped + redacting serializer. |
| `fn capture_upstream_response_body` | 1198 | Gap-05/F1e: stage a failed-attempt's error body onto the `ServingToken` (capped + redacted). |
| `fn record_served_attempt_on_first_byte` | 1240 | Gap-03 bare-leaf path: wrap stream to record exactly one attempt at first-chunk yield. |
| `fn failed_bare_attempt` | 1305 | Build a FAILED `Attempt` with bounded taxonomic error codes. |
| `fn dispatch_chat_stream` | 1337 | Core leaf dispatch: POST, handle G1 context-overflow shrink-and-retry, parse SSE stream. Resets per-attempt capture state. |
| `fn stream_chat_completion` (trait impl) | 1497 | Trait entry: finalize via `finalize_request_for_backend`, sanitize, dispatch. Handles bare-leaf attempt recording. |
| `fn count_tokens` (trait impl) | 1627 | POST to `/tokenize` with messages + model. |
| `fn list_models` (trait impl) | 1681 | GET `/v1/models`. |
| `fn proxy_completions` (trait impl) | 1699 | Raw `/v1/completions` proxy passthrough (no finalization). |

**Key invariants:**
- The leaf is the single point that sees the FINAL provider-model after routing/failover/exposed-alias remap — so it applies `finalization_policies` keyed by `request.model` at line 1511.
- G1 shrink-and-retry (lines 1382–1466) happens INSIDE `dispatch_chat_stream` BEFORE any SSE chunk is parsed — the routing/failover layers never see a context-limit error as a provider failure.
- D2 bare-leaf marker (`tag_primary_provider` at line 538) prevents a leaf nested inside failover/routing from clobbering the real provider name.

---

## Failover

### ProviderCooldownState

`Line 808`, struct `ProviderCooldownState`

```rust
struct ProviderCooldownState {
    cooling_until: Option<Instant>,
    last_error: Option<String>,
}
```

### ProviderMetrics

`Line 245`, struct `ProviderMetrics`

Three atomics behind an `Arc`: `served_count`, `failover_count`, `consecutive_failures`. Lock-free reads. Cleared to 0 on `record_success`.

### FailoverUpstreamProvider

`Line 542`, struct `FailoverUpstreamProvider`

Wraps one `ReqwestUpstreamClient` with its `upstream_model` rewrite, `exposed_model` alias, `upstream_chat_kwargs`, and `Arc<ProviderMetrics>`.

| Method | Line | Description |
|-|-|-|
| `fn new` | 556 | Construct from name, client, model rewrites, kwargs. |

### FailoverUpstreamClient

`Line 575`, struct `FailoverUpstreamClient`

List of `FailoverUpstreamProvider`s plus cooldown duration and shared `Mutex<Vec<ProviderCooldownState>>`.

| Method | Line | Description |
|-|-|-|
| `fn new` | 1713 | Initialize providers + cooldown states. |
| `fn provider_upstream_model` | 1725 | The `upstream_model` rewrite for a given provider index, if any. |
| `fn available_provider_indices` | 1731 | List of indices whose `cooling_until` has expired. Grabs the `Mutex`. |
| `fn provider_is_available` | 1750 | Single-provider availability check. |
| `fn provider_health_with_route` | 1773 | Build `Vec<ProviderHealth>` for the chain, stamping `route` and `catalog_meta`. Snapshots cooldown states under one short lock hold. |
| `fn cooldown_error` | 1837 | Build the "all providers in cooldown" error with `next_retry_secs`. |
| `fn request_for_provider` | 1860 | Clone the `BackendChatRequest` and apply the provider's `upstream_model` rewrite + `upstream_chat_kwargs`. |
| `fn prefetch_first_chunk` | 1883 | Race the first SSE chunk against `request_timeout`. See dedicated section below. |
| `fn stream_after_prefetch` | 1897 | Wrap the remaining stream to apply per-chunk timeout + mark provider failure on mid-stream error. |
| `fn mark_provider_success` | 1947 | Clear cooldown state, reset consecutive failures, bump served count. |
| `fn mark_provider_failure` (static) | 1965 | Set cooldown deadline, record error, bump failure counters. Cooldown → `Cooling`/`Down` based on `consecutive_failures >= DOWN_THRESHOLD` (3). |
| `fn mark_failure` | 2007 | Instance method delegating to static `mark_provider_failure`. |
| `fn stream_chat_completion_with_timeout_from_provider` | 2018 | Dispatch to a single known-good index (used by routing fallback targets). |
| `fn stream_chat_completion_with_provider_indices` | 2040 | Core failover loop: iterate provider indices, dispatch to leaf, handle terminal/Failover/FailoverNoCooldown dispositions, prefetch first chunk, mark success/failure. Records per-attempt D3 trace. |
| `fn take_attempt_header_byte` | 2215 | Read the wire-header-byte time off the shared `ServingToken`. |
| `fn record_attempt` | 2230 | Push one `Attempt` onto the flow's serving token. |
| `fn count_tokens_from_provider` | 2276 | Token counting to a single provider. |
| `fn count_tokens_with_provider_indices` | 2290 | Token counting across providers (first success wins). |
| `fn proxy_completions_from_provider` | 2311 | Raw `/v1/completions` proxy to one provider. |
| `fn proxy_completions_with_provider_indices` | 2329 | Proxy completions with failover across indices. |
| `fn backend_candidate_plan` | 2855 | Build candidate set: each provider's effective model (with `upstream_model` rewrite), context limits `None`. |
| `fn provider_health` | 2873 | `provider_health_with_route(None, default)` — bare chain, no route stamp. |

**Failover loop** (lines 2040–2209):

1. For each provider index in `provider_indices`:
   - Build the provider-specific request via `request_for_provider` (model rewrite + kwargs).
   - Arm per-attempt header-byte slot + clear pending response body.
   - Call the leaf's `stream_chat_completion`.
   - On return:
     - **Terminal** disposition → return the error immediately (no cooldown, no failover).
     - **FailoverNoCooldown** disposition → record attempt, set `last_error`, `continue`.
     - **Failover** disposition → `mark_failure` (sets cooldown), record attempt, `continue`.
   - On success → `prefetch_first_chunk`:
     - **First chunk arrives** → `mark_provider_success`, tag serving provider, clear staged body, return stream (wrapped via `stream_after_prefetch` for mid-stream timeouts).
     - **Error/empty/timeout** → `mark_failure`, record attempt, `continue`.
2. If all providers exhausted, return `last_error` (or a generic "all failed").

---

## Routing

### RoutingUpstreamProvider

`Line 582`, struct `RoutingUpstreamProvider`

One routing provider: its `primary_client` (the `ReqwestUpstreamClient`), `fallback_exposed_models` (alias->failover-provider-index mappings), and a nested `FailoverUpstreamClient` that wraps primary + fallbacks.

| Method | Line | Description |
|-|-|-|
| `fn new` | 591 | Build the provider: primary client becomes index 0 of the failover chain; fallbacks appended. Extracts `fallback_exposed_models`. |
| `fn failover_provider_model` | 635 | Effective backend model of a nested failover provider by index (for G4 native-vision gating). |

### RouteUpstreamProvider

`Line 646`, struct `RouteUpstreamProvider`

Synthetic single-model upstream for ad-hoc routes (G7). Wraps one `FailoverUpstreamClient` with a single `FailoverUpstreamProvider`. Never enumerated in `/v1/models`.

### ModelRouteSpec

`Line 673`, struct `ModelRouteSpec`

A compiled ad-hoc route: `name` (literal or glob), compiled `Regex`, index into `route_providers`, optional upstream model rewrite.

### RoutingModelCatalog

`Line 729`, struct `RoutingModelCatalog`

Union of all provider `/v1/models` catalogs + route specs. Built by `refresh_catalog`.

| Method | Line | Description |
|-|-|-|
| `fn resolve` | 2555 | Resolve request model → `(RoutingResolution, MatchKind)`. Precedence: exact id, exact route name, glob route, canonical-key match, default (first catalog model). |
| `fn match_route` | 2612 | Match against ad-hoc routes: exact (case-insensitive) beats any glob; first glob declared wins. |
| `fn default_candidate` | 2628 | First model of the first non-empty provider catalog. |
| `fn union_body` | 2640 | Build `/v1/models`-style JSON response from `union_entries`. |

### RoutingUpstreamClient

`Line 705`, struct `RoutingUpstreamClient`

Owns a `Vec<RoutingUpstreamProvider>`, `Vec<RouteUpstreamProvider>`, `Vec<ModelRouteSpec>`, a cached `RoutingModelCatalog` behind `AsyncMutex`, and `Arc<Mutex<Arc<CatalogMeta>>>` for lock-free health reads.

| Method | Line | Description |
|-|-|-|
| `fn new` | 2370 | Construct without routes (delegates to `with_routes`). |
| `fn with_routes` | 2377 | Construct with ad-hoc routes. |
| `fn route_provider` | 2393 | Look up a synthetic route provider by index. |
| `fn routed_request` | 2403 | Clone request and apply upstream-model rewrite, preserving `client_chat_template_kwargs`/`serving`/`capture`. |
| `fn load_catalog` | 2429 | Return cached catalog (TTL 300 s) or `refresh_catalog()`. |
| `fn refresh_catalog` | 2444 | Fetch `/v1/models` from each routing provider's primary client, build union catalog, register fallback exposed models, set `catalog_meta`. |
| `fn stream_chat_completion` (trait) | 2880 | Resolve model → route/catalog, tag `route` on serving token, dispatch to nested failover client. |
| `fn backend_candidate_plan` (trait) | 2905 | Enumerate candidate models from the resolved provider's failover chain (primary uses all nested; fallback uses exactly one). Context limits scoped per-provider (T9 R3). |
| `fn provider_health` | 2991 | Aggregate health across every routing provider's failover chain and every route provider, stamping `route` + `catalog_meta`. |
| `fn count_tokens` (trait) | 3089 | Resolve model → dispatch to nested failover's `count_tokens`. |
| `fn list_models` (trait) | 3132 | Return cached union body as JSON response. |
| `fn proxy_completions` (trait) | 3137 | Resolve model from proxy body, rewrite model, dispatch to nested failover's proxy. |
| `fn supported_model_catalog` | 3190 | Build `Vec<UpstreamModelEntry>` from cached union catalog context limits. |

**Routing dispatch** (lines 3014–3087):
1. Load catalog (with TTL cache).
2. Resolve `request.model` through `catalog.resolve()`.
3. If **Route** resolution → dispatch to `RouteUpstreamProvider`'s nested `FailoverUpstreamClient`.
4. If **Catalog** resolution with `RoutingModelTarget::Primary` → dispatch to the full nested `FailoverUpstreamClient`.
5. If **Catalog** resolution with `RoutingModelTarget::Fallback` → dispatch to a specific provider inside the nested failover chain via `stream_chat_completion_with_timeout_from_provider`.

---

## Request Finalization (finalize_request_for_backend)

`Line 3786`, `pub fn finalize_request_for_backend`

Called by the leaf (`ReqwestUpstreamClient::stream_chat_completion`, line 1511) and by `count_tokens` (line 1629). Applied to `BackendChatRequest` with the FINAL `request.model` after all routing/failover/exposed-alias remaps.

Steps in order:

1. **upstream_chat_kwargs** (line 3798): `merge_chat_kwargs_gap_fill` with per-model kwargs resolved from `BackendFinalizationPolicies::resolve_chat_kwargs` (global base + per-model). Gap-fill, request-wins.

2. **Reasoning effort** (lines 3800–3826):
   - If an `effort` policy has an explicit `reasoning_effort_fragment` → set `request.reasoning_effort = None`, later apply fragment.
   - Else if the policy has `upstream_reasoning` → preserve raw effort (it will be mapped upstream).
   - Else → `clamp_reasoning_effort` (none/low pass through, everything else → high).
   - `thinking_override` resolved: disabled if effort is "none" or the fragment contains a thinking-disabling key.

3. **Family chat_template_kwargs** (line 3828): `apply_family_chat_template_kwargs` — inject family-specific kwargs from `template_family` override or model-id sniffing.

4. **Effort fragment** (line 3830): `apply_reasoning_effort_fragment` — deep-merge the fragment into `extra_body` (overrides configured/family defaults), then re-assert client `chat_template_kwargs` so client still wins.

5. **Profile thinking kwarg** (line 3838): `apply_profile_thinking_kwarg` — set `chat_template_kwargs.enable_thinking` based on `thinking_override`. Custom param name/value from `upstream_reasoning` config if set; Kimi skips (always enabled).

**Supporting functions:**

| Function | Line | Description |
|-|-|-|
| `fragment_disables_thinking` | 3841 | True if fragment sets `reasoning_effort: "none"` or `chat_template_kwargs.{enable_thinking,thinking}: false`. |
| `apply_profile_thinking_kwarg` | 3855 | Set thinking kwarg in `chat_template_kwargs` from policy config or default `enable_thinking`. |
| `merge_chat_kwargs_gap_fill` | 3901 | Merge defaults into `extra_body` with request-wins semantics. Skip max-token aliases if client expressed any. |
| `clamp_reasoning_effort` | 3941 | Default clamp for mapless backends: none/low pass through, everything else → high. |
| `reasoning_effort_fragment` | 3955 | Resolve effort fragment from per-model map: exact, then canonical-key, then `"*"` wildcard. |
| `apply_reasoning_effort_fragment` | 3980 | Deep-merge fragment into `extra_body` (prefer-source over family/config), re-assert client kwargs. |
| `apply_family_chat_template_kwargs` | 4018 | Inject `chat_template_kwargs` from family template (Anthropic/DeepSeek/Kimi/OpenAI). |

---

## Cooldown

### States

`Line 808`, struct `ProviderCooldownState`

Per-provider cooldown state: `cooling_until: Option<Instant>` and `last_error: Option<String>`. Managed via `Mutex<Vec<ProviderCooldownState>>` shared across `FailoverUpstreamClient`.

### Downs vs. Cooling

`Line 48`, const `DOWN_THRESHOLD = 3`

When a provider is inside its cooldown window:
- `consecutive_failures < 3` → status `Cooling` (transient).
- `consecutive_failures >= 3` → status `Down` (hard-down in topology map).

`Line 48` comment: "A single transient failure stays `Cooling`."

### Cooldown Configuration

- `cooldown: Duration` on `FailoverUpstreamClient` (line 577). Default from config, per `FailoverUpstreamClient::new`.
- If `cooldown == Duration::ZERO`, the provider still records the failure but no deadline is set (`cooling_until = None`). The `available_provider_indices` check still works — a `None` deadline is always available.
- `mark_provider_failure` (line 1965): `cooling_until = (cooldown > 0).then(|| Instant::now() + cooldown)`.

### Cooldown Lifecycle

| Event | What happens |
|-|-|
| Provider fails (Failover disposition) | `mark_provider_failure` sets `cooling_until`, records `last_error`, bumps `consecutive_failures` + `failover_count`. |
| `consecutive_failures >= DOWN_THRESHOLD` during cooling | `provider_health_with_route` reports status `Down`. |
| Provider succeeds (first chunk arrives) | `mark_provider_success` clears `cooling_until` and `last_error`, resets `consecutive_failures` to 0, bumps `served_count`. |
| Time passes, `cooling_until <= now` | `available_provider_indices` includes this provider again. |

---

## Prefetch (failover race, NOT availability warming)

`Line 1883`, `fn prefetch_first_chunk`

```rust
async fn prefetch_first_chunk(
    mut stream: UpstreamStream,
    request_timeout: Duration,
) -> AppResult<(ChatCompletionChunk, UpstreamStream)>
```

Called by `FailoverUpstreamClient::stream_chat_completion_with_provider_indices` (line 2143) **after** the leaf returned a successful HTTP response (2xx, SSE stream established).

What it does:

1. Awaits `stream.next()` with a `request_timeout`.
2. If the first chunk arrives within timeout → return `Ok((chunk, remaining_stream))`.
3. If the stream ends empty → error "upstream stream ended before the first chunk".
4. If the first chunk is an error (`Some(Err(...))`) → propagate that error.
5. If the stream times out (no chunk within `request_timeout`) → error "upstream stream timed out".

This is **not** availability warming. It is a **failover race**: the failover loop considers a provider "available" based solely on its cooldown state (lines 1731–1748). The loop tries available providers in order, dispatches to each, and only declares a winner after `prefetch_first_chunk` returns a valid `ChatCompletionChunk`. If the first chunk fails (timeout, stream-ends, parse error), the failover loop marks that provider failed and tries the next one. This prevents a provider that establishes HTTP connection but produces no usable first chunk from holding up the response.

The first-chunk race is the **sole** point at which the failover loop can continue past a 2xx response — all earlier failures (connect, non-2xx status) are handled by the leaf's `dispatch_chat_stream` and the error dispositions in the failover loop.

---

## Health Checking

### ProviderHealth

`Line 209`, struct `ProviderHealth`

Serializable DTO with: `id`, `name`, `route`, `base_url`, `status` (Healthy/Cooling/Down), `cooling_until_ms`, `last_error`, `served_count`, `failover_count`, `consecutive_failures`, `catalog_fetched_ms`, `catalog_size`.

All fields serialize unconditionally (no `skip_serializing_if`) — frontend D9/D10/D12 model validates this exact shape.

### ProviderHealthSnapshot

`Line 301`, struct `ProviderHealthSnapshot`

Immutable versioned container: `version: u64` + `providers: Vec<ProviderHealth>`. Built inside `publish()`.

### ProviderHealthPublisher

`Line 319`, struct `ProviderHealthPublisher`

Holds `Mutex<Arc<ProviderHealthSnapshot>>` + atomic version counter.

- `publish(providers)` (line 338): bump version, build new `Arc<ProviderHealthSnapshot>`, atomically swap into the `Mutex`.
- `latest()` (line 349): cheap `Arc::clone` of the current snapshot.

Published on a coalesced 1 s tick AND a cooldown-deadline wake (so an idle `Cooling → Healthy` transition flips without traffic).

### Health data sources

| Layer | Method | Line | What it reports |
|-|-|-|-|
| `FailoverUpstreamClient` | `provider_health_with_route` | 1773 | Per-provider status from cooldown states + `ProviderMetrics` atomics. Snapshot cooldown states under one short lock hold. |
| `FailoverUpstreamClient` | `provider_health` | 2873 | Bare chain: `provider_health_with_route(None, default)`. |
| `RoutingUpstreamClient` | `provider_health` | 2991 | Aggregates every routing provider's failover chain (stamped with `route` + catalog meta) + every route provider's chain. |

Health is **derived from cooldown state + metrics atomics** — there is no active health-check polling (no pings, no probes). A provider's status reflects its last dispatch outcome: a failure sets `cooling_until`; a success clears it. Between dispatch events the status remains at whatever the last outcome left it (idle providers stay Healthy until the next failure).
