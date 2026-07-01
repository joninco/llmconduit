# IMPLEMENTATION_PLAN.md — Anthropic SSE stream conformance

> **Spec:** `.ralph/specs/anthropic-sse-conformance.md` + authoritative `ANTHROPIC_STREAM_CONFORMANCE_PLAN.md` (root).
> **Golden:** `.ralph/golden_8001_native_messages.sse`. **Conventions:** `AGENTS.md`. **G8:** `GAPS.md`.
> **Branch:** `anthropic-sse-conformance`. **Run:** `/ralph-orchestrate --agents 2` (auto-review ON), Sonnet-5 subagents.

## Executive Summary
**Status: 9/9 COMPLETE ✅ — conformance achieved + live-verified.** `/v1/messages` streaming is now byte-shape-conformant
with vLLM native: one terminal `message_delta`, signed thinking, non-zero `message_start.input_tokens` (estimated),
correct ordering, no ping. All 4 deviations eliminated. Round-1 multi-model review clean after CR1 fix. Branch not pushed.

## Completed

| Task | Description | Status |
|-|-|-|
| 0B1 | Strict conformance harness | ✅ |
| C1 | One terminal message_delta | ✅ |
| C2 | Sign thinking + ingress strip | ✅ |
| C3 | Real message_start.input_tokens | ✅ (estimated — see below) |
| C4 | ping + error-terminal shape | ✅ |
| T5 | Conformance sweep + docs | ✅ |
| CR1.1 | Round-1 review: strip C3 estimate at `/v1/responses` raw-forward boundary | ✅ |
| CR1.2 | Round-1 review: document unconditional-estimate-compute tradeoff (accepted, LOW) | ✅ |

**Ordering is strict and (mostly) serial** — tasks 0B1→C1→C2→C3→C4 all edit
`src/adapters/responses_to_anthropic/mod.rs` and the shared test surface, so they cannot parallelize.
Run one phase per iteration in order. **Every task must leave `cargo test` GREEN** (update the tests that
the behavior change breaks within the SAME task — do not defer all test churn to T5).

Hard constraints (verbatim from spec — re-read before editing):
- Keep replay / system-prefix / web_search injection / dashboard working. NOT native passthrough.
- KEEP `estimated_output_bytes` + `last_output_tokens` bookkeeping (collector relies on it: `collector.rs:68/150/154`).
- `web_search` does NOT call `record_output_delta` (`mod.rs:540`) — cover CLIENT `tool_use` in no-progressive-delta tests.
- Do NOT touch the dashboard usage path (`engine.rs:2139/2170`).

Source anchors verified against HEAD (`mod.rs` line numbers, current, post-C4):
`ensure_started`:151 (no ping — dropped in C4; message_start push, input_tokens read inline) ·
`record_output_delta` def 767, call sites 235, 249, 272, 335, 393, 851 · `flush_reasoning_as_thinking`:648
(unconditional signature emit, real-or-synthetic) · `handle_completed`:486 (terminal Δ, `completed=true` 492) ·
`finalize`:454 (`completed` early-return, terminal Δ) · `handle_failed`:556 (C4: now also sets `completed=true`
570, so the stream ends AT `error` — `finalize()` no-ops after a failure) · web_search:`WEB_SEARCH_TOOL_NAME`
const 42 / `is_hidden_server_tool` 58. Ingress strip: `anthropic_to_responses.rs:367` (Thinking →
`encrypted_content`, filters empty AND `SYNTHETIC_SIGNATURE_PREFIX`-prefixed at 386-392).
`SYNTHETIC_SIGNATURE_PREFIX` const: `models/anthropic.rs` "Shared constants" section.

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

### T6 RESULTS — ✅ VERIFIED (2026-06-30)
1. **`cargo test --release` GREEN** — 671 lib + 149 `gateway.rs` + `port_streaming_peek.rs` + every integration
   binary, 0 failed / 0 ignored / 0 panics.
2. **Verification instance**: the running `:5022` is the systemd `llmconduit.service` on the *installed*
   `/usr/local/bin/llmconduit` (OLD binary) — NOT disrupted. The NEW release binary was run on an alt port `:5055`
   with a copied config (same `localhost:8001/v1` upstream) to verify new-code wire shape non-invasively.
3. **Live byte-shape parity** (reasoning+text, `.ralph/golden_8001_native_messages.sse` as target): 5055 new code
   passes ALL 6 invariants and is structurally identical to the 8001 native golden —
   `message_start → thinking(delta×N + signature_delta) → text(delta×M) → ONE terminal message_delta(end_turn) → message_stop`,
   NO ping. Thinking signature = `llmconduit-synthetic-v1:<sha256…>` (non-empty synthetic). `message_start.input_tokens`
   non-zero (estimate; the terminal delta + SDK-reconstructed final usage carry the REAL upstream count).
4. **Python SDK** (`anthropic` 0.115.0, `messages.stream()`): BOTH 5055 (`claude-sonnet-5`→DeepSeek) and 8001 parse
   with NO exception, blocks `[thinking, text]`, `stop_reason=end_turn`, correct final answer ("42").
5. **TS SDK** (`@anthropic-ai/sdk`, `messages.stream()` + `finalMessage()`): BOTH parse cleanly, correct final message.

---

## Code Review Fixes (Round 1) — DONE

> **Source**: Multi-model code review (Codex `gpt-5.5`, Opus) on 2026-06-30. Both findings were in **C3** (the early
> input-token estimate stamped onto canonical `response.created`), sharing one root cause: the estimate is
> computed/stamped unconditionally, decoupled from the Anthropic streaming egress that is its only consumer.
> C1/C2/C4 reviewed clean — no defect found.

- **CR1.1** (MEDIUM, leak): `stream_responses_response` (`http.rs:1377`) raw-forwards `event.data`, so the
  estimate leaked onto the `/v1/responses` streaming wire. Fixed with a boundary strip — new
  `responses_wire_event_data` helper removes `estimated_input_tokens` from `response.created` only, before
  serializing; zero-cost for every other event. Chose this over out-of-band-threading the estimate into
  `AnthropicStreamConverter` (would have rippled `Gateway::stream_responses*`'s signature through `http.rs` +
  all of `tests/gateway.rs`) — disproportionate for a MEDIUM, single-file fix.
- **CR1.2** (LOW, redundant compute): accepted + documented rather than churned. Full laziness needs the same
  egress-awareness CR1.1's rejected alternative would have threaded down; without it `run_turn` cannot know
  whether the caller is Anthropic (needs the estimate) or Chat/Responses (doesn't) at compute time. Rewrote the
  `engine.rs` comment at the compute site to state the real (non-trivial) cost accurately and why it's bounded/
  accepted, per the task's own "document the decision, don't over-engineer a LOW finding" fallback.

Full detail (topology verification, decision rationale, tests added, regression-proof of the new test): `.ralph/COMPLETED_TASKS.md` → "Task CR1.1 + CR1.2".

---
