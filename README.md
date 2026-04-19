# resp2chat

A high-performance API gateway that translates [OpenAI Responses API](https://platform.openai.com/docs/api-reference/responses) requests into [Chat Completions](https://platform.openai.com/docs/api-reference/chat) format, enabling Codex and other Responses API clients to use any chat-completions backend — sglang, vLLM, Ollama, or any OpenAI-compatible server.

## Key Features

- **Full Responses API translation** — streaming SSE events, multi-turn conversations, tool calls, reasoning, and function call arguments all translated bidirectionally
- **94% KV cache hit rate** — request structure is optimized for sglang's radix attention; the ~29,500 token shared prefix (system prompt + tools) is automatically reused across sessions
- **Server-side web search** — proxy-side Brave Search integration; when unconfigured, web search tools are silently stripped so clients fall back to their own MCP tools
- **Client-side MCP compatibility** — function tools, image generation, and custom tools pass through transparently for client MCP servers to handle
- **Response replay cache** — SHA-256 keyed FIFO cache with configurable capacity for instant replays of identical requests
- **Production hardened** — stream timeouts, bounded caches, error sanitization, atomic config file permissions, client connect timeouts

## Quick Start

```bash
# Build
cargo build --release

# Interactive configuration
./target/release/resp2chat configure

# Start the gateway
./target/release/resp2chat start
```

The gateway listens on `http://127.0.0.1:4000` by default. Point your Responses API client at it:

```bash
# Codex CLI
cat >> ~/.codex/config.toml << 'EOF'
[model_providers.r2c]
name = "resp2chat"
base_url = "http://127.0.0.1:4000/v1"
wire_api = "responses"
requires_openai_auth = false

[profiles.r2c]
model_provider = "r2c"
model = "Qwen3.5"
EOF

codex -p r2c "what files are in this directory?"
```

```bash
# curl
curl -N http://localhost:4000/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"model":"Qwen3.5","input":"Hello!","stream":true}'
```

## SSE Event Stream

resp2chat emits the full Responses API event sequence:

```
response.created
response.in_progress
  response.output_item.added
    response.content_part.added
      response.output_text.delta ...
    response.output_text.done
    response.content_part.done
  response.output_item.done
  response.reasoning_summary_part.added
    response.reasoning_summary_text.delta ...
  response.reasoning_summary_part.done
  response.function_call_arguments.delta ...
  response.function_call_arguments.done
response.completed  (or response.incomplete)
```

The `response.completed` event includes a full `ResponseResource` with `id`, `object`, `created_at`, `status`, `output[]`, `model`, and `usage`.

## Supported Tools

| Tool Type | Behavior |
|-|-|
| `function` | Passed to upstream model; function calls returned to client |
| `web_search` | Proxy-side via Brave API (or stripped if no API key) |
| `image_generation` | Silently stripped; client uses MCP server instead |
| `local_shell` | Passed through as function call |
| `custom` | Passed through as function call |
| `namespace` | Flattened to individual functions for upstream |
| `tool_search` | Client-side execution |

## Configuration

Config file at `~/.config/resp2chat/config.yaml` (created by `resp2chat configure`):

```yaml
bind_addr: "127.0.0.1:4000"
upstream_base_url: "http://127.0.0.1:8000/v1"
upstream_model: "Qwen3.5"
brave_api_key: "your-key-here"  # optional
```

All fields support environment variable overrides:

| Field | Default | Env Var |
|-|-|-|
| `bind_addr` | `127.0.0.1:4000` | `RESP2CHAT_BIND_ADDR` |
| `upstream_base_url` | `http://127.0.0.1:8000/v1` | `RESP2CHAT_UPSTREAM_BASE_URL` |
| `upstream_api_key` | None | `RESP2CHAT_UPSTREAM_API_KEY` (fallback: `OPENAI_API_KEY`) |
| `upstream_model` | None | `RESP2CHAT_UPSTREAM_MODEL` |
| `brave_api_key` | None | `BRAVE_SEARCH_API_KEY` |
| `brave_max_results` | 5 | `RESP2CHAT_BRAVE_MAX_RESULTS` |
| `request_timeout_secs` | 60 | `RESP2CHAT_REQUEST_TIMEOUT_SECS` |
| `connect_timeout_secs` | 10 | `RESP2CHAT_CONNECT_TIMEOUT_SECS` |
| `max_web_search_rounds` | 5 | `RESP2CHAT_MAX_WEB_SEARCH_ROUNDS` |
| `max_replay_entries` | 1000 | `RESP2CHAT_MAX_REPLAY_ENTRIES` |
| `flatten_content` | true | `RESP2CHAT_FLATTEN_CONTENT` |

Config files are created with `0600` permissions on Unix.

## Architecture

```
Codex / Client
    │
    ▼  POST /v1/responses (Responses API)
┌──────────┐
│ resp2chat │──▶ validate_request() ──▶ lower_request()
│  gateway  │         │                      │
│          │    tool_choice          ResponsesRequest
│          │    validation          → ChatCompletionRequest
│          │                              │
│          │                              ▼
│          │                     POST /v1/chat/completions
│          │                              │
│          │                       ┌──────┴──────┐
│          │                       │   sglang /   │
│          │                       │   vLLM /     │
│          │                       │   upstream   │
│          │                       └──────┬──────┘
│          │                              │
│          │◀── StreamState ◀── SSE chunks (ChatCompletionChunk)
│          │    apply_chunk()
│          │    finalize()
│          │         │
│          │         ▼
│          │    StreamEmission variants
│          │    → SSE events (Responses API format)
└──────────┘
    │
    ▼  SSE stream to client
```

### Key Components

| File | Purpose |
|-|-|
| `src/engine.rs` | Request orchestration, multi-turn tool loop, web search, replay |
| `src/adapters/responses_to_chat.rs` | Responses API → Chat Completions translation |
| `src/adapters/chat_to_responses.rs` | StreamState machine: Chat chunks → Responses SSE events |
| `src/upstream.rs` | HTTP client, request sanitization, logging |
| `src/config.rs` | Configuration with env overrides and file permissions |
| `src/replay.rs` | SHA-256 keyed FIFO response cache |
| `src/search.rs` | Brave Search API client |
| `src/models/` | Request/response types for both API formats |
| `src/error.rs` | Error handling with client/server message separation |
| `src/ui.rs` | Optional TUI monitor (`--ui` flag) |

## Endpoints

| Endpoint | Method | Description |
|-|-|-|
| `/v1/responses` | POST | Responses API (streaming) |
| `/v1/models` | GET | Proxy to upstream models list |
| `/healthz` | GET | Health check |

## Request Logging

Enable upstream request logging for debugging or KV cache analysis:

```yaml
upstream_request_log_path: "/tmp/resp2chat-upstream.jsonl"
```

Analyze consecutive requests for prefix stability:

```bash
resp2chat analyze-log
```

## systemd Service

```ini
# ~/.config/systemd/user/resp2chat.service
[Unit]
Description=resp2chat Responses-to-Chat gateway

[Service]
ExecStartPre=cargo build --release --manifest-path=/path/to/resp2chat/Cargo.toml
ExecStart=/path/to/resp2chat/target/release/resp2chat start
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now resp2chat
systemctl --user restart resp2chat  # rebuilds from source
journalctl --user -u resp2chat -f   # view logs
```

## Testing

```bash
cargo test          # 129 tests (103 unit + 26 integration)
cargo clippy        # zero warnings
```

## License

MIT
