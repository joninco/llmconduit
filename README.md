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

Minimal config:

```yaml
bind_addr: "127.0.0.1:4000"
upstream_base_url: "http://127.0.0.1:8000/v1"
upstream_model: "Qwen3.5"
```

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
primary upstream model catalogs. If a request omits `model`, passes a blank
model, or requests a model that is not currently available, llmconduit uses the
first model from the first upstream with a catalog entry. Requested model names
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
parallel tool use.
### Roles

A per-profile `roles` block maps whole-message roles before the conversation is
sent upstream. It is fail-closed: a role with no matching rule is rejected with
HTTP 400. With no `roles` block configured, messages pass through **verbatim** - all
role shaping is opt-in.

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

```toml
[model_providers.llmconduit]
name = "llmconduit"
base_url = "http://127.0.0.1:4000/v1"
wire_api = "responses"
requires_openai_auth = false

[profiles.llmconduit]
model_provider = "llmconduit"
model = "Qwen3.5"
```

```bash
codex -p llmconduit "what files are in this directory?"
```

## Docker

```bash
docker build -t llmconduit .
docker run --rm -p 4000:4000 \
  --add-host=host.docker.internal:host-gateway \
  -e LLMCONDUIT_UPSTREAM_BASE_URL=http://host.docker.internal:8000/v1 \
  llmconduit
```

## Endpoints

| Endpoint | Description |
|-|-|
| `POST /v1/responses` | OpenAI Responses API |
| `POST /v1/chat/completions` | OpenAI Chat Completions API |
| `POST /v1/messages` | Anthropic Messages API |
| `GET /v1/models` | Proxied model list |
| `GET /healthz` | Health check |
| `GET /debug` | Debug UI when started with `--with-debug-ui` |

## Environment

Common overrides:

```text
LLMCONDUIT_BIND_ADDR
LLMCONDUIT_UPSTREAM_BASE_URL
LLMCONDUIT_UPSTREAM_API_KEY
LLMCONDUIT_UPSTREAM_MODEL
LLMCONDUIT_SYSTEM_PROMPT_PREFIX
LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON
LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS
LLMCONDUIT_BRAVE_MAX_RESULTS
LLMCONDUIT_REQUEST_TIMEOUT_SECS
LLMCONDUIT_CONNECT_TIMEOUT_SECS
LLMCONDUIT_MAX_WEB_SEARCH_ROUNDS
LLMCONDUIT_MAX_REPLAY_ENTRIES
LLMCONDUIT_FLATTEN_CONTENT
BRAVE_SEARCH_API_KEY
OPENAI_API_KEY
```

`OPENAI_API_KEY` is used as a fallback upstream API key.

## Request Logs

Set this in config to write upstream chat requests as JSONL:

```yaml
upstream_request_log_path: "/tmp/llmconduit-upstream.jsonl"
```

Then inspect prefix stability:

```bash
llmconduit analyze-log
```

## Test

```bash
cargo test
```

## License

MIT
