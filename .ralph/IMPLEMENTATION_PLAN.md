# IMPLEMENTATION_PLAN.md — Topic F / F1: durable per-turn capture

> **Spec (oracle):** `.ralph/specs/F1-durable-turn-capture.md` (Codex-xhigh review folded in 2026-07-01).
> **Conventions:** `AGENTS.md`. **Run:** `/ralph-orchestrate --no-review --agents 1` — Sonnet-5 subagents,
> SERIAL (shared files: `config.rs`, `lib.rs`, `http.rs`, `engine.rs`, `upstream.rs`, new `turn_capture.rs`).
> Per-task Codex-xhigh review per `.ralph/REVIEW_PROTOCOL.md`.
> **To activate:** archive the current plan/goal, then
> `cp .ralph/IMPLEMENTATION_PLAN.turn-capture.md .ralph/IMPLEMENTATION_PLAN.md` and
> `cp .ralph/GOAL.turn-capture.md .ralph/GOAL.md`. Branch `durable-turn-capture`.

## Executive summary

**Status: 6/6 Codex-xhigh APPROVED (F1a–F1f) — FEATURE COMPLETE + full-suite green (1101 tests, deterministic) + LIVE-VERIFIED on alt port :5099 (2026-07-02): artifact with all 4 sections written with --with-debug-ui OFF (own-gate); `<think>` localization confirmed (DeepSeek `delta.reasoning` in upstream_response → Anthropic `thinking` blocks in served_response); mid-stream disconnect → `served_partial:true` + no hang; no `.work`/`.tmp` residue; prod :5022 untouched. Pending: merge decision only.** Opt-in `turn_capture_dir` writes ONE atomic JSON per turn
(`<dir>/<api_call_id>.json`) with the FULL inbound Anthropic request, translated OpenAI request, RAW vLLM
response, and served Anthropic bytes + outcome — so an operator / fresh Claude session can debug weird CC
output (stray `<think>` tags, malformed tool calls) that is otherwise a 200 OK with no durable trace.
Works WITHOUT `--with-debug-ui`. Bounded memory (sections stream to temp files; single JSON assembled by a
streaming escaper). Age-rotated via the existing `debug_log_max_age_hours`.

Design was reviewed by Codex xhigh BEFORE implementation; all HIGH/MED findings are folded into the spec:
own instrumentation gate (not the debug-ui gate); temp-file sections (not an in-memory map — OOM);
`BackendChatRequest` carrier (not `ServingToken`); hybrid finalize (engine owns status/reason, an HTTP
response-`Body` wrapper owns served bytes); RAII drop-guard finalize; UTF-8/base64 encoding contract.

## Tasks

| Task | Description | Status |
|-|-|-|
| F1a | `turn_capture.rs` module (`TurnCapture`/`TurnCaptureState`, disabled zero-op sink) + `turn_capture_dir` on `Config`+`PersistedConfig`+default+env+`configure()`+`debug_log_dirs()` + `Gateway`/`lib.rs` DI. AC-1,2,3 | ✅ |
| F1b | HTTP own-gate + inbound capture (copy+redact) + served-body `Body` tee (stream/non-stream/error/disconnect); per-turn temp-file sections + registry. AC-4,5,6 | ✅ |
| F1c | Engine terminal `engine_done` (RAII `CaptureGuard` + middleware backstop) on all terminals incl pre-spawn; both-`done` barrier → bounded streaming assembly + unconditional registry evict + best-effort `.work` delete (failed delete → F1f orphan sweep, never blocks publish/evict) + atomic rename. AC-7,8,9 | ✅ (Codex-xhigh APPROVED r3; r1 fixed 2 HIGH race+leak, r2 fixed HIGH cleanup-invariant) |
| F1d | `capture: Option<Arc<TurnCaptureState>>` on `BackendChatRequest` (+ `with_capture` builder); upstream_request capture in `dispatch_chat_stream`→`logged_send_chat_request`, `Section` gained `replace`/`SectionChunk::{Append,Replace}` for last-writer-wins (shrink retry / failover final attempt); shared `redact_payload_secrets_in_value` added to `redaction.rs` (`http::redact_payload_secrets` now delegates); replace uses an atomic `Mutex<ReplaceSlot{pending,sealed}>` coalescing slot (guaranteed last-writer-wins under backpressure + accept-vs-close). AC-10,11 | ✅ (Codex-xhigh APPROVED r3; r1 fixed drop-on-full, r2 fixed accept/close race) |
| F1e | Raw upstream_response tap in `stream_success_response` (bounded `ServedSink` reuse, sticky-partial drop guard, no hang) + final failed HTTP body (capture-local staged body, per-attempt clear, streamed-success wins) + UTF-8/base64 encoding (generic assembly); upstream_response is final-attempt-only via a cancel-safe synchronous gen-suffixed section swap; eager section-file create (no post-finalize recreation); discriminator on first-byte+clean-EOS (truthful partial). AC-12,13,14 | ✅ (Codex-xhigh APPROVED; r1 failover-scope, r2 cancel-safe, r3 no-recreation+truthful-partial) |
| F1f | Orphan `.work` sweep (`cleanup_orphan_work_dirs`, mtime-based, wired into `spawn_cleanup`) + rotation confirmed (artifacts + orphan `.work`) + `sole_capture_state` flake fixed (4 F1b tests read the durable `<id>.json` via `wait_for_only_artifact`, not the transient `.work` dir) + docs (FEATURES/AGENTS/README); orphan sweep scoped to `turn_capture_dir` only. AC-15,16,17 | ✅ (Codex-xhigh APPROVED r2; r1 fixed HIGH over-broad sweep + vacuous AC-16) |

**Ordering: serial F1a → F1b → F1c → F1d → F1e → F1f** (shared files force `--agents 1`). Each code task
leaves `cargo test` GREEN within itself. F1a lands the sink+config every later task calls; F1b/F1c stand up
the HTTP+engine finalize spine (an artifact is produced end-to-end with inbound+served after F1c); F1d/F1e
fill the two upstream sections; F1f makes the artifact atomic/rotated and documents it.

## Review corrections baked into the spec (do not re-litigate — from Codex xhigh 2026-07-01)

1. **Own gate, not the debug-ui gate.** The `ApiCallId` extension is inserted only inside
   `flow_store().is_enabled()` (`http.rs:536`); turn capture MUST have its own gate or it is dead off the
   dashboard.
2. **No in-memory full-body map (OOM).** Stream sections to temp files; assemble the single JSON by
   streaming, bounded memory.
3. **`stream_anthropic_response` loop-end is NOT the finalizer** — misses disconnect (`http.rs:1311`),
   non-streaming (`collect_anthropic_response` `http.rs:1475`), pre-spawn (`engine.rs:1196`). Hybrid:
   response-`Body` wrapper for served bytes + engine terminal for status/reason.
4. **RAII drop guards**, not best-effort finalize — mirror `TelemetryGuard`/`MiddlewareGuard`.
5. **Carrier = `BackendChatRequest`** (`upstream.rs:3288`, rebuilds clone Arcs), NOT `ServingToken` (keeps
   file IO off the serving-metrics mutex).
6. **Final-attempt-only** request+response (last-writer-wins); ALSO capture the final failed HTTP body;
   `attempts[]` is a documented follow-up.
7. **Encoding contract**: UTF-8 SSE text; base64 + marker for non-UTF-8.
8. **New logged surface ⇒ redact** (AGENTS.md:137) + **no `Bytes`-slice retention** (AGENTS.md:144). The
   operator-visibility itself is NOT a leak (REVIEW_PROTOCOL 2026-06-22) — do not flag it.

## Acceptance mapping

- F1a → AC-1 (config parse/env/preserve), AC-2 (`debug_log_dirs` includes it), AC-3 (disabled zero-op).
- F1b → AC-4 (works w/o debug-ui), AC-5 (inbound redacted), AC-6 (served: stream/non-stream/disconnect).
- F1c → AC-7 (pre-spawn failed artifact), AC-8 (completed all sections), AC-9 (disconnect cancelled+partial,
  no hang).
- F1d → AC-10 (final on-wire request, shrink-retry shows shrunk), AC-11 (failover serving attempt).
- F1e → AC-12 (exact raw SSE bytes incl `<think>`-in-content), AC-13 (final failed body), AC-14
  (malformed→partial, no hang).
- F1f → AC-15 (atomic single JSON, base64 round-trip, no residue), AC-16 (end-to-end redaction), AC-17
  (age rotation of artifacts + orphan `.work`).

## Live verification (orchestrator gate, after F1f green)

Prereq: `:5022` rebuilt with `turn_capture_dir: /home/jon/.local/share/llmconduit/turns` in the config
(dashboard may be ON or OFF — verify BOTH once).
1. A normal `/v1/messages` CC turn → exactly one `<api_call_id>.json` appears; it contains all four sections;
   `served_response` matches what the client received; no `.work` residue.
2. Force a `<think>`-in-content response from the stub/backend → the leak is visible in BOTH
   `upstream_response` (raw vLLM) and `served_response` (Anthropic) — confirming the artifact localizes
   whether the leak is upstream or converter-introduced.
3. Kill a turn mid-stream (client Ctrl-C) → artifact `status:"cancelled"`, `served_response.partial=true`,
   written within ~1s (no hang).
4. Set `debug_log_max_age_hours: 0`/tiny and confirm old artifacts + orphan `.work` dirs are pruned.
