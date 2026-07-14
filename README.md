# llmconduit

LLM API gateway for local and OpenAI-compatible chat-completions backends.

It accepts OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages
requests, normalizes them, and forwards them to an upstream
`/v1/chat/completions` server. It can also run server-side tools such as Brave
Search.

## Build

```bash
cargo build --release
```

## Configure

```bash
./target/release/llmconduit configure
```

The default config path is:

```text
~/.config/llmconduit/config.yaml
```

Configuration is loaded at startup. Restart llmconduit after editing the file.

Minimal config:

```yaml
bind_addr: "127.0.0.1:4000"
upstream_base_url: "http://127.0.0.1:8000/v1"
upstream_model: "Qwen3.5"
```

Loopback listeners may run without client authentication. For any wildcard or
other non-loopback bind, startup requires an environment-only API token unless
you explicitly opt into insecure development serving:

```bash
export LLMCONDUIT_API_TOKEN='replace-with-a-dedicated-gateway-token'
```

Clients may present it as `Authorization: Bearer …` or `x-api-key`. The token
protects every `/v1/*` route and is never a YAML field. `/` and `/health` stay
public; dashboard authentication is separate. The development-only escape
hatch is `LLMCONDUIT_ALLOW_UNAUTHENTICATED_API=1`.

Multi-upstream model routing:

```yaml
upstreams:
  - name: "local"
    upstream_base_url: "http://127.0.0.1:8000/v1"
  - name: "openrouter"
    upstream_base_url: "https://openrouter.ai/api/v1"
    upstream_api_key: "..."
```

When `upstreams` is configured, llmconduit exposes the ordered union of the
primary upstream model catalogs. Chat Completions and Anthropic Messages keep
the existing default-model behavior for a missing, blank, or unavailable model.
Raw Responses requests require `model` (400 when missing and 404 when explicitly
unknown). Requested model names
are normalized against the catalogs, so aliases such as different case or
punctuation route to the exact model id exposed by the backend. If multiple
upstreams expose the same model id, the first upstream wins.

Optional nested fallback providers:

```yaml
upstreams:
  - name: "local"
    upstream_base_url: "http://127.0.0.1:8000/v1"
    fallback_upstreams:
      - name: "backup"
        upstream_base_url: "https://openrouter.ai/api/v1"
        upstream_api_key: "..."
        upstream_model: "openai/gpt-4.1-mini"
        exposed_model: "GPT-4.1-mini"
        upstream_chat_kwargs:
          provider:
            order:
              - z-ai
            allow_fallbacks: true
```

If a selected upstream fails before producing the first chat chunk, only that
upstream's nested `fallback_upstreams` are tried. llmconduit does not treat the
next model-routing upstream as a failure fallback. Fallback models are not shown
in `/v1/models` unless `exposed_model` is set. A fallback `upstream_model` is
optional; when set, fallback requests use that model, otherwise they keep the
routed primary model id. `exposed_model` advertises a fallback model under a
client-facing alias and routes requests for that alias to the declaring fallback
provider.
Fallback `upstream_chat_kwargs` are merged only when that fallback is selected,
with per-model kwargs and explicit request values taking precedence.

The legacy top-level `upstream_*` and `fallback_upstreams` settings still work
when `upstreams` is not configured.

Global and per-model request defaults:

```yaml
system_prompt_prefix: |
  Shared instructions prepended to every request.

upstream_chat_kwargs:
  stream_reasoning: true

model_profile_templates:
  thinking:
    separate_reasoning: true
    chat_template_kwargs:
      enable_thinking: true

model_profiles:
  GLM-5.1:
    extends:
      - thinking
    chat_template_kwargs:
      clear_thinking: false

  Kimi-K2.6:
    extends:
      - thinking
    system_prompt_prefix: |
      Extra Kimi-specific instructions.
    chat_template_kwargs:
      preserve_thinking: true

  GLM-5.2:
    extends:
      - thinking
    upstream_chat_kwargs:
      parallel_tool_calls: true
```

`system_prompt_prefix` is prepended to all Responses, Chat Completions, and
Anthropic Messages requests. A profile-specific prefix is appended after the
global prefix. `upstream_chat_kwargs` merge in this order: top-level defaults,
matched model profile templates, matched model profile, then explicit request
values. In model profiles and templates, extra profile-level keys are shorthand
for upstream chat kwargs; the explicit `upstream_chat_kwargs` wrapper still
works and overrides the shorthand when both set the same key. When a profile
`extends` multiple templates, the `extends` list is applied in declaration
order: later entries override earlier ones, and the profile's own fields
override all templates.

### Reserved `*` profile

A profile keyed `*` is a pure fallback for per-model settings. When a request
names a model that no specific `model_profiles` entry matches, the `*` profile
stands in as that model's profile: its `upstream_chat_kwargs` and
`system_prompt_prefix` apply. When a specific profile DOES match, the `*`
profile is not consulted at all - an explicit match never inherits unset fields
from `*`. The `*` profile can itself `extend` templates, so extending a shared
template is the way to give `*` and explicit profiles common defaults. Use
`model_profile_templates` (`extends`) to share fields between explicit
profiles, not `*`.

Per-model profile matching precedence, highest to lowest:

1. The request model - matched by name (case-insensitive) against `model_profiles`.
2. The resolved/upstream model (after `upstream_model` rewriting) - matched by name.
3. The reserved `*` profile - used only when neither 1 nor 2 matches.

Top-level config is the base below all profiles: `upstream_chat_kwargs` is the
deep-merge base, and `system_prompt_prefix` is always prepended. Client request
values still override profile settings, as described above.

```yaml
model_profiles:
  # Fallback for any model without an explicit profile.
  "*":
    upstream_chat_kwargs:
      chat_template_kwargs:
        enable_thinking: true

  GLM-5.2:
    upstream_chat_kwargs:
      chat_template_kwargs:
        enable_thinking: false
```

With this config, a request for `GLM-5.2` uses only the `GLM-5.2` profile
(`enable_thinking: false`); the `*` profile contributes nothing. A request for
any other model (e.g. `Qwen-3`) falls back to `*` (`enable_thinking: true`).

### Model capabilities

A profile's `capabilities` block overrides the Anthropic model capabilities
advertised on `/v1/models` for Anthropic clients.

```yaml
model_profiles:
  "*":
    capabilities:
      thinking:
        types: [adaptive, enabled]
      effort:
        levels: [max, xhigh, high, medium, low, minimal, none]
      image_input: false
```

- `supported` is the only knob and defaults to `true`. The simple caps (`batch`,
  `citations`, `code_execution`, `image_input`, `pdf_input`,
  `structured_outputs`) accept a bare bool as shorthand for `{supported: <bool>}`.
- `thinking.types`, `effort.levels`, and `context_management.features` list the
  advertised sub-entries; each inherits the cap's `supported` flag.
- Unknown cap keys, effort levels, thinking types, and context-management features
  are rejected at load.
- A configured cap replaces the base (upstream-supplied, else the default
  capabilities) for that cap key, wholesale; unconfigured caps keep the base.
  A matched profile without a `capabilities` block gets no fill-in from the `*`
  profile. Caps resolve per upstream id: an id-keyed profile, else the first alias
  whose `upstream_model` targets the id, else the reserved `*` profile.

### Responses capabilities

Responses-only feature support is declared separately and resolved by provider
plus served model. Global declarations are overlaid by the selected provider
and then the matching model profile:

```yaml
responses_capabilities:
  parallel_tool_calls: false
  structured_outputs: [text, json_object, json_schema]
  reasoning_summary: upstream
  encrypted_reasoning: unsupported
  input_image: placeholder
  input_file: unsupported
  truncation_auto: unsupported
  text_verbosity: unsupported
  service_tiers: []
  prompt_cache_key: gateway_hash
  prompt_cache_retention: []
```

An advanced feature omitted from the resolved declaration is unsupported, apart
from the existing safe placeholder policy for non-native image input. A request
that needs an unsupported primary capability receives an OpenAI-shaped
400 naming the parameter. An incapable nested fallback is removed only for that
request, without cooldown or health penalty; routing never jumps to a different
primary merely to gain a capability. Hosted OpenAI Files, Conversations, Code
Interpreter, and Computer Use remain outside llmconduit's implemented surface.

`prompt_cache_key: gateway_hash` hashes the opaque key before local use and
does not forward the original; `upstream` forwards it. Raw images still never
reach a non-native backend.

### Reasoning effort

A profile's `reasoning_effort` block shapes the upstream `reasoning_effort` field
and controls the thinking template kwarg injected on Anthropic routes. Effort
shaping applies on `/v1/messages`, `/v1/responses`, `/v1/chat/completions`, and
`/v1/messages/count_tokens`; thinking-kwarg injection applies on the Anthropic
routes.

```yaml
model_profiles:
  "*":
    reasoning_effort:
      default: high
      map:
        low: high
        xhigh: max
        "*": high
      thinking_param_name: enable_thinking
      thinking_param_value_on: true
      thinking_param_value_off: false
```

- `map` translates effort levels case-insensitively. An explicit key wins; `*`
  rewrites any otherwise-unlisted effort.
- `default` is emitted when the client does not send an effort. Omitting it sends
  no `reasoning_effort` field.
- Anthropic requests always state thinking on/off through the configured template
  kwarg (default `enable_thinking: true`/`false`), overriding static defaults.
  A resolved `none` effort forces the off value.
- A matching profile without `reasoning_effort` is not back-filled from `*`.

The fork's advanced fragment form is also retained. It resolves at the final
provider leaf, so a routed or failover model receives its own vocabulary, and
it can place controls anywhere in the request rather than only remapping the
top-level field:

```yaml
model_profiles:
  GLM-5.2:
    reasoning_effort_default: max
    reasoning_effort_map:
      high: {chat_template_kwargs: {reasoning_effort: high}}
      max: {chat_template_kwargs: {reasoning_effort: max}}
      none: {chat_template_kwargs: {enable_thinking: false}}
```

`reasoning_effort` and the fragment-based
`reasoning_effort_map`/`reasoning_effort_default` form are mutually exclusive
within a resolved profile.

### Example: GLM-5.2 on vLLM

```yaml
model_profiles:
  GLM-5.2:
    reasoning_effort:
      map:
        none: none
        minimal: none
        low: high
        medium: high
        xhigh: max
```

`parallel_tool_calls` is a typed default: when a client omits it, the resolved
`upstream_chat_kwargs.parallel_tool_calls` default applies, and an explicit
client value always wins. The default is the global `upstream_chat_kwargs`
deep-merged with the matching profile, so a profile value overrides the global
one. The Anthropic (`/v1/messages`) route has no client field for it, so that
resolved default is the only way to control it there. Setting it to `true` (as
on `GLM-5.2` above) lets Claude Code fan out independent tool calls in one turn;
setting it to `false` forces sequential calls for a model that mishandles
parallel tool use. llmconduit always forces `false` while a gateway-owned Brave
Search or image-analysis tool is active, because those internal tool/result
loops must remain sequential.

### Token counting

`POST /v1/messages/count_tokens` applies the same model resolution, system
prefix, role rules, tools, chat-template defaults, and backend finalization as
generation, then calls the upstream server-root `/tokenize` endpoint. An
upstream without `/tokenize` returns an Anthropic `not_found_error`; that
unsupported result is cached for the lifetime of the process.

### Roles

A per-profile `roles` block maps whole-message roles before the conversation is
sent upstream. It is fail-closed: a role with no matching rule is rejected with
HTTP 400. With no `roles` block configured, the compatibility behavior remains:
`developer` is rewritten to `system`, and system messages are hoisted. Adding a
`roles` block opts that model into the exact policy below, including arbitrary
role pass-through.

`roles` holds an optional `merge_adjacent` list plus a map of role name to a
rule, or an ordered list of rules. `*` is the wildcard role: it matches any role
that has no explicit key. A single rule is shorthand for a one-element list. In
a list, the first rule whose `when` matches wins; a rule with no `when` always
matches, so put it last as the catch-all.

Per-rule keys:

- `when` (`leading` / `inline` / `always`, default `always`): `leading` matches
  index 0, `inline` matches index > 0, `always` matches any position. Omitting
  `when` is equivalent to `always`; spell it out only to be explicit.
- `action` (`accept` / `reject` / `drop` / `rewrite`, default `accept`):
  `accept` keeps the message in place; `reject` returns HTTP 400; `drop` removes
  the message; `rewrite` renames the role, staying its own turn in place.
- `target_role` (string, required with `action: rewrite`): the new role name.
- `tag` (string, optional): wrap the message content in `<tag>...</tag>`.
- `tag_attributes` (map<string,string>, requires `tag`): render attributes on
  the opening tag, alphabetical by key, XML-escaped (`&` `"` `<`).

Tagging gives the model extra context about a block. For example, rewriting a
`developer` message to `system` with `tag: system-instruction` and
`tag_attributes: {description: "IMPORTANT system message. You MUST follow this with high priority!"}`
wraps the content as
`<system-instruction description="IMPORTANT system message. You MUST follow this with high priority!">...</system-instruction>`.

`merge_adjacent` is a post-pass keyed on the **final** role (after rewrites). It
coalesces each maximal run of consecutive messages that share a final role in
the list into one content-only message joined with `\n\n`. There is no
inline/leading distinction at this level - it only looks at the role messages
end up as and whether they are adjacent. Folding system and tool into `user` is
`rewrite` to `user` plus `merge_adjacent: [user]`, which coalesces the
resulting adjacent user messages into one while keeping their relative order.

Resolution order for a message: the explicit role key, then the `*` wildcard,
then fail-closed `reject`.

```yaml
model_profiles:
  # Full-role, system inline ANYWHERE; tool role supported (GLM-5.2, Kimi K2.7).
  # Both group tool runs in-template, so do NOT set merge_adjacent on `tool`.
  GLM-5.2:
    roles:
      "*":       { action: reject }
      user:      {}
      assistant: {}
      tool:      {}
      system:    {}
      developer: { action: rewrite, target_role: system }

  # System-FIRST only (Qwen3.5 raises on a non-first system message). An INLINE
  # system or developer message is rewritten to `user` in place; the index-0
  # system message stays system and a leading developer message is rewritten to
  # system, so Qwen never sees a non-first system.
  Qwen3.5:
    roles:
      "*":       { action: reject }
      user:      {}
      assistant: {}
      tool:      {}
      system:
        - { when: inline, action: rewrite, target_role: user }
        - {}
      developer:
        - { when: inline, action: rewrite, target_role: user }
        - { action: rewrite, target_role: system }

  # System-less model (Gemma): only `user`/`assistant` exist. Fold system and
  # tool into `user` and coalesce the adjacent user runs.
  Gemma:
    roles:
      merge_adjacent: [user]
      "*":       { action: reject }
      user:      {}
      assistant: {}
      system:    { action: rewrite, target_role: user }
      tool:      { action: rewrite, target_role: user, tag: tool_result }
```

Optional Brave Search:

```yaml
brave_api_key: "..."
```

Optional vision offload — forward images to a separate vision-capable model instead
of the primary upstream:

```yaml
image_agent_enabled: true
vision_url: "http://127.0.0.1:8001/v1"
vision_model: "Qwen3-VL"
```

Any image that still reaches a backend without native vision support (whether or not
the agent above is active, e.g. no `vision_url` configured) is degraded instead of
forwarded raw:

```yaml
# placeholder (default): replace the image in place with an instructive text note so
#   the model asks the user to describe it / requests text, instead of guessing.
# reject: fail the turn before dispatch with an HTTP 400 (the provider is never
#   contacted, so it is never cooled down or failed over).
unsupported_image_policy: placeholder
```

### Responses state and replay

Responses `store` and `previous_response_id` use a bounded state store. Memory
is the default; SQLite adds continuity across restarts:

```yaml
response_store:
  backend: memory       # memory | sqlite
  path: null            # required for sqlite
  max_entries: 1000
  retention_hours: 720

replay:
  enabled: false
  max_entries: 100
```

`store:true` persists completed/incomplete canonical history;
`store:false` never does. Failed and cancelled responses are not referenceable.
Responses `instructions` accepts either the standard string form or a normalized
Response input-item array (including easy messages). Current instructions replace,
rather than inherit, previous instructions.
Private visible-history replay is a separate optimization, defaults off, and
can be bypassed per request with the consumed `llmconduit_replay:false`
extension when the server feature is enabled.

## Run

```bash
./target/release/llmconduit start
```

Useful flags:

```bash
./target/release/llmconduit start --raw
./target/release/llmconduit start --with-debug-ui
```

The gateway listens on `http://127.0.0.1:4000` by default.

## Codex

Use the standard Responses base URL plus the checked Codex model catalog; do
not change `/v1/models` to Codex's private catalog shape. See
[docs/codex.md](docs/codex.md) for the complete provider configuration and
version-specific catalog guidance.

## Docker

The Docker build compiles and embeds the complete dashboard in a separate Node
stage; Node and the frontend sources are not present in the final image.

```bash
docker build -t llmconduit .
docker run --rm -p 4000:4000 \
  --add-host=host.docker.internal:host-gateway \
  -e LLMCONDUIT_UPSTREAM_BASE_URL=http://host.docker.internal:8000/v1 \
  -e LLMCONDUIT_API_TOKEN="$LLMCONDUIT_API_TOKEN" \
  llmconduit
```

The image binds `0.0.0.0:4000`, so `LLMCONDUIT_API_TOKEN` is a required
deployment input unless the explicit insecure API override is supplied.

To expose `/debug` and `/dashboard`, replace the final line with
`llmconduit start --with-debug-ui`.

Non-loopback dashboard access requires authentication by default. To deliberately
run tokenless on a trusted network, set `LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1`;
startup logs a prominent warning because `/debug` and `/dashboard` will be open.

## Endpoints

| Endpoint | Description |
|-|-|
| `POST /v1/responses` | OpenAI Responses API |
| `POST /v1/chat/completions` | OpenAI Chat Completions API |
| `POST /v1/messages` | Anthropic Messages API |
| `GET /v1/models` | Proxied model list |
| `GET /metrics` | Raw Prometheus passthrough from the first configured primary upstream |
| `GET /health` | Health check (public) |
| `GET /debug` | Debug UI when started with `--with-debug-ui` |

`/metrics` preserves the primary backend's status, body, and eligible end-to-end
headers. It does not fail over, because metrics from a different provider would
describe different capacity and process state. `/dashboard/api/metrics` remains
llmconduit's separate gateway-owned dashboard telemetry surface.

## Environment

Common overrides:

```text
LLMCONDUIT_BIND_ADDR
LLMCONDUIT_UPSTREAM_BASE_URL
LLMCONDUIT_UPSTREAM_API_KEY
LLMCONDUIT_UPSTREAM_MODEL
LLMCONDUIT_SYSTEM_PROMPT_PREFIX
LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON
LLMCONDUIT_API_LOG_BODY_MODE
LLMCONDUIT_UPSTREAM_REQUEST_LOG_BODY_MODE
LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS
LLMCONDUIT_BRAVE_MAX_RESULTS
LLMCONDUIT_REQUEST_TIMEOUT_SECS
LLMCONDUIT_CONNECT_TIMEOUT_SECS
LLMCONDUIT_MAX_WEB_SEARCH_ROUNDS
LLMCONDUIT_MAX_REPLAY_ENTRIES
LLMCONDUIT_FLATTEN_CONTENT
LLMCONDUIT_TURN_CAPTURE_DIR
LLMCONDUIT_BACKEND_METRICS
LLMCONDUIT_API_TOKEN
LLMCONDUIT_ALLOW_UNAUTHENTICATED_API
BRAVE_SEARCH_API_KEY
OPENAI_API_KEY
```

`OPENAI_API_KEY` is used as a fallback upstream API key.

With `--with-debug-ui`, llmconduit also polls every configured primary,
fallback, and model-route backend's server-root `/metrics` endpoint for
normalized vLLM/SGLang engine health. Set `LLMCONDUIT_BACKEND_METRICS=off` to
disable those scrapes. This telemetry is observational only and never affects
routing, failover, cooldowns, or request budgeting.

## Request Logs

Set this in config to write upstream chat requests as JSONL:

```yaml
upstream_request_log_path: "/tmp/llmconduit-upstream.jsonl"
upstream_request_log_body_mode: metadata
```

API and upstream request-body logging default to `metadata`. Set the relevant
mode to `redacted_payload` only for deliberate diagnostics; payload mode still
uses shared secret and image-URI redaction. Malformed JSON is logged only as a
bounded hash/length marker. Turn capture remains separately opt-in.

Then inspect prefix stability:

```bash
llmconduit analyze-log
```

## Durable turn capture

Set `turn_capture_dir` to persist ONE self-contained JSON artifact per inference
turn — the full request+response chain, for debugging output that returned a plain
`200 OK` (e.g. a stray `<think>` tag that leaked into text, a dropped tool call).
It is opt-in and works independently of the `--with-debug-ui` dashboard:

```yaml
turn_capture_dir: "/tmp/llmconduit-turns"
# Optional: age-rotate artifacts (and sweep crash-orphaned work dirs) after N hours.
debug_log_max_age_hours: 48
```

Each instrumented turn writes `<turn_capture_dir>/<api_call_id>.json` with four
sections — `inbound_request`, `upstream_request` (translated, on-wire),
`upstream_response` (raw upstream bytes — the pre-parse ground truth), and
`served_response` (the exact bytes returned to the client) — plus outcome metadata
(`status`, `terminal_reason`, timings, per-section `{bytes, partial, encoding}`).
Diff `upstream_response` against `served_response` to localize a `<think>` leak as
upstream-emitted vs converter-introduced. Request sections are redacted (secret keys
+ image URIs); memory stays bounded (sections stream to per-turn temp files under
`<dir>/.work/<id>/`, assembled atomically via tmp→fsync→rename). Leave
`turn_capture_dir` unset to disable (zero overhead — no thread, no allocation).

## Test

```bash
cargo test
```

## License

MIT
