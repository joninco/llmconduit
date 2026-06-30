# E2E-TRACE-PLAN.md — End-to-end request traceability for llmconduit

Status: **PROPOSED** (not started). Source: Codex xhigh review (gpt-5.5, 253k tok) of
the current observability surface, 2026-06-20. Analysis only — no code written yet.

Goal: make a single request (turn) fully traceable across the three tiers
**Claude Code client → llmconduit (axum) → vLLM `/v1/chat/completions`**, such that a
reported bad turn can be localized to the boundary that broke it by grepping one ID.

---

## Current observability (baseline)

- HTTP middleware (`src/http.rs`) mints `api_call_id = api_<uuid>` per inbound request
  and logs it on every inbound event: request (headers, `body_bytes` count,
  `body_sha256`, `body_summary` key-list, no body unless ≤16KB), payload (full body
  only if ≤16KB), and `response prepared` (status, served/requested model, elapsed_ms).
- Upstream JSONL logger (`src/upstream.rs ~585-599`): `logger.log(request)` runs BEFORE
  `send_chat_request`, recording the exact lowered ChatCompletions body POSTed to vLLM
  at `~/.local/share/llmconduit/upstream-requests.jsonl`. Fires even if the request
  then fails. **Optional** (default config `upstream_request_log_path: None`; this
  repo's config enables it).
- vLLM failure path: non-2xx → error string w/ status + redacted truncated body
  (500 chars); context-overflow → shrink-and-retry `warn!` with full detail; provider
  failures → `warn!` with `cooldown_secs` + error.
- Logs → journald (`RUST_LOG=info` default). Debug UI (`--with-debug-ui` → MonitorHub)
  streams live ResponseItems over WebSocket but is OFF by default
  (`MonitorHub::disabled()` = zero overhead).

---

## Gaps

### Originally identified (verified + corrected by Codex)

**GAP 1 — `api_call_id` does not propagate past the HTTP layer — CONFIRMED.**
`grep api_call_id src/upstream.rs` → only `tool_call_id` (unrelated). The ID is
generated in `log_api_call` and attached to inbound logs only
(`src/http.rs:87,115,152`); it is NOT passed into `Gateway::stream_responses`,
`run_turn`, upstream clients, or the JSONL logger (`src/http.rs:715`,
`src/engine.rs:706`, `src/upstream.rs:463`). So `journalctl -g api_<id>` traces
inbound cleanly but the key stops at the vLLM boundary. Joining inbound (journal)
with the upstream body (JSONL) relies on timing + served_model, not a shared ID. The
JSONL logger is per-upstream not per-request → concurrent turns interleave with no
per-turn key.

Corrections:
- JSONL logger is optional, not always present (default `upstream_request_log_path:
  None`, `src/config.rs:587`).
- It DOES redact image URIs before disk write (`src/upstream.rs:463`). It does NOT
  redact secret-like JSON keys (unlike inbound body dumps at `src/http.rs:241`).
- It carries no per-turn key.

**GAP 2 — Upstream success (2xx) path is silent — CONFIRMED, BROADER than stated.**
Upstream 2xx returns directly into the stream parser with no success log
(`src/upstream.rs:628`, `src/upstream.rs:2640`). For streaming requests the HTTP
middleware `elapsed_ms` measures response *construction*, not stream completion,
because streaming responses are returned immediately (`src/http.rs:956`). Final
completion is emitted only to `MonitorHub` (`src/engine.rs:1855,1881`), which is off
by default.

Important correction: **vLLM non-2xx is not always logged.** The leaf client builds
an `AppError` with redacted body but does not itself log it (`src/upstream.rs:692`).
`AppError::into_response` logs errors only when an error becomes an HTTP response
(`src/error.rs:142`). In plain single-provider mode the app uses
`ReqwestUpstreamClient` directly (`src/lib.rs:194`). **A streaming vLLM 500 can
therefore become `response.failed` with no structured journal line.**

### Additional gaps found by Codex

1. **Generated request ID not returned to client.** `with_model_headers` only adds
   model headers (`src/http.rs:799`). If Claude Code reports a bad turn it may not
   have the conduit ID to search logs.
2. **Streaming failures/cancellations journal-silent.** Spawned `run_turn` error path
   emits monitor state + `response.failed` but no `tracing` event (`src/engine.rs:833`).
   `AppError::into_response` logs only when an error becomes an HTTP response
   (`src/error.rs:142`); streaming bypasses that.
3. **Routing success unattributable.** Routing logs only when the model string
   changes (`src/upstream.rs:1104`); exact-match + failover success can select a
   provider with no log. Failover success logs only fallback use (`src/upstream.rs:981`).
4. **Replay hit/miss + insert silent** (`src/engine.rs:1025`, `src/engine.rs:1805`).
   Replay *changes the lowered upstream payload*, so silent = cannot explain what vLLM
   actually received.
5. **Context-budget decisions silent** (`src/engine.rs:788`): candidate context floor,
   estimated input tokens, max-token cap all unlogged. High-value for vLLM
   context-overflow debugging.
6. **Tool/reasoning/SSE detail is live-only.** MonitorHub supports output / reasoning
   / function-argument / tool-phase events (`src/monitor.rs:53`) but `disabled()`
   returns immediately by default (`src/monitor.rs:257`). Reasoning signatures are
   streamed but not mirrored to monitor (`src/engine.rs:1594`).
7. **Debug UI is not durable storage.** In-memory, 30-min, 512-event retained state
   (`src/monitor.rs:9`). Useful live, insufficient after-the-fact.
8. **Redaction inconsistent across sinks.** Inbound body dumps redact secret keys +
   images (`src/http.rs:241`). The upstream JSONL logger + upstream error bodies
   redact **images only**, not secret-like JSON keys (`src/upstream.rs:463`,
   `src/upstream.rs:2963`).

---

## Ranked change list

| # | Change | Where (file:fn) | Value | Cost | Rationale |
|--:|---|---|---|---|---|
| 1 | Explicit `TraceContext` (`api_call_id` + inbound `x-request-id` + `response_id`); **return `x-llmconduit-api-call-id` to client**; forward upstream as `x-request-id`/`x-llmconduit-api-call-id`; include in JSONL | `http::log_api_call`, handlers, `Gateway::stream_responses`, `BackendChatRequest`, `ReqwestUpstreamClient::send_chat_request`, `UpstreamRequestLogger::log` | High | Med | Join key across Claude Code, conduit, JSONL, vLLM. A tracing span alone is insufficient — spawned tasks and disk logs need explicit context. |
| 2 | Log upstream attempt lifecycle: dispatch, provider/model, attempt idx, HTTP status, first-chunk received, stream end/error, elapsed-to-status / -first-chunk / -end | `logged_send_chat_request`, `stream_chat_completion`, `stream_success_response`, `FailoverUpstreamClient::prefetch_first_chunk`, `stream_after_prefetch` | High | Low-Med | Fixes silent 2xx AND most silent failure paths without logging raw tokens. |
| 3 | One structured per-turn terminal trace record (routing, replay, budget, upstream attempts, tool rounds, usage, finish reason, terminal status, error) | `Gateway::stream_responses` / `run_turn` | High | Med | One JSON object per turn beats reconstructing from ad-hoc lines. |
| 4 | Decision breadcrumbs for replay, budget, routing, failover classification, catalog cache | `find_replay_baseline`, budget block, `RoutingUpstreamClient::stream_chat_completion_with_timeout`, failover error branches, catalog loaders | Med-High | Low-Med | Explain "why did this request hit that provider/body/token budget?" |
| 5 | Persist streaming failure/cancel outcomes with trace ID | spawned `run_turn`, `next_upstream_chunk`, converter tasks | High | Low | Currently client-visible but journal-silent. |
| 6 | Tighten redaction on new log surfaces | `request_log.rs` / `UpstreamRequestLogger`, trace-record serializer | Med | Low-Med | New logs must avoid raw body/delta content by default. Exact lowered body stays only in the explicit JSONL artifact (sensitive by design). |

Order is debug-value / cost ratio: #1 is the single highest-leverage change — nothing
is greppable end-to-end until the join key exists everywhere.

---

## Risks / overhead

- **`tokio::spawn` won't preserve tracing context** reliably. Mitigation: explicit
  small `TraceContext` struct + optional span entered at major boundaries, not
  span-reliance alone.
- **JSONL schema compatibility:** adding `api_call_id` to the logged request can break
  `analyze-log` prefix-stability. Mitigation: wrap as `{ "request": ..., "trace": ... }`
  and update `analyze-log` to diff `request`, OR keep the old request line + add a
  separate trace JSONL.
- **Sensitive data:** do NOT put lowered bodies or deltas into journald. The upstream
  JSONL is already sensitive by design. New trace records → counts, hashes,
  model/provider IDs, statuses, finish reasons, bounded redacted previews only.
- **High-QPS streaming overhead:** avoid per-token journal events. Log first chunk +
  final summary + counters + errors. Per-delta tracing stays debug-only or sampled.
- **Concurrency:** current `UpstreamRequestLogger` lock is per-instance
  (`src/upstream.rs:450`); multiple clients on the same path may not share a lock.
  Mitigation: centralize JSONL writing or share locks by path.
- **vLLM header propagation:** forwarding a generated opaque request ID is low risk;
  do NOT forward user-provided secrets or full prompts in headers. Preserve inbound
  `x-request-id` as a separate parent field.

---

## Out of scope (deliberate)

- Enabling `--with-debug-ui` / MonitorHub by default (kept opt-in; gap #6/#7 addressed
  via persisted trace record instead, so the live debug UI stays zero-overhead).
- Per-token / per-delta journal logging (stays debug-only to protect hot path).
- Changing default `upstream_request_log_path` (stays opt-in; trace work assumes the
  JSONL artifact exists when enabled, surfaces counts/hashes when it does not).

## Open questions to resolve before implementation

- Trace record destination: separate trace JSONL vs embedded in the existing upstream
  JSONL (impacts `analyze-log`).
- Whether to return `x-llmconduit-api-call-id` on ALL responses or only non-streaming
  (streaming responses have already-sent headers).
- Failover-attempt counter surface: trace record only, or also a `debug!` breadcrumb
  per attempt.
