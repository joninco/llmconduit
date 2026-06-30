# Spec — Anthropic `/v1/messages` streaming wire-conformance

**Authoritative source:** `ANTHROPIC_STREAM_CONFORMANCE_PLAN.md` (v4, repo root) — read IN FULL.
**Golden reference:** `.ralph/golden_8001_native_messages.sse` (captured from vLLM native 8001).
**Conventions:** `AGENTS.md` (canonical-Responses-only, comments-explain-why, no-new-wire-fields-without-round-trip-test). **G8 contract:** `GAPS.md` §G8 (reasoning promotion/suppression — preserve).

## Goal
Make llmconduit's Anthropic `/v1/messages` **streaming** output **byte-shape conformant** with
vLLM's native `/v1/messages`. Upgrade from *malformed-but-tolerated* to *strictly conforming*.

## Target byte-shape (decoded golden, reasoning+text turn)
```
message_start      usage = { input_tokens: <REAL>, output_tokens: 0 }
content_block_start  idx0  { type: thinking, thinking: "" }
content_block_delta  idx0  thinking_delta × N
content_block_delta  idx0  signature_delta "<non-empty>"     ← last delta in the block
content_block_stop   idx0
content_block_start  idx1  { type: text, text: "" }
content_block_delta  idx1  text_delta × M
content_block_stop   idx1
message_delta        { stop_reason } usage = { input_tokens, output_tokens }   ← exactly ONE, terminal
message_stop
```
Note: vLLM native emits **NO `ping`**.

## The 4 deviations to eliminate (current → target)
| # | Current | Target |
|-|-|-|
| 1 | 10 `message_delta` (8 before first `content_block_start`, 1 mid-block) | exactly **one terminal** `message_delta` carrying `stop_reason` + final usage |
| 2 | thinking block unsigned (empty signature) | **signed** — non-empty `signature_delta` (synthetic marker when no real upstream sig) |
| 3 | `message_start.usage.input_tokens = 0` | **real/estimated** prompt-token count (or documented residual) |
| 4 | `ping` before `message_start` | match vLLM native (no ping) — cosmetic, never block |

## Invariants (the conformance harness asserts ALL of these on every surface)
1. **Exactly one** terminal `message_delta`, and it carries `stop_reason`.
2. **No** `message_delta` before the first `content_block_start`.
3. **No** `message_delta` between a `content_block_delta` and its `content_block_stop` (never inside an open block).
4. A thinking block carries a **non-empty** signature (`signature_delta`).
5. `message_start.input_tokens` reflects real prompt size (per decision 0a-2; estimate acceptable, document if residual).
6. Stream ends `message_delta → message_stop`.

Surfaces to assert: text-only, reasoning+text, **client `tool_use`**, web_search/server-tool, finalize/error.

## Hard constraints (load-bearing — do NOT violate)
- Keep ALL chat-pipeline features: replay, system-prefix, web_search injection, dashboard. **Not** native passthrough.
- Phase 1 updates EVERY `record_output_delta` call site (`mod.rs` 200, 214, 236, 299, 357, 777) and KEEPS the
  `estimated_output_bytes` / `last_output_tokens` bookkeeping — the non-stream collector relies on
  `last_output_tokens` when upstream usage is absent (`collector.rs:68/150/154`).
- `web_search` does **NOT** call `record_output_delta` (emits blocks directly at `mod.rs:534`). Cover **CLIENT**
  `tool_use` — not web_search — in the no-progressive-delta tests.
- Do **NOT** touch the dashboard usage path (reads upstream `ChunkUsage`, `engine.rs:2139/2170`, not the wire).
- Synthetic signature is **shape-only** (a proxy can't mint a real Anthropic signature). Strip it on Anthropic
  ingress so a client echo-back is never re-forwarded as a real `thinking.signature`.

## Definition of Done
Conformance harness green on all surfaces; live 5022 stream byte-shape matches 8001 native; Python
(and ideally TypeScript) Anthropic SDK parse cleanly + return the correct final message.
