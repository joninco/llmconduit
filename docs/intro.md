# Intro

`llmconduit` is an LLM gateway that accepts Anthropic Messages, OpenAI Responses, and OpenAI Chat Completions requests, normalizes them into an internal Responses-style representation, applies per-model profile shaping (roles, reasoning effort, caps, system prompt prefix), runs a server-side tool loop with Brave Search and image analysis, then forwards to an OpenAI-compatible upstream provider (vLLM, OpenRouter, etc.) and streams the response back in the client's expected wire format.

## Layers

```
Client protocol
  → HTTP Router (axum)
  → Adapter into Responses representation
  → Gateway Engine (profile shaping, capabilities, tool loop)
  → Upstream Client (routing, failover, cooldown)
  → OpenAI-compatible provider
  → Normalized stream
  → Adapter back to client protocol
  → Client
```

## Quick file map

| Area | Files | Docs |
|-|-|-|
| Routes | `src/http.rs` | [routes.md](routes.md) |
| Adapters | `src/adapters/` | [adapters.md](adapters.md) |
| Engine | `src/engine.rs` | [engine.md](engine.md) |
| Config | `src/config.rs` + `config.yaml` | [config.md](config.md) |
| Upstream | `src/upstream.rs` | [upstream.md](upstream.md) |
| Server-side tools | `src/search.rs`, `src/vision/` | [tools.md](tools.md) |
| Dashboard | `src/dashboard_*.rs` | [dashboard.md](dashboard.md) |
| Observability | `src/metrics.rs`, `src/turn_capture.rs`, etc. | [observability.md](observability.md) |
| CLI | `src/cli.rs`, `src/main.rs` | [cli.md](cli.md) |
