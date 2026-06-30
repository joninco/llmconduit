# IMPLEMENTATION_PLAN.md — Anthropic SSE stream conformance

> **Spec:** `.ralph/specs/anthropic-sse-conformance.md` + authoritative `ANTHROPIC_STREAM_CONFORMANCE_PLAN.md` (root).
> **Golden:** `.ralph/golden_8001_native_messages.sse`. **Conventions:** `AGENTS.md`. **G8:** `GAPS.md`.
> **Branch:** `anthropic-sse-conformance`. **Run:** `/ralph-orchestrate --agents 2` (auto-review ON), Sonnet-5 subagents.

## Executive Summary
**Status: 0/7 tasks completed.** Make `/v1/messages` streaming byte-shape-conformant with vLLM native:
one terminal `message_delta`, signed thinking, real `message_start.input_tokens`, correct ordering.

**Ordering is strict and (mostly) serial** — tasks 0B1→C1→C2→C3→C4 all edit
`src/adapters/responses_to_anthropic/mod.rs` and the shared test surface, so they cannot parallelize.
Run one phase per iteration in order. **Every task must leave `cargo test` GREEN** (update the tests that
the behavior change breaks within the SAME task — do not defer all test churn to T5).

Hard constraints (verbatim from spec — re-read before editing):
- Keep replay / system-prefix / web_search injection / dashboard working. NOT native passthrough.
- KEEP `estimated_output_bytes` + `last_output_tokens` bookkeeping (collector relies on it: `collector.rs:68/150/154`).
- `web_search` does NOT call `record_output_delta` (`mod.rs:534`) — cover CLIENT `tool_use` in no-progressive-delta tests.
- Do NOT touch the dashboard usage path (`engine.rs:2139/2170`).

Source anchors verified against HEAD (`mod.rs` line numbers, current):
`ensure_started`:141 (ping 144, message_start 145, input_tokens 155) · `record_output_delta` def 679, offending
push 691-701, call sites **200, 214, 236, 299, 357, 777** · `flush_reasoning_as_thinking`:588 (signature emit 612-617)
· `handle_completed`:442 (terminal Δ 483) · `finalize`:410 (terminal Δ 422) · `handle_failed`:497 · web_search:534.
Ingress strip: `anthropic_to_responses.rs:366` (Thinking → `encrypted_content`, currently filters empty at 377-380).

---

## Task 0B1 — Strict conformance harness (FIRST; defines "done")
**Why first:** the harness encodes the target invariants so every later phase has an objective pass/fail gate.
**Files:** new conformance assertion helpers reusable from BOTH unit tests (`AnthropicStreamEvent` form) AND the two
integration test crates (`tests/gateway.rs` JSON-SSE form, `tests/port_streaming_peek.rs` event form).

**Do:**
1. Add a reusable assertion library exposing two entry points:
   - `assert_stream_conformant(events: &[AnthropicStreamEvent], surface: Surface)` — operates on the converter's
     public output enum (used by `tests.rs` unit tests + `port_streaming_peek.rs`).
   - `assert_sse_conformant(events: &[serde_json::Value], surface: Surface)` — operates on parsed JSON SSE
     (used by `tests/gateway.rs`).
   Recommended placement: `tests/common/mod.rs` for the integration-crate-visible JSON form; for the event form,
   either a small `pub`/`#[cfg(any(test, feature=...))]` module in `src/adapters/responses_to_anthropic/` or a
   mirrored helper in `tests/common`. Subagent picks the cleanest structure that all three test files can import.
2. The assertions (ALL invariants from the spec):
   - exactly ONE `message_delta`, and it carries a non-null `stop_reason`;
   - NO `message_delta` appears before the first `content_block_start`;
   - NO `message_delta` appears between a `content_block_delta` and the matching `content_block_stop` (open block);
   - if a `thinking` block exists, it emits a NON-EMPTY `signature_delta`;
   - the LAST two events are `message_delta` then `message_stop`;
   - `Surface` lets a caller relax block-shape expectations (text-only has no thinking block; error surface ends
     with `error` — assert that variant explicitly).
3. **Self-validation test (this is what keeps T0B1 green):** feed the harness a hand-built CONFORMANT event vector
   (mirrors the golden: message_start → thinking(+sig) → text → ONE message_delta → message_stop) and assert it
   PASSES; feed a NON-CONFORMANT vector (progressive deltas + empty sig, i.e. today's shape) and assert it is
   REJECTED (use `#[should_panic]` or a `Result`-returning variant). This proves the harness discriminates without
   yet touching the real converter (that is T-C1..C4).

**DoD:** `cargo test` green; harness self-tests prove accept-good / reject-bad. No production code changed yet.

---

## Task C1 — One terminal `message_delta` (deviation #1)
**Files:** `src/adapters/responses_to_anthropic/mod.rs` (+ test surface).

**Do:**
1. `record_output_delta` (`mod.rs:679`): make it **bookkeeping-only** — keep `estimated_output_bytes` accumulation
   and the `last_output_tokens` update; **remove the `output.push(MessageDelta{…})`** (lines 691-701). Drop the now-unused
   `output: &mut Vec<AnthropicStreamEvent>` parameter and update its doc comment.
2. Update ALL call sites to drop the `output` arg: `mod.rs` **200, 214, 236, 299, 357, 777**.
3. Confirm the terminal `message_delta` still carries final `output_tokens`: `handle_completed:483` uses
   `output_tokens = upstream_usage.output_tokens.unwrap_or(last_output_tokens).max(last_output_tokens)`;
   `finalize:422` uses `last_output_tokens`. The bookkeeping you kept is what feeds these. The collector
   (`collector.rs:150-156`) reads the terminal Δ's `output_tokens`.
4. **Tests broken by removing progressive deltas — fix in THIS task:**
   - `tests.rs` sequence cases ~**181-185, 217-225, 388-394, 432-440**: remove the intermediate `"message_delta"`
     rows; keep exactly one terminal `"message_delta"` before `"message_stop"`.
   - `tests.rs:539` `emits_progress_usage_for_reasoning_deltas`: REPURPOSE (rename e.g.
     `terminal_delta_carries_reasoning_output_tokens`) — assert there are NO progressive `message_delta`s during
     reasoning AND the single terminal Δ's `output_tokens` is non-zero (reflects the buffered reasoning byte-count
     via the kept bookkeeping).
   - `tests.rs:572` `completed_without_upstream_usage_preserves_progress_usage`: keep the core assertion (terminal
     `output_tokens` non-zero from bookkeeping when upstream usage absent); drop the "progress" framing.
   - `tests/gateway.rs:8567` and `:8654`: flip from `!progress_tokens.is_empty()` to "exactly one terminal
     `message_delta` carrying `output_tokens`; zero progressive deltas".
   - `tests/port_streaming_peek.rs:414`: update the comment + the "progressive usage `message_delta`s are allowed"
     allowance — pre-terminal, only `message_start`/`ping` may appear, never a `message_delta`.
5. Add a harness-backed conformance test (use T0B1) on a real reasoning+text converter run: exactly one terminal Δ.

**DoD:** `cargo test` green; `assert_stream_conformant` passes invariants 1-3 + 6 on a real converter run;
no `record_output_delta` call site still passes `output`.

---

## Task C2 — Sign the thinking block + ingress strip (deviation #2)
**Files:** `mod.rs` (`flush_reasoning_as_thinking`), `anthropic_to_responses.rs` (ingress), a shared const.

**Do:**
1. Define `const SYNTHETIC_SIGNATURE_PREFIX: &str` — a clearly-synthetic recognizable marker (e.g.
   `"llmconduit-synthetic-v1:"`). Place it so BOTH the converter and the Anthropic ingress adapter can reference it
   (e.g. a `pub(crate)` const in `models/anthropic.rs` or a shared adapters module).
2. `flush_reasoning_as_thinking` (`mod.rs:588`): after emitting the buffered thinking deltas, when
   `take_signature()` is `None` (no real upstream signature — the DeepSeek `reasoning_content` case), synthesize a
   non-empty `signature_delta` = `SYNTHETIC_SIGNATURE_PREFIX` + a deterministic suffix (e.g. the message id or a
   hash of the buffered text — must NOT use wall-clock/RNG). Emit it as the LAST delta in the block (matches the
   golden ordering). When a REAL signature IS present, forward it unchanged (preserve current behavior).
   - This path also covers the terminal "keep as thinking" case (`flush_reasoning_terminal` → `flush_reasoning_as_thinking`
     when `!promote`). The PROMOTE-to-text path (`mod.rs:646-663`) emits a `text` block, no signature — leave it.
3. Anthropic ingress (`anthropic_to_responses.rs:366-382`, the `Thinking { thinking, signature }` arm): when
   `signature` starts with `SYNTHETIC_SIGNATURE_PREFIX`, map `encrypted_content` to `None` (strip) so a client
   echo-back of our synthetic marker is never re-forwarded upstream as a real `thinking.signature`. Keep the existing
   empty-filter; keep forwarding genuine (non-synthetic, non-empty) signatures.
4. **Tests:**
   - Unchanged (real-sig forwarding still works): `tests.rs:232` `converts_reasoning_signature_delta` (sig "sig_123"),
     `:323` `accumulates_multi_part_signature_deltas`, `:639` `collector_preserves_thinking_signature`,
     `gateway.rs:8654` (asserts `signature == "sig_123"` when upstream provides one).
   - NEW: reasoning-only / reasoning+text turn with NO upstream signature → emitted thinking block has a NON-EMPTY
     `signature_delta` whose value starts with `SYNTHETIC_SIGNATURE_PREFIX`.
   - NEW (round-trip, AGENTS.md "no new wire field without round-trip test"): an Anthropic request whose assistant
     `thinking` block carries a synthetic-prefixed signature → canonical `Reasoning.encrypted_content` is `None`
     (stripped); a real signature → forwarded as `encrypted_content`.

**DoD:** `cargo test` green; every emitted thinking block is signed; synthetic stripped on ingress; real forwarded;
`assert_stream_conformant` invariant #4 passes.

---

## Task C3 — Real `message_start.input_tokens` (deviation #3; the hard one — do NOT block)
**Files:** likely `engine.rs` (carry the early estimate onto the created/started signal) + `mod.rs`
(`ensure_started`) + converter construction; or document residual.

**Timing facts (verified):** `response.created` (`engine.rs:4090`) fires BEFORE the upstream responds, so the REAL
`prompt_tokens` (arrives late via `chunk.usage`, `engine.rs:2139`) is NOT available at `message_start`. An EARLY
ESTIMATE exists: `estimate_input_tokens` (`engine.rs:445`, computed at `engine.rs:1289`). vLLM native carries the
real count at `message_start` (golden: `input_tokens: 20`) because it has the tokenizer; llmconduit does not.

**Decision tree (per 0a-2 — pick the first that is CLEAN):**
1. **Probe** the live 8001 chat stream: does `prompt_tokens` arrive in an EARLY chunk? If yes, thread the real value
   into `message_start`.
2. **(Recommended middle path)** Thread the early ESTIMATE into `message_start`: carry `estimate_input_tokens` onto
   the `response.created` event payload (or pass it to `AnthropicStreamConverter::new`) so `ensure_started`
   (`mod.rs:155`) emits a non-zero, plausible `input_tokens` instead of `0`. Tag it as an estimate (DQ). This is
   non-architectural (no stream buffering) and closes the visible `0` deviation.
3. **Residual:** if neither is clean without an architectural change, LEAVE `0` and DOCUMENT it (in the spec + this
   plan) as the single accepted residual deviation. Do NOT block the rest of the work.
   - Do NOT buffer `message_start` until the late usage arrives — that defers stream start (bad UX, architectural).

**Tests:** update `tests.rs:524` `assert_eq!(message_start.usage.input_tokens, Some(0))` to match the chosen
behavior (`Some(<estimate>)` or keep `Some(0)` with a comment citing the documented residual). Confirm NO regression
in the FINAL non-stream `input_tokens` (`tests.rs:634` expects the real `12` from completed usage — that overrides at
`handle_completed:468` → `collector.rs:70`, so it must stay `12`). Confirm `gateway.rs` completed-usage input_tokens
asserts (e.g. `:858`, `:3934`) unaffected.

**DoD:** `cargo test` green; `message_start.input_tokens` is real/estimated/documented-residual; final usage unchanged.

---

## Task C4 — ping + error-terminal shape (deviations #4; cosmetic — never block)
**Files:** `mod.rs` (`ensure_started`, `handle_failed`), tests.

**Do:**
1. **ping:** golden vLLM native emits **NO `ping`**. `ensure_started` (`mod.rs:144`) currently pushes `Ping` then
   `MessageStart`. To byte-match, the cleanest is to DROP the `Ping` emission (or, if a ping is desired for client
   keep-alive, move it AFTER `message_start` to at least not precede it). Pick the option that matches the golden; if
   dropping has wider implications (e.g. SSE keep-alive elsewhere), keep + document. Update `tests.rs:747`
   (`vec!["ping","message_start","message_delta","message_stop"]`) to the chosen order/shape.
2. **error terminal:** `handle_failed` (`mod.rs:497`) emits only `error`; HTTP streaming then calls `finalize()`
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
