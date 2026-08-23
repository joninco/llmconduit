# Configuration Reference

## Overview

Config is loaded from `~/.config/llmconduit/config.yaml` (or TOML via `.toml` extension) and
overlaid with `LLMCONDUIT_*` environment variables. The post-env struct is `PersistedConfig`;
`Config::from_persisted` validates/resolves it into the runtime `Config`.

## Struct Hierarchy

```
PersistedConfig
 +-- upstreams: Vec<PersistedUpstream>
 |    +-- fallback_upstreams: Vec<PersistedFallbackUpstream>
 +-- fallback_upstreams: Vec<PersistedFallbackUpstream>
 +-- model_profile_templates: BTreeMap<String, PersistedModelProfile>
 |    +-- roles: Option<RolesConfig>
 |    |    +-- rules: BTreeMap<String, RoleRuleSet>
 |    |    |    +-- RoleRule { when, action, target_role, tag, tag_attributes }
 |    |    +-- merge_adjacent: Vec<String>
 |    +-- capabilities: Option<CapabilitiesConfig>
 |    |    +-- SimpleCap (batch, citations, code_execution, image_input, pdf_input, structured_outputs)
 |    |    +-- ThinkingCap (supported, types)
 |    |    +-- EffortCap (supported, levels)
 |    |    +-- ContextManagementCap (supported, features)
 |    +-- reasoning_effort: Option<ReasoningConfig>
 |         +-- default, map, thinking_param_name, thinking_param_value_{on,off}
 +-- model_profiles: BTreeMap<String, PersistedModelProfile> (same shape)
 +-- responses_capabilities: ResponsesCapabilitiesConfig
 +-- response_store: ResponseStoreConfig
 +-- replay: ReplayConfig
 +-- model_routes: OrderedModelRoutes (declaration-order map)
 |    +-- PersistedModelRoute { upstream_base_url, upstream_model }
 +-- price_table: HashMap<String, ModelPrice>
      +-- input_per_1k, output_per_1k, cached_per_1k, cached_price_configured
```

---

## PersistedConfig (line 1165)

```rust
struct PersistedConfig {  // line 1165
    bind_addr: String,                                     // default "127.0.0.1:4000"
    upstream_base_url: String,                             // default "http://127.0.0.1:8000/v1"
    upstream_api_key: Option<String>,
    upstream_model: Option<String>,
    system_prompt_prefix: Option<String>,
    upstream_request_log_path: Option<String>,
    api_log_body_mode: LogBodyMode,                         // default Metadata
    upstream_request_log_body_mode: LogBodyMode,            // default Metadata
    turn_capture_dir: Option<String>,                      // F1: per-turn debug capture
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstreams: Vec<PersistedUpstream>,                     // routing mode
    fallback_upstreams: Vec<PersistedFallbackUpstream>,    // non-routing failover
    upstream_failure_cooldown_secs: u64,                   // default 30
    model_profile_templates: BTreeMap<String, PersistedModelProfile>,
    model_profiles: BTreeMap<String, PersistedModelProfile>,
    responses_capabilities: ResponsesCapabilitiesConfig,
    model_routes: OrderedModelRoutes,                      // G7: ad-hoc routes in decl order
    template_family: Option<String>,                       // global: "kimi" | "deepseek"
    brave_base_url: String,                                // default "https://api.search.brave.com/res/v1"
    brave_api_key: Option<String>,
    brave_max_results: usize,                              // default 5
    request_timeout_secs: u64,                             // default 60
    connect_timeout_secs: u64,                             // default 10
    max_web_search_rounds: usize,                          // default 5
    flatten_content: bool,                                 // default true
    max_replay_entries: usize,                             // deprecated replay-size alias
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
    unsupported_image_policy: UnsupportedImagePolicy,      // default Placeholder
    price_table: HashMap<String, ModelPrice>,
}
```

---

## PersistedUpstream (line 1148)

```rust
struct PersistedUpstream {  // line 1148
    name: Option<String>,
    upstream_base_url: String,
    upstream_api_key: Option<String>,
    upstream_api_key_env: Option<String>,      // mutually exclusive with upstream_api_key
    upstream_model: Option<String>,
    wire_api: UpstreamWireApi,                 // chat_completions (default) | codex_responses
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstream_request_log_path: Option<String>,
    responses_capabilities: Option<ResponsesCapabilitiesConfig>,
    fallback_upstreams: Vec<PersistedFallbackUpstream>,
}
```

---

## PersistedFallbackUpstream (line 1130)

```rust
struct PersistedFallbackUpstream {  // line 1130
    name: Option<String>,
    upstream_base_url: String,
    upstream_api_key: Option<String>,
    upstream_api_key_env: Option<String>,      // mutually exclusive with upstream_api_key
    upstream_model: Option<String>,
    exposed_model: Option<String>,            // model id advertised to the client
    wire_api: UpstreamWireApi,                // must match its primary
    upstream_chat_kwargs: JsonMap<String, JsonValue>,
    upstream_request_log_path: Option<String>,
    responses_capabilities: Option<ResponsesCapabilitiesConfig>,
}
```

---

## PersistedModelProfile (line 986)

Per-model overrides. Supports template inheritance via `extends`. Keyed by resolved model id.

```rust
struct PersistedModelProfile {  // line 986
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

---

## RolesConfig (line 420)

Per-role message routing rules. Applied during conversation translation before upstream dispatch.

```rust
struct RolesConfig {  // line 420
    merge_adjacent: Vec<String>,               // roles to merge consecutive same-role
    rules: BTreeMap<String, RoleRuleSet>,      // role name -> rule(s)
}
```

### RoleRuleSet (line 388)

```rust
struct RoleRuleSet {  // line 388
    rules: Vec<RoleRule>,
}
```

A single rule or an array. `*` acts as a wildcard fallback.

### RoleRule (line 372)

```rust
struct RoleRule {  // line 372
    when: Option<When>,                          // Leading | Inline | Always
    action: Action,                              // Accept (default) | Reject | Drop | Rewrite
    target_role: Option<String>,                 // required when action = Rewrite
    tag: Option<String>,                         // content wrapper tag
    tag_attributes: BTreeMap<String, String>,    // requires tag to be set
}
```

### Action (line 348)

```rust
enum Action {  // line 348, default: Accept
    Accept,     // pass through as-is
    Reject,     // fail the turn
    Drop,       // silently discard the message
    Rewrite,    // re-role to target_role
}
```

### When (line 364)

```rust
enum When {  // line 364
    Leading,    // only the first message of that role
    Inline,     // any subsequent message of that role
    Always,     // both leading and inline
}
```

---

## CapabilitiesConfig (line 309)

Per-profile capability overrides advertised via `/v1/models`. Replaces individual keys in the upstream
model advertisement; unconfigured keys retain the upstream default.

```rust
struct CapabilitiesConfig {  // line 309
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

### SimpleCap (line 209)

```rust
struct SimpleCap {  // line 209
    supported: bool,    // default true
}
```

Accepts bare boolean as shorthand: `image_input: true` is equivalent to `image_input: { supported: true }`.

### ThinkingCap (line 243)

```rust
struct ThinkingCap {  // line 243
    supported: bool,            // default true
    types: Vec<ThinkingType>,   // Adaptive | Enabled
}
```

### EffortCap (line 266)

```rust
struct EffortCap {  // line 266
    supported: bool,           // default true
    levels: Vec<EffortLevel>,  // Max | Xhigh | High | Medium | Low | Minimal | Disabled("none")
}
```

### ContextManagementCap (line 286)

```rust
struct ContextManagementCap {  // line 286
    supported: bool,               // default true
    features: Vec<ContextFeature>, // ClearThinking20251015 | ClearToolUses20250919 | Compact20260112
}
```

---

## ResponsesCapabilitiesConfig

Conservative OpenAI Responses capabilities can be declared globally, per upstream/fallback, and
per model profile. Resolution overlays global → provider → served-model profile. Checks occur only
after the primary route has been selected: an unsupported primary request returns a parameter-
specific 400, while an incapable nested fallback is omitted for that request without a cooldown or
health penalty.

```yaml
responses_capabilities:
  parallel_tool_calls: false
  structured_outputs: [text, json_object, json_schema]
  reasoning_summary: upstream       # upstream | unsupported
  encrypted_reasoning: unsupported  # passthrough | unsupported
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

## Responses state and private replay

The public Responses state store and llmconduit's private prefix replay cache are independent:

```yaml
response_store:
  backend: memory       # memory | sqlite
  path: null            # required for sqlite; invalid for memory
  max_entries: 1000
  retention_hours: 720

replay:
  enabled: false
  max_entries: 100
```

`store:true` persists completed/incomplete canonical history so a later request can use
`previous_response_id`; `store:false` does not. Missing, expired, evicted, failed, cancelled, or
non-stored IDs are not referenceable. The memory backend is a bounded TTL-aware LRU. SQLite uses a
versioned transactional database, restrictive Unix permissions, asynchronous blocking-pool work,
and a bounded memory front cache; its configured path is authoritative across restarts.

Private replay defaults off and is not controlled by `store`. When replay is enabled, the
non-standard `llmconduit_replay:false` request extension bypasses lookup and insertion for that
request and is consumed before upstream dispatch. `max_replay_entries` remains a deprecated size
alias for older configurations; prefer `replay.max_entries`.

## API logging modes

Both request-body logging surfaces default to metadata-only operation:

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

---

## ModelPrice (line 658)

Per-model billing rates, keyed by served model id. Drives dashboard flow cost rollup.

```rust
struct ModelPrice {  // line 658
    input_per_1k: f64,               // USD per 1k prompt tokens
    output_per_1k: f64,              // USD per 1k completion tokens
    cached_per_1k: f64,             // USD per 1k cached prompt tokens (default 0.0)
    cached_price_configured: bool,   // whether cached_per_1k was explicitly set (default false)
}
```

Non-finite (NaN/Inf) entries are silently dropped with a warning.

---

## Resolved (Runtime) Structs

The following are constructed by `Config::from_persisted` and are not serialized directly.

All configured service URLs are validated before these structs are built. Top-level,
routing, fallback, model-route, Brave, and Vision URLs reject userinfo/passwords,
query strings, and fragments; credentials belong in the dedicated key fields or
environment variables and cannot be embedded in a projected URL.

### Config (line 553)

```rust
struct Config {  // line 553
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
    upstream_failure_cooldown_secs: u64,
    model_profiles: BTreeMap<String, ModelProfile>,
    responses_capabilities: ResponsesCapabilitiesConfig,
    model_routes: Vec<ModelRoute>,
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

### ModelProfile (line 1091)

Resolved profile after template inheritance. Same fields as `PersistedModelProfile` minus `extends`.

### ModelRoute (line 797)

```rust
struct ModelRoute {  // line 797
    name: String,
    glob: Option<Regex>,   // compiled glob matcher, None for exact-match routes
    upstream_base_url: Url,
    upstream_model: Option<String>,
}
```

### UpstreamConfig (line 784)

Resolved upstream with parsed URL. Fields mirror `PersistedUpstream` with `SocketAddr`/`Url`/`PathBuf`
types.

### FallbackUpstreamConfig (line 1119)

Resolved fallback. Adds `exposed_model: Option<String>`.

### ReasoningEffortPolicy (line 1109)

```rust
struct ReasoningEffortPolicy {  // line 1109
    map: BTreeMap<String, JsonValue>,
    default: Option<String>,
    upstream_reasoning: Option<ReasoningConfig>,
}
```

---

## Validation Rules

### RolesConfig::validate (line 479)

- `action: rewrite` requires a non-empty `target_role`.
- `target_role` is only valid with `action: rewrite`.
- `tag` names must match `[a-zA-Z0-9_\-:.]`.
- `tag_attributes` requires a non-empty `tag`.
- `merge_adjacent` only permits content-only roles: `system`, `developer`, `user`. Merging `assistant` or `tool` is rejected because it would discard `tool_calls`/`tool_call_id`.

### Model Profile Validation (in `resolve_model_profiles`, line 2020)

- `reasoning_effort` (typed shorthand) cannot be combined with `reasoning_effort_map` or `reasoning_effort_default`.
- Cycles in `extends` are detected and rejected with a cycle trace error.
- Unknown template references are rejected at startup.

### Price Table (line 771)

- Any `ModelPrice` entry with NaN or infinite rate is dropped with a warning (both YAML and env overrides).

### SSE / Body Size Floors (in `from_persisted`, line 1575-1581)

- `min_completion_tokens` is floored at 1.
- `max_sse_frame_bytes` is floored at 1024 (1 KiB).
- `max_request_body_bytes` is floored at 1024 (1 KiB).
- `image_cache_max_size` is floored at 1.

### Response store and replay

- `response_store.max_entries` and `response_store.retention_hours` must each be at least 1.
- `response_store.backend: sqlite` requires a non-empty `path`.
- `response_store.path` is rejected for the memory backend.
- `replay.max_entries` must be at least 1; replay remains disabled unless `replay.enabled` is true.

---

## Resolution Logic

### Config::from_persisted (line 1495)

1. Parse strings into typed values (`SocketAddr`, `Url`, `PathBuf`, `Duration`).
2. Resolve model profiles via `resolve_model_profiles`:
   - Recursively merge `extends` chain (template inheritance).
   - Child fields override parent fields at each level.
   - `reasoning_effort` (typed) clears any fragment-based map/default from the parent.
3. Resolve model routes via `resolve_model_routes`:
   - Compile globs to regex (anchored, case-insensitive).
   - Reject blank keys, missing/invalid URLs, uncompilable globs.
4. Floor safety-critical values (completion tokens, frame/body byte caps).
5. Filter non-finite price entries.

### Template Inheritance (resolve_persisted_model_profile, line 2051)

- `extends` is a list of template names resolved from `model_profile_templates`.
- Templates are resolved recursively (DFS) with cycle detection.
- Fields merge: `upstream_model`, `template_family`, `native_vision` are replace-on-some;
  `system_prompt_prefix` collects into a vector joined by `"\n\n"`; `upstream_chat_kwargs`
  are deep-merged; `reasoning_effort` replaces the fragment-based map entirely.

### Profile Lookup (model_profile, line 1864)

1. Exact key match in `model_profiles` BTreeMap.
2. ASCII-case-insensitive fallback (first matching key).
3. `model_profiles_for_resolved_model` resolves by: final backend model, then configured upstream model, then request model, then `*` wildcard.

### Env Overrides (apply_env_overrides, line 2342)

Each `LLMCONDUIT_*` env var overrides the YAML value. Key env vars:

| Env Var | Overrides |
|---|---|
| `LLMCONDUIT_UPSTREAM_API_KEY` | `upstream_api_key` |
| `OPENAI_API_KEY` | `upstream_api_key` (fallback if no `LLMCONDUIT_*` set) |
| `LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON` | `upstream_chat_kwargs` (wholesale JSON replace) |
| `LLMCONDUIT_API_LOG_BODY_MODE` | `api_log_body_mode` (`metadata` or `redacted_payload`) |
| `LLMCONDUIT_UPSTREAM_REQUEST_LOG_BODY_MODE` | `upstream_request_log_body_mode` |
| `LLMCONDUIT_PRICE_TABLE_JSON` | `price_table` (wholesale JSON replace) |
| `BRAVE_SEARCH_API_KEY` | `brave_api_key` |

`LLMCONDUIT_API_TOKEN` and `LLMCONDUIT_ALLOW_UNAUTHENTICATED_API` are environment-only public API
security controls rather than `PersistedConfig` overrides. They must never be written into YAML.

---

## Current Live Config (`~/.config/llmconduit/config.yaml`)

This section is descriptive only; repository changes do not edit the live file or service. Because
the described listener is non-loopback, a future restart also requires an environment-only
`LLMCONDUIT_API_TOKEN` (preferred) or the explicit insecure override. Configure that deployment
prerequisite separately before restarting.

The live config binds `0.0.0.0:5022` with a single upstream (`local-vllm` at
`http://localhost:8000/v1`) and three model profiles:

- **DeepSeek-V4-Flash-DSpark**: `template_family: deepseek`, roles with inline system->developer
  rewrite, rejects unlisted roles via `*: { action: reject }`.
- **Kimi-K2.7-Code**: `template_family: kimi`, simple kwargs.
- **GLM-5.2-NVFP4**: `reasoning_effort_map` maps Claude's effort ladder to GLM's
  `chat_template_kwargs` vocabulary. `clear_thinking: false` to preserve CoT across turns.

Request logging at `~/.local/share/llmconduit/upstream-requests.jsonl`, debug rotation at 24h,
per-turn capture at `~/.local/share/llmconduit/turns`, timeout 1800s.
