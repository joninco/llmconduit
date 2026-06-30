# Anthropic Stream Conformance Plan (v4 — target: full wire-shape conformance)

## Goal
Make llmconduit's Anthropic `/v1/messages` **streaming** output **fully wire-conformant** —
byte-shape matching vLLM's native `/v1/messages`: exactly one terminal `message_delta`,
a **signed** thinking block, real `message_start.input_tokens`, and correct event
ordering. Upgrade from *malformed-but-tolerated* to *conforming*.

## Non-goal
Does **not** fix `claude -p` (separate Claude Code auth guard; the conformant vLLM native
endpoint also fails it).

## Current state (live probe, for context)
The official `anthropic` Python SDK 0.115.0 (`messages.stream()`) parses llmconduit's
**current, non-conformant** stream without error (empty signature accepted, progressive
`Δ` tolerated) and yields the correct final message. So today's output is
*malformed-but-tolerated*. This plan goes beyond toleration to **strict conformance**
(robustness to stricter/ordering-asserting clients, future Anthropic backends, and
byte-shape parity with vLLM). Caveat on testing: probe used the **Python** SDK; validate
against the **TypeScript** SDK (Claude Code's) in Phase 6.

## Approach (chosen)
**Fix the translation** in `src/adapters/responses_to_anthropic/`. Keep the
chat→responses→anthropic pipeline + features. Not native passthrough.

## The conformance gap (current → target), validated against source
`Δ`=message_delta, `CB`=content_block:
```
vLLM 8001 (target):  message_start │ CB(think)… CBstop │ CB(text)… CBstop │ Δ │ stop   (Δ=1, signed, input_tokens=real)
llmconduit (current): ping message_start │ Δ×8 │ CB(think)… CBstop │ CB(text)… Δ CBstop │ Δ │ stop  (Δ=10, sig=EMPTY, input_tokens=0)
```

| # | Deviation | Target | Source anchor |
|-|-|-|-|
| 1 | 10 `message_delta` (8 before first `CBstart`, 1 mid-block) | **one terminal** `Δ` (after all blocks, carries `stop_reason`+usage) | offending push `mod.rs:691`; all call sites of `record_output_delta`: **mod.rs 200, 214, 236, 299, 357, 777**; terminals kept: `handle_completed` mod.rs:483, `finalize` mod.rs:422 |
| 2 | thinking block **unsigned** (empty signature) | **signed** (`signature_delta`, non-empty) | `flush_reasoning_as_thinking` mod.rs:588/612. Real upstream sig is forwarded when present (`chat_to_responses.rs:162`, `chat.rs:279`); DeepSeek `reasoning_content` has none → must **synthesize** |
| 3 | `message_start.usage.input_tokens = 0` | **real prompt token count** | `ensure_started` mod.rs:141/155; `message_start` fires from `response.created` (id-only: `responses.rs:481`, `engine.rs:1914`); estimator is a budgeting heuristic, not a tokenizer (`engine.rs:426`) |
| 4 | `ping` emitted **before** `message_start` | match Anthropic's actual ordering | `ensure_started` mod.rs:141 |

**Honest ceiling:** full *wire-shape* conformance is achievable. A *cryptographically
authentic* Anthropic signature is not (llmconduit isn't Anthropic) — the synthetic
signature satisfies shape (field present, non-empty, `signature_delta` emitted), not
real verification. That's the correct ceiling for a proxy.

## Phase 0a — Decisions (resolve before coding)
1. **Signature (REQUIRED for conformance).** Default: emit a **recognizable synthetic
   `signature_delta`** in `flush_reasoning_as_thinking` when no real upstream signature
   exists, AND **strip it on Anthropic ingress** so a client echo-back is never
   re-forwarded as a real `thinking.signature`. Echo-back path to neutralize:
   `anthropic_to_responses.rs:366` (→ `encrypted_content`) → `responses_to_chat.rs:91`
   (`store=false` at `anthropic_to_responses.rs:53`, so the risk is echo-back, not replay
   storage). Keep real-signature forwarding + `emit_thinking=false` (G8) intact.
2. **input_tokens (REQUIRED; hardest item).** Pick one:
   (a) thread upstream `prompt_tokens` into `message_start` (vLLM reports it — but usually
   arrives late in the chat stream); (b) **buffer `message_start`** until the prompt-token
   count is known; (c) real tokenizer count up front (adds dep/cost). If none is clean,
   this is the **single acceptable residual deviation** — document it (clients tolerate
   0; even Anthropic's exact value isn't correctness-critical).
3. **ping placement.** Anthropic does emit `ping`; decide position relative to
   `message_start` to match the target shape.
4. **finalize()/error terminal shape.** `handle_failed` emits only `error` (mod.rs:497);
   HTTP streaming then calls `finalize()` (`http.rs:1305`) → `error → Δ → message_stop`.
   Check Anthropic's real error-stream shape and decide whether to keep the trailing
   `Δ`+`message_stop` or end at `error`.

## Phase 0b — Strict conformance harness (locks DoD)
Per-surface assertions — text, reasoning+text, **client tool_use**, web_search/server-
tool, finalize/error. (Note: web_search emits `server_tool_use`/`web_search_tool_result`
directly at mod.rs:534 and does **not** call `record_output_delta`; client tool_use
does.) Invariants: **exactly one** terminal `message_delta` (carries `stop_reason`); **no
`Δ` before the first `content_block_start`**; **no `Δ` between a `content_block_delta`
and its `content_block_stop`**; thinking block has a **non-empty signature**;
`message_start.input_tokens` reflects real prompt size (per 0a-2); stream ends
`message_delta → message_stop`.

## Phase 1 — One terminal `message_delta`
Make `record_output_delta` bookkeeping-only: keep `estimated_output_bytes` /
`last_output_tokens`; **remove the `output.push(MessageDelta)`** (mod.rs:691-701); drop
the now-unused `output` param; update **all** call sites (200/214/236/299/357/777) and
comments. Verify the terminal `Δ` still carries final `output_tokens` (collector relies
on `last_output_tokens` when upstream usage is absent — `collector.rs:68/150`).

## Phase 2 — Sign thinking (per 0a-1) + ingress strip
Synthesize the `signature_delta` in `flush_reasoning_as_thinking`; add ingress stripping
at `anthropic_to_responses.rs:366`. Preserve real-signature forwarding.

## Phase 3 — Real `message_start.input_tokens` (per 0a-2)
Implement the chosen approach; or document the residual deviation if none is clean.

## Phase 4 — ping + error-terminal shape (per 0a-3/4)

## Phase 5 — Tests & docs
Update progressive-usage expectations everywhere — `tests.rs` sequence cases
(~181-185, 217-225, 388-394, 432-440), **`:538` (`emits_progress_usage_for_reasoning_deltas`)**,
**`:572` (`completed_without_upstream_usage_preserves_progress_usage`)**;
**`tests/gateway.rs:8567` & `:8654`**; **`tests/port_streaming_peek.rs:414`**. Add a
collector/converter test proving terminal `output_tokens` stays non-zero when upstream
usage is absent. Fold in the Phase 0b conformance assertions.

## Phase 6 — Verify
`cargo test` (adapter + `tests/gateway.rs` + `tests/port_streaming_peek.rs`). Live:
capture the 5022 stream for each surface; assert byte-shape parity with the 8001 native
golden. Re-run the SDK strictness probe — now also against the **TypeScript** SDK
(`@anthropic-ai/sdk`, Claude Code's client) — and confirm clean parse + correct final
message.

## Definition of Done
Strict conformance harness green for all surfaces; live 5022 stream byte-shape matches
8001 native (1 terminal `Δ`, signed thinking, correct ordering, real-or-documented
`input_tokens`); Python **and** TypeScript SDKs parse cleanly.

## Risks
- **`input_tokens` is the hardest** (timing): `message_start` fires before usage is known;
  may need buffering. Acceptable residual if no clean path.
- **Synthetic signature is shape-only** (won't pass real Anthropic verification); ingress
  stripping prevents echo-back leakage.
- **Test surface is large** — `tests.rs`, `tests/gateway.rs`, `tests/port_streaming_peek.rs`.
- Dashboard is **not** affected (reads upstream `ChunkUsage`, `engine.rs:2139/2170`, not
  the wire) — confirmed.

## Estimate
~2 sessions: 0a decisions + 0b harness + the input_tokens approach are the real work;
P1 is a small code change; the rest is the (large) test surface.

## Ralph
Sequence 0a → 0b → 1 → 2 → 3 → 4 → 5 → 6. 0a is a human/decision gate; the rest map to
`IMPLEMENTATION_PLAN.md` / `/ralph-orchestrate`.

---
*v4 targets full wire-shape conformance (re-elevates signature + input_tokens from v3's
"optional"). Built on a Codex gpt-5.5 (xhigh) source review (call sites, test surface,
round-trip hazard, web_search precision) and a live SDK strictness probe (current output
is tolerated by Python SDK 0.115.0 but non-conformant).*
