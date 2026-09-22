# Configuration Reference

## Overview

Config is loaded from `~/.config/llmconduit/config.yaml` (or TOML via `.toml` extension) and
overlaid with `LLMCONDUIT_*` environment variables. The post-env struct is `PersistedConfig`
(line 1794); `Config::from_persisted` (line 2212) validates/resolves it into the runtime
`Config` (line 1010).

## Struct Hierarchy

```
PersistedConfig (1794)
 +-- upstreams: Vec<PersistedUpstream> (1748)
 |    +-- upstream_api_key | upstream_api_key_env   (mutually exclusive, line 3118)
 |    +-- upstream_retry / upstream_circuit_breaker / upstream_bulkhead  (per-provider overlays)
 |    +-- fallback_upstreams: Vec<PersistedFallbackUpstream> (1702, same overlays)
 +-- fallback_upstreams: Vec<PersistedFallbackUpstream>
 +-- upstream_retry: UpstreamRetryConfig (648)               [global policy]
 +-- upstream_circuit_breaker: Option<UpstreamCircuitBreakerConfig> (759)
 +-- upstream_bulkhead: UpstreamBulkheadConfig (843)
 +-- model_profile_templates: BTreeMap<String, PersistedModelProfile> (1529)
 |    +-- roles: Option<RolesConfig> (431)
 |    |    +-- rules: BTreeMap<String, RoleRuleSet> (398)
 |    |    |    +-- RoleRule { when, action, target_role, tag, tag_attributes } (384)
 |    |    +-- merge_adjacent: Vec<String>
 |    +-- capabilities: Option<CapabilitiesConfig> (321)
 |    |    +-- SimpleCap (220)  [batch, citations, code_execution, image_input, pdf_input,
 |    |    |                    structured_outputs]
 |    |    +-- ThinkingCap (255) / EffortCap (278) / ContextManagementCap (298)
 |    +-- reasoning_effort: Option<ReasoningConfig> (23)
 |         +-- default, map, thinking_param_name, thinking_param_value_{on,off}
 +-- model_profiles: BTreeMap<String, PersistedModelProfile> (same shape)
 +-- responses_capabilities: ResponsesCapabilitiesConfig (responses_capabilities.rs:110)
 +-- response_store: ResponseStoreConfig (600)
 +-- replay: Option<ReplayConfig> (626)
 +-- model_routes: OrderedModelRoutes (1444, declaration-order map)
 |    +-- PersistedModelRoute { upstream_base_url, upstream_model } (1385)
 +-- anthropic_passthrough: Option<PersistedAnthropicPassthrough> (anthropic_proxy.rs)
 |    +-- upstream_origin: String (only https://api.anthropic.com)
 |    +-- rules: Vec<PersistedPassthroughRule> { model: Option<String>, headers: BTreeMap<String, String> }
 +-- price_table: HashMap<String, ModelPrice> (1179)
      +-- input_per_1k, output_per_1k, cached_per_1k, cached_price_configured
```

---

## PersistedConfig (line 1794)

```rust
struct PersistedConfig {  // line 1794
    bind_addr: String,                                     // default "127.0.0.1:4000"
    upstream_base_url: String,                             // default "http://127.0.0.1:8000/v1"
    upstream_api_key: Option<String>,
    upstream_model: Option<String>,
    system_prompt_prefix: Option<String>,
    upstream_request_log_path: Option<String>,
    api_log_body_mode: LogBodyMode,                         // default Metadata (line 567)
    upstream_request_log_body_mode: LogBodyMode,            // default Metadata
    turn_capture_dir: Option<String>,                      // F1: per-turn debug capture
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstreams: Vec<PersistedUpstream>,                     // routing mode
    fallback_upstreams: Vec<PersistedFallbackUpstream>,    // non-routing failover
    upstream_retry: UpstreamRetryConfig,                   // default enabled (line 648)
    upstream_circuit_breaker: Option<UpstreamCircuitBreakerConfig>,  // None => legacy cooldown
    upstream_bulkhead: UpstreamBulkheadConfig,             // default: no limit (line 843)
    upstream_failure_cooldown_secs: u64,                   // deprecated; default 30
    model_profile_templates: BTreeMap<String, PersistedModelProfile>,
    model_profiles: BTreeMap<String, PersistedModelProfile>,
    responses_capabilities: ResponsesCapabilitiesConfig,
    model_routes: OrderedModelRoutes,                      // G7: ad-hoc routes in decl order
    anthropic_passthrough: Option<PersistedAnthropicPassthrough>, // default None; see anthropic-subscription-proxy.md
    template_family: Option<String>,                       // global: "kimi" | "deepseek"
    brave_base_url: String,                                // default "https://api.search.brave.com/res/v1"
    brave_api_key: Option<String>,
    brave_max_results: usize,                              // default 5
    request_timeout_secs: u64,                             // default 60
    connect_timeout_secs: u64,                             // default 10
    max_web_search_rounds: usize,                          // default 5
    flatten_content: bool,                                 // default true
    max_replay_entries: usize,                             // deprecated replay-size alias (default 100)
    response_store: ResponseStoreConfig,                   // Responses state
    replay: Option<ReplayConfig>,                          // private replay; default disabled
    debug_log_max_age_hours: Option<u64>,
    min_completion_tokens: i64,                            // default 4096
    max_sse_frame_bytes: usize,                            // default 8 MiB (8388608)
    max_request_body_bytes: usize,                         // default 10 MiB (10485760)
    image_agent_enabled: bool,                             // default false
    vision_url: Option<String>,
    vision_model: Option<String>,
    image_cache_max_size: usize,                           // default 100
    image_cache_ttl_secs: u64,                             // default 300
    unsupported_image_policy: UnsupportedImagePolicy,      // default Placeholder (line 556)
    price_table: HashMap<String, ModelPrice>,
}
```

---

## PersistedUpstream (line 1748)

```rust
struct PersistedUpstream {  // line 1748
    name: Option<String>,
    upstream_base_url: String,
    upstream_api_key: Option<String>,
    upstream_api_key_env: Option<String>,      // mutually exclusive with upstream_api_key
    upstream_model: Option<String>,
    wire_api: UpstreamWireApi,                 // chat_completions (default) | codex_responses
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstream_request_log_path: Option<String>,
    responses_capabilities: Option<ResponsesCapabilitiesConfig>,
    upstream_retry: Option<UpstreamRetryOverride>,                 // sparse overlay on global
    upstream_circuit_breaker: Option<UpstreamCircuitBreakerOverride>,
    upstream_bulkhead: Option<UpstreamBulkheadOverride>,
    fallback_upstreams: Vec<PersistedFallbackUpstream>,
}
```

Auto-named `upstream-N` (1-based) when `name` is omitted (parse_upstream, line 3052).

## Env-backed upstream credentials (resolve_upstream_api_key, line 3118)

`upstream_api_key_env` names an environment variable resolved at startup. Rules:

- Mutually exclusive with `upstream_api_key`; both set is a startup error.
- The name must be a valid identifier (`[A-Za-z_][A-Za-z0-9_]*`).
- The variable must be present and non-empty, or startup fails — a missing credential never
  silently degrades to an unauthenticated upstream.
- The resolved value is trimmed; it lives only in memory and is never serialized back by
  `write_persisted_config` (the `_env` field round-trips instead).

---

## PersistedFallbackUpstream (line 1702)

```rust
struct PersistedFallbackUpstream {  // line 1702
    name: Option<String>,
    upstream_base_url: String,
    upstream_api_key: Option<String>,
    upstream_api_key_env: Option<String>,      // mutually exclusive with upstream_api_key
    upstream_model: Option<String>,
    exposed_model: Option<String>,             // model id advertised to the client
    wire_api: UpstreamWireApi,                 // must match its primary (line 3029)
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstream_request_log_path: Option<String>,
    responses_capabilities: Option<ResponsesCapabilitiesConfig>,
    upstream_retry: Option<UpstreamRetryOverride>,
    upstream_circuit_breaker: Option<UpstreamCircuitBreakerOverride>,
    upstream_bulkhead: Option<UpstreamBulkheadOverride>,
}
```

Auto-named `fallback-N` (1-based). A nested fallback's `wire_api` must match its primary
(line 3032); a top-level (non-routing) `fallback_upstreams` entry must be `chat_completions`
(line 2246).

---

## Upstream resilience

Global policy fields on `PersistedConfig`; each upstream/fallback carries sparse
`Option<...Override>` blocks layered on top (`UpstreamResilienceConfig::apply_overrides`,
line 939). Legacy `upstream_failure_cooldown_secs` migrates into the circuit breaker's
maximum interval when `upstream_circuit_breaker` is absent (`from_legacy_cooldown_secs`,
line 779).

### UpstreamRetryConfig (line 648) / UpstreamRetryOverride (line 738)

Bounded, pre-output same-provider retry. Retries only explicit transient HTTP statuses;
transport and stream failures remain eligible for nested fallback but are never repeated on
the same provider.

```rust
struct UpstreamRetryConfig {  // line 648
    enabled: bool,                 // default true
    max_attempts: usize,           // default 3; valid 1..=10
    initial_backoff_ms: u64,       // default 500; >= 1
    max_backoff_ms: u64,           // default 4000; >= initial_backoff_ms
    total_budget_ms: u64,          // default 10000; valid 1..=60000
    honor_retry_after: bool,       // default true
    max_retry_after_secs: u64,     // default 15; <= 60
}
```

The override (line 738) wraps every field in `Option`; missing fields inherit the global
policy.

### UpstreamCircuitBreakerConfig (line 759) / Override (line 829)

Adaptive provider circuit. `max_open_ms == 0` is the compatibility spelling for a disabled
legacy cooldown.

```rust
struct UpstreamCircuitBreakerConfig {  // line 759
    initial_open_ms: u64,          // default 2000
    max_open_ms: u64,              // default 30000; <= 300000
    half_open_max_probes: usize,   // default 1; must be 1 (single-probe semantics)
}
```

Validation (line 803): `max_open_ms <= 300000`; when open, `initial_open_ms >= 1` and
`<= max_open_ms`; `half_open_max_probes == 1`.

### UpstreamBulkheadConfig (line 843) / Override (line 906)

Optional per-provider concurrency limit. `max_in_flight: null` preserves existing
concurrency; with a limit, `max_queue: null` means a zero-length queue (immediate bounded
rejection), never an unbounded waiter list.

```rust
struct UpstreamBulkheadConfig {  // line 843
    max_in_flight: Option<usize>,  // default None; 1..=100000
    max_queue: Option<usize>,      // requires max_in_flight; 0..=100000
    queue_timeout_ms: u64,         // default 5000; 1..=60000 when queue > 0
}
```

The override's `max_in_flight`/`max_queue` are `Option<Option<usize>>` (via
`deserialize_present_option`, line 923): an omitted field inherits the global value while an
explicit YAML `null` clears a global limit for this provider.

---

## PersistedModelProfile (line 1529)

Per-model overrides. Supports template inheritance via `extends`. Keyed by resolved model id.

```rust
struct PersistedModelProfile {  // line 1529
    extends: Vec<String>,                                    // template name(s) to inherit from
    upstream_model: Option<String>,                          // remap to a different upstream model id
    system_prompt_prefix: Option<String>,
    roles: Option<RolesConfig>,                              // role-routing rules
    template_family: Option<String>,                         // "kimi" | "deepseek"
    native_vision: Option<bool>,                             // G4: override multimodal detection
    upstream_chat_kwargs: JsonMap<String, JsonValue>,        // defaults (client value wins)
    reasoning_effort_map: BTreeMap<String, JsonValue>,       // effort level -> request fragment
    reasoning_effort_default: Option<String>,                 // default effort level
    capabilities: Option<CapabilitiesConfig>,                // /v1/models capability overrides
    responses_capabilities: Option<ResponsesCapabilitiesConfig>,
    reasoning_effort: Option<ReasoningConfig>,               // typed alternative (mutually exclusive)
}
```

The custom Deserialize (line 1574) also accepts unknown keys as `upstream_chat_kwargs`
shorthand (a flattened catch-all), then removes the recognized typed fields from that bucket
so `template_family`, `roles`, `reasoning_effort*`, and `capabilities` are never double-counted
as kwargs.

---

## RolesConfig (line 431)

Per-role message routing rules. Applied during conversation translation before upstream dispatch.

```rust
struct RolesConfig {  // line 431
    merge_adjacent: Vec<String>,               // roles to merge consecutive same-role
    rules: BTreeMap<String, RoleRuleSet>,      // role name -> rule(s)
}
```

### RoleRuleSet (line 398)

```rust
struct RoleRuleSet {  // line 398
    rules: Vec<RoleRule>,
}
```

A single rule or an array. `*` acts as a wildcard fallback (`rules_for`, line 482: exact role
first, then `*`).

### RoleRule (line 384)

```rust
struct RoleRule {  // line 384
    when: Option<When>,                          // Leading | Inline | Always
    action: Action,                              // Accept (default) | Reject | Drop | Rewrite
    target_role: Option<String>,                 // required when action = Rewrite
    tag: Option<String>,                         // content wrapper tag
    tag_attributes: BTreeMap<String, String>,    // requires tag to be set
}
```

### Action (line 360)

```rust
enum Action {  // line 360, default: Accept (snake_case serde)
    Accept,     // pass through as-is
    Reject,     // fail the turn
    Drop,       // silently discard the message
    Rewrite,    // re-role to target_role
}
```

### When (line 376)

```rust
enum When {  // line 376 (snake_case serde)
    Leading,    // only the first message of that role
    Inline,     // any subsequent message of that role
    Always,     // both leading and inline
}
```

---

## CapabilitiesConfig (line 321)

Per-profile capability overrides advertised via `/v1/models`. Replaces individual keys in the
upstream model advertisement (`merge_into`, line 334); unconfigured keys retain the upstream
default.

```rust
struct CapabilitiesConfig {  // line 321
    batch: Option<SimpleCap>,
    citations: Option<SimpleCap>,
    code_execution: Option<SimpleCap>,
    image_input: Option<SimpleCap>,
    pdf_input: Option<SimpleCap>,
    structured_outputs: Option<SimpleCap>,
    thinking: Option<ThinkingCap>,
    effort: Option<EffortCap>,
    context_management: Option<ContextManagementCap>,
}
```

### SimpleCap (line 220)

```rust
struct SimpleCap {  // line 220
    supported: bool,    // default true
}
```

Accepts bare boolean as shorthand: `image_input: true` is equivalent to `image_input: { supported: true }`.

### ThinkingCap (line 255)

```rust
struct ThinkingCap {  // line 255
    supported: bool,            // default true
    types: Vec<ThinkingType>,   // adaptive | enabled (line 149)
}
```

### EffortCap (line 278)

```rust
struct EffortCap {  // line 278
    supported: bool,           // default true
    levels: Vec<EffortLevel>,  // max | xhigh | high | medium | low | minimal | none (line 165)
}
```

### ContextManagementCap (line 298)

```rust
struct ContextManagementCap {  // line 298
    supported: bool,               // default true
    features: Vec<ContextFeature>, // clear_thinking_20251015 | clear_tool_uses_20250919 | compact_20260112 (line 191)
}
```

---

## ResponsesCapabilitiesConfig (responses_capabilities.rs, line 110)

Conservative OpenAI Responses capabilities can be declared globally, per upstream/fallback, and
per model profile. Resolution overlays global → provider → served-model profile (`overlay`,
line 138; per-profile inheritance merges via the same overlay). Checks occur only
after the primary route has been selected: an unsupported primary request returns a parameter-
specific 400, while an incapable nested fallback is omitted for that request without a cooldown or
health penalty.

```yaml
responses_capabilities:
  parallel_tool_calls: false
  structured_outputs: [text, json_object, json_schema]
  reasoning_summary: upstream       # upstream | unsupported
  encrypted_reasoning: unsupported  # passthrough | unsupported
  agent_message_encrypted_content: plaintext_compat  # plaintext_compat | unsupported
  input_image: placeholder          # native | agent | placeholder | reject
  input_file: unsupported           # native | unsupported
  truncation_auto: unsupported      # upstream | unsupported
  text_verbosity: unsupported       # upstream | unsupported
  service_tiers: []
  prompt_cache_key: gateway_hash    # gateway_hash | upstream | unsupported
  prompt_cache_retention: []
```

An omitted optional declaration is conservative: baseline text and the existing safe placeholder
image policy remain available, while other advanced behavior is unsupported unless declared.
`prompt_cache_key: gateway_hash` hashes the opaque key into a private local affinity namespace,
keeps the caller's value unchanged in the public Response resource, and never forwards it.
`upstream` forwards the original key after the capability check. Request logs and turn-capture
artifacts redact/suppress the opaque value in either mode. Unsupported hosted OpenAI tools and
services are not enabled by this registry.

## UpstreamWireApi (line 589)

```rust
enum UpstreamWireApi {  // line 589, default: ChatCompletions
    ChatCompletions,   // "chat_completions"
    CodexResponses,    // "codex_responses"; legacy spelling "responses" is an ingress-only alias
}
```

`codex_responses` is the deliberately narrow Codex Responses-Lite sidecar contract, not a claim
that arbitrary public Responses providers share its private headers/event dialect. A failover
chain must use one protocol throughout: nested fallback `wire_api` must equal its primary's
(line 3032), and top-level (non-routing) fallbacks must be `chat_completions` (line 2246).

## Responses state and private replay

The public Responses state store and llmconduit's private prefix replay cache are independent:

```yaml
response_store:      # ResponseStoreConfig, line 600
  backend: memory       # memory | sqlite (ResponseStoreBackend, line 575)
  path: null            # required for sqlite; invalid for memory
  max_entries: 1000     # default 1000, must be >= 1
  retention_hours: 720  # default 720, must be >= 1

replay:              # ReplayConfig, line 626
  enabled: false        # default false
  max_entries: 100      # default 100, must be >= 1
```

`store:true` persists completed/incomplete canonical history so a later request can use
`previous_response_id`; `store:false` does not. Missing, expired, evicted, failed, cancelled, or
non-stored IDs are not referenceable. The memory backend is a bounded TTL-aware LRU. SQLite uses a
versioned transactional database, restrictive Unix permissions, asynchronous blocking-pool work,
and a bounded memory front cache; its configured path is authoritative across restarts.

Private replay defaults off and is not controlled by `store`. When replay is enabled, the
non-standard `llmconduit_replay:false` request extension bypasses lookup and insertion for that
request and is consumed before upstream dispatch. `max_replay_entries` remains a deprecated size
alias for older configurations; prefer `replay.max_entries` (when `replay` is absent, the alias
still seeds the effective config, line 2298).

## API logging modes

Both request-body logging surfaces default to metadata-only operation (`LogBodyMode`, line 567):

```yaml
api_log_body_mode: metadata
upstream_request_log_body_mode: metadata
```

Accepted values are `metadata` and `redacted_payload`. Payload mode is an explicit diagnostic
opt-in and still applies the shared secret and image-URI redactors. Malformed JSON is represented
only by a bounded hash/length marker. Durable turn capture remains a separate opt-in.

API authentication is environment-only, not persisted here. `LLMCONDUIT_API_TOKEN` protects every
`/v1/*` route and accepts Bearer or `x-api-key`. Loopback may run without a token; a wildcard or
other non-loopback bind refuses startup unless the token or the explicit development override
`LLMCONDUIT_ALLOW_UNAUTHENTICATED_API=1` is present. `/` and `/health` stay public, and dashboard
authentication remains separate.

---

## ReasoningConfig (line 23)

Typed shorthand for effort remapping and thinking-kwarg control. Mutually exclusive with
`reasoning_effort_map` + `reasoning_effort_default` in a profile.

```rust
struct ReasoningConfig {  // line 23
    default: Option<String>,
    map: BTreeMap<String, String>,              // effort level -> mapped value
    thinking_param_name: String,                // default "enable_thinking"
    thinking_param_value_on: JsonValue,          // default true
    thinking_param_value_off: JsonValue,         // default false
    forward_thinking_param: bool,                // default true; false omits dynamic template kwargs
}
```

Map keys and values are trimmed; empty entries are dropped (Deserialize, line 91).
`thinking_param_value(on)` (line 82) selects the on/off JSON value.

---

## ModelPrice (line 1179)

Per-model billing rates, keyed by served model id. Drives dashboard flow cost rollup. Field
names mirror the frozen frontend `ModelPrice` contract byte-for-byte.

```rust
struct ModelPrice {  // line 1179
    input_per_1k: f64,               // USD per 1k prompt tokens
    output_per_1k: f64,              // USD per 1k completion tokens
    cached_per_1k: f64,             // USD per 1k cached prompt tokens (default 0.0)
    cached_price_configured: bool,   // whether cached_per_1k was explicitly set (default false)
}
```

`cached_price_configured` is the gap-07 presence seam: a `0.0` cached rate is ambiguous between
"provider charges 0 for cache reads" and "entry omitted the rate". The custom Deserialize
(line 1257) prefers an explicit flag, else derives presence from whether a `cached_per_1k` key
was present. Entries with a negative or non-finite rate are dropped with a warning
(`retain_finite_prices`, line 1294; both YAML and the env JSON override feed through it).

---

## Model routes (G7)

`model_routes` is an `OrderedModelRoutes` (line 1444): a `Vec` of `(name, route)` pairs that
(de)serializes as a YAML map while preserving declaration order, so overlapping globs are
first-match-wins. Duplicate keys collapse to last-wins in place (`upsert`, line 1461). CLI
`--model-route NAME=URL[,UPSTREAM_MODEL]` specs (parser at line 2795) merge in after env
overrides, replacing a same-named file route in place.

```rust
struct PersistedModelRoute {  // line 1385
    upstream_base_url: Option<String>,   // or bare string / `url` alias
    upstream_model: Option<String>,      // or `model` alias
}
```

Glob names (`*`, `?`, `[...]`) compile to anchored case-insensitive regexes (`glob_to_regex`,
line 2716); an uncompilable pattern is a clean startup error. A route slots between an exact
catalog id and the canonical-key/default fallbacks — an exact upstream id always beats a route.

---

## Resolved (Runtime) Structs

The following are constructed by `Config::from_persisted` (line 2212) and are not serialized
directly.

All configured service URLs are validated before these structs are built (`parse_service_url`,
line 3161). Top-level, routing, fallback, model-route, Brave, and Vision URLs reject
userinfo/passwords, query strings, and fragments; credentials belong in the dedicated key fields
or environment variables and cannot be embedded in a projected URL.

### Config (line 1010)

```rust
struct Config {  // line 1010
    bind_addr: SocketAddr,
    upstream_base_url: Url,
    upstream_api_key: Option<String>,
    upstream_model: Option<String>,
    system_prompt_prefix: Option<String>,
    upstream_request_log_path: Option<PathBuf>,
    api_log_body_mode: LogBodyMode,
    upstream_request_log_body_mode: LogBodyMode,
    turn_capture_dir: Option<PathBuf>,
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstreams: Vec<UpstreamConfig>,
    fallback_upstreams: Vec<FallbackUpstreamConfig>,
    upstream_retry: UpstreamRetryConfig,
    upstream_circuit_breaker: UpstreamCircuitBreakerConfig,
    upstream_bulkhead: UpstreamBulkheadConfig,
    upstream_failure_cooldown_secs: u64,     // deprecated input, retained for diagnostics
    model_profiles: BTreeMap<String, ModelProfile>,
    responses_capabilities: ResponsesCapabilitiesConfig,
    model_routes: Vec<ModelRoute>,
    anthropic_passthrough: Option<AnthropicPassthrough>,
    template_family: Option<String>,
    brave_base_url: Url,
    brave_api_key: Option<String>,
    brave_max_results: usize,
    request_timeout: Duration,
    connect_timeout_secs: u64,
    max_web_search_rounds: usize,
    flatten_content: bool,
    max_replay_entries: usize,
    response_store: ResponseStoreConfig,
    replay: ReplayConfig,
    debug_log_max_age_hours: Option<u64>,
    min_completion_tokens: i64,
    max_sse_frame_bytes: usize,
    max_request_body_bytes: usize,
    image_agent_enabled: bool,
    vision_url: Option<Url>,
    vision_model: Option<String>,
    image_cache_max_size: usize,
    image_cache_ttl_secs: u64,
    unsupported_image_policy: UnsupportedImagePolicy,
    price_table: HashMap<String, ModelPrice>,
}
```

`has_backend_credentials` (line 1115): true when any primary, fallback, nested fallback, or
Brave config carries a credential; turn capture uses it to decide retention of raw peer output.

### ModelProfile (line 1642)

Resolved profile after template inheritance. Same fields as `PersistedModelProfile` minus
`extends`; `template_family` is normalized to `kimi`/`deepseek` (line 3179) and
`system_prompt_prefix` is the template-chain prefixes joined with `"\n\n"` (line 2995).

### ModelRoute (line 1340)

```rust
struct ModelRoute {  // line 1340
    name: String,
    glob: Option<Regex>,   // compiled glob matcher, None for exact-match routes
    upstream_base_url: Url,
    upstream_model: Option<String>,
}
```

`route_matches` (line 1369) is the shared boolean match primitive (exact case-insensitive, or glob).

### UpstreamConfig (line 1308) / FallbackUpstreamConfig (line 1671)

Resolved providers with parsed URL and `name: String` (auto-generated when omitted). Each carries
a fully-resolved `resilience: UpstreamResilienceConfig` (global policy + that provider's
overrides); the fallback adds `exposed_model: Option<String>`.

### ReasoningEffortPolicy (line 1661)

```rust
struct ReasoningEffortPolicy {  // line 1661
    map: BTreeMap<String, JsonValue>,
    default: Option<String>,
    upstream_reasoning: Option<ReasoningConfig>,  // present when built from the typed syntax
}
```

---

## Validation Rules

### RolesConfig::validate (line 489)

- `action: rewrite` requires a non-empty `target_role`.
- `target_role` is only valid with `action: rewrite`.
- `tag` / `tag_attributes` names must match `[a-zA-Z0-9_\-:.]`.
- `tag_attributes` requires a non-empty `tag`.
- `merge_adjacent` only permits content-only roles: `system`, `developer`, `user`. Merging
  `assistant` or `tool` is rejected because it would discard `tool_calls`/`tool_call_id`.

### Model Profile Validation (resolve_model_profiles, line 2824)

- `reasoning_effort` (typed shorthand) cannot be combined with `reasoning_effort_map` or
  `reasoning_effort_default`.
- Cycles in `extends` are detected and rejected with a cycle trace error (line 2866).
- Unknown template references are rejected at startup (case-insensitive template lookup at
  line 2871).

### Resilience Validation

- Retry (line 710): `max_attempts` 1..=10; `initial_backoff_ms >= 1`; `max_backoff_ms >=
  initial_backoff_ms`; `total_budget_ms` 1..=60000; `max_retry_after_secs <= 60`.
- Circuit breaker (line 803): `max_open_ms <= 300000`; `initial_open_ms` in 1..=max_open_ms when
  open; `half_open_max_probes == 1`.
- Bulkhead (line 874): `max_in_flight` 1..=100000; `max_queue` requires `max_in_flight`; when
  queueing, `queue_timeout_ms` 1..=60000 and `max_queue <= 100000`.

### Credentials (resolve_upstream_api_key, line 3118)

- `upstream_api_key` and `upstream_api_key_env` cannot both be set.
- `_env` must name a valid identifier and resolve to a present, non-empty variable.

### Price Table (retain_finite_prices, line 1294)

- Any `ModelPrice` entry with a negative or non-finite rate is dropped with a warning (both YAML
  and env overrides).

### SSE / Body Size Floors (in `from_persisted`, lines 2364–2376)

- `min_completion_tokens` is floored at 1.
- `max_sse_frame_bytes` is floored at 1024 (1 KiB).
- `max_request_body_bytes` is floored at 1024 (1 KiB).
- `image_cache_max_size` is floored at 1.

### Response store and replay (lines 2270–2304)

- `response_store.max_entries` and `response_store.retention_hours` must each be at least 1.
- `response_store.backend: sqlite` requires a non-empty `path`.
- `response_store.path` is rejected for the memory backend.
- `replay.max_entries` must be at least 1; replay remains disabled unless `replay.enabled` is true.

---

## Resolution Logic

### Config::from_persisted (line 2212)

1. Parse strings into typed values (`SocketAddr`, `Url`, `PathBuf`, `Duration`).
2. Migrate the legacy cooldown into the circuit breaker when no explicit block is configured.
3. Build the global resilience policy and validate it (line 2229).
4. Parse fallback upstreams (rejecting non-`chat_completions` top-level fallbacks) and routing
   upstreams (rejecting nested wire-protocol mismatch), each with its overlaid resilience.
5. Resolve model profiles via `resolve_model_profiles` (line 2824) — recursive `extends` merge
   with cycle detection.
6. Resolve model routes via `resolve_model_routes` (line 2764) — compile globs, reject blank
   keys / missing or invalid URLs.
   Resolve optional `anthropic_passthrough` with `AnthropicPassthrough::resolve`: validate the
   fixed HTTPS Anthropic origin, compile model globs, and validate header conditions. See
   [native subscription routing](anthropic-subscription-proxy.md) for precedence and limits.
7. Validate response store / replay bounds.
8. Floor safety-critical values (completion tokens, frame/body byte caps, image-cache size).
9. Filter non-finite price entries.

### Template Inheritance (resolve_persisted_model_profile, line 2855)

- `extends` is a list of template names resolved from `model_profile_templates`.
- Templates are resolved recursively (DFS) with cycle detection; lookup is exact then
  ASCII-case-insensitive.
- Field merging (merge_resolved_model_profile, line 2889 / merge_persisted_model_profile, line 2939):
  - `upstream_model`, `template_family`, `native_vision`, `roles`, `capabilities` — set-if-some
    (child wins).
  - `responses_capabilities` — field-by-field overlay (child's declared keys win).
  - `system_prompt_prefix` — appended to a vector, joined by `"\n\n"` (line 2995).
  - `upstream_chat_kwargs` — deep-merged (`merge_json_maps`, line 3484).
  - `reasoning_effort_map` — per-level insert (child level overrides parent level);
    `reasoning_effort_default` set-if-some.
  - `reasoning_effort` (typed) replaces the fragment-based map/default entirely, and a child
    fragment map clears an inherited typed config (line 2915).

### Profile Lookup (model_profile, line 2662)

1. Exact key match in `model_profiles` BTreeMap.
2. ASCII-case-insensitive fallback (first matching key).
3. `model_profiles_for_resolved_model` (line 2638) collects, in order: final backend model,
   configured upstream model, then request model — deduplicated — and falls back to the `*`
   wildcard profile only when none matched. Consumers (`resolve_roles_config_for_resolved_model`
   line 2627, `resolve_system_prompt_prefix_for_resolved_model` line 2590) scan that list in
   reverse (request model first).

### Leaf policy accessors

Applied at the upstream leaf — the single point that knows the FINAL provider model after
routing/failover/exposed-alias remap:

| Method | Line | Purpose |
|---|---|---|
| `resolve_upstream_model` | 2379 | profile `upstream_model` → global `upstream_model` → request model |
| `explicit_response_model_alias` | 2390 | profile-only alias (no global fallback), for Responses aliasing |
| `price_for` | 2400 | exact then case-insensitive `ModelPrice` lookup |
| `matches_model_route` | 2417 | boolean glob/exact route match (dispatch projection) |
| `is_plain_single_provider` | 2430 | true when no upstreams, routes, or fallbacks are configured |
| `reasoning_effort_policies` | 2443 | per-backend-model effort map/default (compiles both syntaxes) |
| `template_family_policies` | 2510 | per-backend-model family overrides |
| `global_template_family` | 2526 | global family fallback for the leaf |
| `upstream_chat_kwargs_policies` | 2541 | per-backend-model kwargs (extends-merged) |
| `global_upstream_chat_kwargs` | 2557 | global kwargs base layer for the leaf |
| `profile_native_vision` | 2569 | profile-only `native_vision` for exactly the given model (G4) |
| `resolve_capabilities_for_upstream` | 2599 | id-keyed profile → alias targeting id → `*` profile |
| `debug_log_dirs` | 2152 | deduped log dirs the running gateway actually writes (rotation) |

### Env Overrides (apply_env_overrides, line 3267)

Each `LLMCONDUIT_*` env var overrides the YAML value (blank/unparseable values are ignored).
Key env vars:

| Env Var | Overrides |
|---|---|
| `LLMCONDUIT_UPSTREAM_API_KEY` | `upstream_api_key` |
| `OPENAI_API_KEY` | `upstream_api_key` (fallback if no YAML value and no `LLMCONDUIT_*` set) |
| `LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON` | `upstream_chat_kwargs` (wholesale JSON replace) |
| `LLMCONDUIT_API_LOG_BODY_MODE` | `api_log_body_mode` (`metadata` or `redacted_payload`) |
| `LLMCONDUIT_UPSTREAM_REQUEST_LOG_BODY_MODE` | `upstream_request_log_body_mode` |
| `LLMCONDUIT_REPLAY_ENABLED` | `replay.enabled` (boolean parse) |
| `LLMCONDUIT_PRICE_TABLE_JSON` | `price_table` (wholesale JSON replace) |
| `BRAVE_SEARCH_API_KEY` | `brave_api_key` |

The full list (every var read in `apply_env_overrides`): `LLMCONDUIT_BIND_ADDR`,
`LLMCONDUIT_UPSTREAM_BASE_URL`, `LLMCONDUIT_UPSTREAM_API_KEY`, `OPENAI_API_KEY`,
`LLMCONDUIT_UPSTREAM_MODEL`, `LLMCONDUIT_TEMPLATE_FAMILY`, `LLMCONDUIT_SYSTEM_PROMPT_PREFIX`,
`LLMCONDUIT_UPSTREAM_REQUEST_LOG_PATH`, `LLMCONDUIT_API_LOG_BODY_MODE`,
`LLMCONDUIT_UPSTREAM_REQUEST_LOG_BODY_MODE`, `LLMCONDUIT_TURN_CAPTURE_DIR`,
`LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON`, `LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS`,
`LLMCONDUIT_BRAVE_BASE_URL`, `BRAVE_SEARCH_API_KEY`, `LLMCONDUIT_BRAVE_MAX_RESULTS`,
`LLMCONDUIT_REQUEST_TIMEOUT_SECS`, `LLMCONDUIT_CONNECT_TIMEOUT_SECS`,
`LLMCONDUIT_MAX_WEB_SEARCH_ROUNDS`, `LLMCONDUIT_FLATTEN_CONTENT`,
`LLMCONDUIT_MAX_REPLAY_ENTRIES`, `LLMCONDUIT_REPLAY_ENABLED`,
`LLMCONDUIT_DEBUG_LOG_MAX_AGE_HOURS`, `LLMCONDUIT_MIN_COMPLETION_TOKENS`,
`LLMCONDUIT_MAX_SSE_FRAME_BYTES`, `LLMCONDUIT_MAX_REQUEST_BODY_BYTES`,
`LLMCONDUIT_IMAGE_AGENT_ENABLED`, `LLMCONDUIT_VISION_URL`, `LLMCONDUIT_VISION_MODEL`,
`LLMCONDUIT_IMAGE_CACHE_MAX_SIZE`, `LLMCONDUIT_IMAGE_CACHE_TTL_SECS`,
`LLMCONDUIT_UNSUPPORTED_IMAGE_POLICY`, `LLMCONDUIT_PRICE_TABLE_JSON`.

`LLMCONDUIT_API_TOKEN` and `LLMCONDUIT_ALLOW_UNAUTHENTICATED_API` are environment-only public API
security controls rather than `PersistedConfig` overrides. They must never be written into YAML.
`upstream_api_key_env` (per upstream/fallback) resolves any additional named variable at startup.

### Config file loading

`default_config_path` (line 3187) resolves `~/.config/llmconduit/config.yaml`. A missing file
loads the default config. `.toml` paths (by extension, line 3201) parse via the `toml` crate and
are read-only: `write_persisted_config` (line 3224) refuses to write TOML and creates/writes
YAML with mode 0600.

---

## Current Live Config (`~/.config/llmconduit/config.yaml`)

This section is descriptive only; repository changes do not edit the live file or service. Because
the described listener is non-loopback, a restart requires the environment-only
`LLMCONDUIT_API_TOKEN` (preferred) or the explicit insecure override. Configure that deployment
prerequisite separately before restarting.

The live config binds `0.0.0.0:5022` in routing mode with six upstreams: `local-vllm`
(`http://localhost:8000/v1`), plus remote `deepseek-api`, `xai-api`, `gemini-api`, and two
`meta-api` pins (`wire_api: codex_responses`) whose credentials all resolve from
`upstream_api_key_env` service-environment variables. Model profiles cover the local vLLM catalog
(DeepSeek V4/V4.1 families with effort-ladder maps and inline-system role rewrites, Kimi K2.7/K3,
Qwen3.8 variants, GLM 5.2/5.3 variants with native-vision overrides and fragment-based effort
maps) plus remote profiles (`deepseek-v4-pro`, `grok-4.6`, `gemini-3.7-flash`, `muse-spark-1.3*`).
Per-upstream request logs live under `~/.local/share/llmconduit/`, debug rotation at 24h,
per-turn capture at `~/.local/share/llmconduit/turns`, timeout 1800s, and a `price_table` with
per-1k rates for `GLM-5.2-NVFP4` and `DeepSeek-V4-Flash-0731`.
