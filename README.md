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

Optional fallback upstreams:

```yaml
upstream_failure_cooldown_secs: 30
fallback_upstreams:
  - name: "backup"
    upstream_base_url: "https://openrouter.ai/api/v1"
    upstream_api_key: "..."
    upstream_model: "openai/gpt-4.1-mini"
    upstream_chat_kwargs:
      provider:
        order:
          - z-ai
        allow_fallbacks: true
```

When configured, llmconduit tries the primary upstream first. If a provider
fails before producing the first chat chunk, it is skipped for
`upstream_failure_cooldown_secs` and the next configured provider is tried.
Failures after streaming has started still fail the active request, but mark
that provider unhealthy for subsequent requests. A fallback `upstream_model` is
optional. When set, llmconduit sends chat requests to that provider with the
configured model and filters that provider's `/v1/models` response down to only
that model. When unset, request models and model lists pass through unchanged.
Fallback `upstream_chat_kwargs` are provider-specific freeform chat completion
defaults. They are merged into requests only when that fallback is selected,
with per-model kwargs and explicit request values taking precedence.

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
