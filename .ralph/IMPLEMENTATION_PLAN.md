# IMPLEMENTATION_PLAN.md — Anthropic SSE stream conformance

> **Spec:** `.ralph/specs/anthropic-sse-conformance.md` + authoritative `ANTHROPIC_STREAM_CONFORMANCE_PLAN.md` (root).
> **Golden:** `.ralph/golden_8001_native_messages.sse`. **Conventions:** `AGENTS.md`. **G8:** `GAPS.md`.
> **Branch:** `anthropic-sse-conformance`. **Run:** `/ralph-orchestrate --agents 2` (auto-review ON), Sonnet-5 subagents.

## Executive Summary
**Status: 4/7 tasks completed.** Make `/v1/messages` streaming byte-shape-conformant with vLLM native:
one terminal `message_delta`, signed thinking, real `message_start.input_tokens`, correct ordering.

## Completed

| Task | Description | Status |
|-|-|-|
| 0B1 | Strict conformance harness | ✅ |
| C1 | One terminal message_delta | ✅ |
| C2 | Sign thinking + ingress strip | ✅ |
| C3 | Real message_start.input_tokens | ✅ (estimated — see below) |

**Ordering is strict and (mostly) serial** — tasks 0B1→C1→C2→C3→C4 all edit
`src/adapters/responses_to_anthropic/mod.rs` and the shared test surface, so they cannot parallelize.
Run one phase per iteration in order. **Every task must leave `cargo test` GREEN** (update the tests that
the behavior change breaks within the SAME task — do not defer all test churn to T5).

Hard constraints (verbatim from spec — re-read before editing):
- Keep replay / system-prefix / web_search injection / dashboard working. NOT native passthrough.
- KEEP `estimated_output_bytes` + `last_output_tokens` bookkeeping (collector relies on it: `collector.rs:68/150/154`).
- `web_search` does NOT call `record_output_delta` (`mod.rs:540`) — cover CLIENT `tool_use` in no-progressive-delta tests.
- Do NOT touch the dashboard usage path (`engine.rs:2139/2170`).

Source anchors verified against HEAD (`mod.rs` line numbers, current):
`ensure_started`:146 (ping 149, message_start 150, input_tokens 160) · `record_output_delta` def 713, call sites
**205, 219, 242, 305, 363, 797** · `flush_reasoning_as_thinking`:594 (unconditional signature emit, real-or-synthetic,
620-635; `synthetic_signature` helper 814) · `handle_completed`:448 (terminal Δ 489) · `finalize`:416 (terminal Δ
428) · `handle_failed`:503 · web_search:540. Ingress strip: `anthropic_to_responses.rs:367` (Thinking →
`encrypted_content`, filters empty AND `SYNTHETIC_SIGNATURE_PREFIX`-prefixed at 386-392).
`SYNTHETIC_SIGNATURE_PREFIX` const: `models/anthropic.rs` "Shared constants" section.

---

## Task C4 — ping + error-terminal shape (deviations #4; cosmetic — never block)
**Files:** `mod.rs` (`ensure_started`, `handle_failed`), tests.

**Do:**
1. **ping:** golden vLLM native emits **NO `ping`**. `ensure_started` (`mod.rs:149`) currently pushes `Ping` then
   `MessageStart`. To byte-match, the cleanest is to DROP the `Ping` emission (or, if a ping is desired for client
   keep-alive, move it AFTER `message_start` to at least not precede it). Pick the option that matches the golden; if
   dropping has wider implications (e.g. SSE keep-alive elsewhere), keep + document. Update `tests.rs:834`
   (`vec!["ping","message_start","message_delta","message_stop"]`) to the chosen order/shape.
2. **error terminal:** `handle_failed` (`mod.rs:503`) emits only `error`; HTTP streaming then calls `finalize()`
   (`http.rs:1305`) → `error → message_delta → message_stop`. Check Anthropic's real error-stream shape; decide
   whether to keep the trailing `Δ + message_stop` or end at `error`. Low priority — keep current behavior +
   document if unclear. Ensure the conformance harness's error surface asserts whichever shape is chosen.

**DoD:** `cargo test` green; ping/error shape matches golden where cheap, else documented; harness error-surface green.

---

## Task T5 — Comprehensive conformance sweep + docs
**Files:** `tests.rs`, `tests/gateway.rs`, `tests/port_streaming_peek.rs`, `tests/common`, docs.

**Do:**
1. Route REAL converter/collector output through `assert_stream_conformant` / `assert_sse_conformant` for EVERY
   surface: text-only, reasoning+text, **client `tool_use`**, web_search/server-tool, finalize/error.
2. Add (if not already in C1) a collector/converter test proving the terminal `output_tokens` stays non-zero when
   upstream usage is ABSENT (the kept bookkeeping path, `collector.rs:150-156`).
3. Ensure CLIENT `tool_use` (NOT web_search) is the subject of the no-progressive-delta assertions.
4. Sweep the full surface for any remaining stale progressive-usage expectation; `cargo test` fully green.
5. Docs: mark `ANTHROPIC_STREAM_CONFORMANCE_PLAN.md` phases done; record any residual (input_tokens) explicitly in
   the spec. Keep `AGENTS.md` operational only (no status text there).

**DoD:** full `cargo test` green; conformance harness applied to all five surfaces.

---

## Task T6 — Verify (live + SDK) — ORCHESTRATOR GATE (not a code-change task)
Run by the orchestrator / a verify subagent after C1-T5 + review are green. Prereq: 5022 (rebuilt) + 8001 running.
1. `cargo test` (adapter + `tests/gateway.rs` + `tests/port_streaming_peek.rs`) green.
2. **Rebuild + restart 5022** on the new binary (the running 5022 is the OLD build).
3. Live byte-shape parity: capture streaming `/v1/messages` SSE from 5022 AND 8001 native
   (`DeepSeek-V4-Flash-DSpark`) for a reasoning+text prompt. Assert 5022 matches the golden: ONE terminal
   `message_delta`, no `message_delta` before the first `content_block_start` or inside an open block, non-empty
   thinking signature, ends `message_delta → message_stop`.
4. Strict-client probe: venv + `pip install anthropic`; `client.messages.stream()` against 5022 (model
   `claude-sonnet-5`) and 8001 — both parse with NO exception + return the correct final message. TS SDK
   (`@anthropic-ai/sdk`) if time permits.

**DoD = overall DoD:** harness green; live 5022 byte-shape matches 8001 native; Python (ideally TS) SDK parse cleanly.
