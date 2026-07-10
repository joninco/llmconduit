# Updating the docs

These docs are **generated from code**. When the code changes, regenerate affected docs by running the steps below.

## Quick command

```bash
# Regenerate everything
for doc in routes adapters engine config config.example.yaml upstream tools observability cli dashboard; do
  echo "==> regenerating docs/$doc.md"
  # follow the recipe for each below
done
```

## Per-doc recipe

### routes.md

Source: `src/http.rs`

Extract: all `.route()` calls from the `Router::new()` builder at the bottom of the file. For each route, get:
- HTTP method
- Path
- Handler function name (the second arg to `.route()`)
- Line number of the `.route()` call

Also included: `/dashboard/api/*` routes from `src/dashboard_api.rs`, dashboard auth routes from `src/dashboard_auth.rs`, dashboard UI routes from `src/dashboard_ui.rs`, dashboard WS from `src/dashboard_ws.rs`, debug UI from `src/debug_ui.rs`.

Format: markdown table with columns `Method | Path | Handler | Line | Description`. Add a section per handler with expanded context from comments.

### adapters.md

Sources:
- `src/adapters/mod.rs` — module declarations
- `src/adapters/anthropic_to_responses.rs` — `pub fn` exports
- `src/adapters/chat_completions.rs` — `pub fn` exports (this is the actual Chat adapter at HTTP layer)
- `src/adapters/chat_to_responses.rs` — `pub fn`, `pub struct`, `pub enum` exports (engine-internal, not HTTP-facing)
- `src/adapters/responses_to_anthropic/mod.rs`, `collector.rs`, `conformance.rs` — `pub fn` exports
- `src/adapters/responses_to_chat.rs` — `pub fn` exports

Then cross-reference with `src/http.rs` to find which are actually called from the HTTP layer.

Format: sections per module with the HTTP-callable entry points listed first, then full public API, with line numbers.

### engine.md

Source: `src/engine.rs`

Extract all public and major internal functions, grouped into categories:
- Core entry points: `stream_responses`, `stream_responses_with_api_call_id`, `run_turn`
- Model resolution: `resolve_request_model`
- Profile shaping: system prompt prefix, roles, lowering
- Tool loop: `handle_tool_calls`, `run_web_search`, `run_image_analysis`
- E1 repair: repair round, `push_shaped`, `closed_tool_set_note`
- G3 budget: input token estimation, max_output_tokens capping
- G4 image agent: image stripping, analyzeImage injection
- E2b residual safety: image rejection after G4
- Replay: replay prefix, store integration
- Dashboard telemetry: flow store, metrics, abort hub
- merge_adjacent: tail-scoped merge before upstream send

For each function: function name, line number, short description, return type / side effects.

### config.md

Source: `src/config.rs`

Dump all public structs in the config hierarchy starting from `PersistedConfig`:

```rust
struct PersistedConfig { line, fields }
struct PersistedModelProfile { line, fields }
struct RolesConfig { line, fields, validate() rules }
struct RoleRule { line, fields }
struct CapabilitiesConfig { line, fields }
struct UpstreamConfig { line, fields }
// all referenced types...
```

Then document resolution methods: `resolve_*` functions, their lookup order, default values.

### config.example.yaml

Source: `src/config.rs` struct layout + `~/.config/llmconduit/config.yaml`

Generate a real working YAML example that matches the actual struct hierarchy, NOT a fictional version. Include DeepSeek and GLM profiles as examples. Comment every section. Include pricing, roles, reasoning_effort_map, routing, env cheat sheet.

### upstream.md

Source: `src/upstream.rs`

Document:
- `struct ReqwestUpstreamClient` and all ~19 methods
- Failover: `ProviderCooldownState`, `ProviderMetrics`, `FailoverUpstreamProvider`, `FailoverUpstreamClient` + loop pseudocode
- Routing: `RoutingUpstreamProvider`, `RouteUpstreamProvider`, `ModelRouteSpec`, `RoutingModelCatalog`, `RoutingUpstreamClient` + dispatch flow
- `finalize_request_for_backend`: 5-step leaf finalization pipeline
- Cooldown: `Cooling`/`Down` states, `DOWN_THRESHOLD=3`
- `prefetch_first_chunk`: failover race (NOT availability warming)
- Health: `ProviderHealth`, passive (no probes)

### tools.md

Sources: `src/search.rs`, `src/engine.rs`, `src/vision/`, `src/tool_delta_gate.rs`

Document:
- `web_search`: `SearchClient` trait, `BraveSearchClient`, `run_web_search`, limits
- `image_analysis`: `VisionClient`, `analyzeImage`, gating, limits
- E1 repair: hallucinated-tool recovery, synthetic tool results, `CLOSED_TOOL_SET_NOTE`, `ToolDeltaGate`
- Tool loop flow diagram

### observability.md

Sources: `src/metrics.rs`, `src/turn_capture.rs`, `src/dashboard_*.rs`, `src/tool_delta_gate.rs`, `src/sse_guard/`, `src/redaction.rs`, `src/log_rotation.rs`, `src/request_log.rs`, `src/replay.rs`, `src/raw.rs`

For each component: file path, entry point function/struct, description, integration with engine/HTTP.

### dashboard.md

Sources: `src/dashboard_auth.rs`, `src/dashboard_api.rs`, `src/dashboard_flow.rs`, `src/dashboard_ws.rs`, `src/dashboard_ui.rs`

Document:
- Auth: `DashboardAuth`, `from_env`, `authenticate`, `authenticate_ws`, `require_session`, env vars
- REST API: endpoint table with method/path/handler/description
- Flow store: `DashboardFlowStore`, `FlowRecord`, `FlowStatus`, caps (512 records, 30 min TTL, 64 MiB)
- WebSocket: `DashboardFrame`, `DashboardPayload` arms, `SeqCursors`, batched envelope
- UI: `DASHBOARD_DIST`, CSP, login page HTML

### cli.md

Sources: `src/main.rs`, `src/cli.rs`

Document every subcommand and flag:
- `configure` — interactive prompts
- `start` — flags: `--raw`, `--with-debug-ui`, `--model-route`
- `analyze-log` — what it analyzes
- Global flags: `--with-debug-ui`

## When to regenerate

| Trigger | Regenerate |
|-|-|
| New route or handler changed | `routes.md` |
| New adapter or entry point | `adapters.md` |
| New engine function or responsibility | `engine.md` |
| New config field or struct | `config.md` + `config.example.yaml` |
| Upstream routing/failover change | `upstream.md` |
| New server-side tool | `tools.md` |
| New observability component | `observability.md` |
| Dashboard endpoint or auth change | `dashboard.md` |
| CLI command or flag change | `cli.md` |

## Verification

After regenerating, spot-check:
1. Every file opens without syntax errors
2. Line numbers in the doc match actual lines in source
3. No fictional examples (config.example.yaml must match actual structs)
4. No paths to files that don't exist
