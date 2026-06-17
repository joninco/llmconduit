# G8 — Reasoning promotion / suppression heuristics

**Priority:** MED · **Surface:** response-translation (Responses → Anthropic) · **GAPS.md:** G8

## Purpose
Implement the nuanced reasoning-stream handling claude-relay had in its Chat→Anthropic converter,
ported to llmconduit's canonical-Responses → Anthropic converter:
- reasoning-only output is **promoted to a text block** at `finish_reason:stop`,
- but **kept as a thinking block** at `finish_reason:length` (genuine truncated CoT),
- thinking is **suppressed** when an `emit_thinking`-style toggle is off (still promote at stop),
- **late** reasoning arriving after text has started is dropped,
- thinking carrying a **signature** is never promoted (genuine CoT).

## Reference (study, adapt — do NOT transliterate)
- claude-relay behavior source: `/home/jon/git/claude-relay/claude_relay/convert_stream.py`.
- claude-relay tests: `tests/test_convert_stream.py::test_reasoning_*`,
  `::test_*_promoted*`, `::test_signature_*`, `::test_late_reasoning_*` (~8 behaviors).
- llmconduit target: `src/adapters/responses_to_anthropic.rs`
  (`AnthropicStreamConverter` / `AnthropicStreamCollector`, `finalize()`).

## Acceptance criteria (executable)
Add `tests/port_response_translation.rs` using the shared SSE collectors in `tests/common/mod.rs`
(`collect_stream`, `parse_anthropic_sse_events`, `reasoning_chunk`, `nested_thinking_chunk`, `finish_chunk`):
- [ ] reasoning-only + `finish_reason:stop` → emitted as a **text** block.
- [ ] reasoning-only + `finish_reason:length` → emitted as a **thinking** block (not promoted).
- [ ] thinking with a signature → stays thinking (never promoted), even at stop.
- [ ] late reasoning after text started → dropped (not flushed).
- [ ] (if an emit-thinking toggle is added) thinking suppressed when off, but still promoted at stop.

## Constraints (load-bearing — see AGENTS.md)
- `finalize()` MUST still emit synthetic `message_delta` + `message_stop` if the canonical stream ends
  without `response.completed` — clients must never hang.
- Progressive output-usage estimation must be preserved.
- Existing `gateway.rs` Anthropic streaming tests MUST stay green (no regressions).
- `response.web_search_results` additive-event handling must be untouched.

## Dependencies
None hard. Confined to `responses_to_anthropic.rs` (does not touch `engine.rs`), so it will not
file-conflict with G1/G2/G3.

## Definition of Done
- [ ] New tests green · existing Anthropic tests still green.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] Codex (xhigh) review passed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-sequence-verifiable-units`, `principle-prove-it-works`, `principle-fix-root-causes`.
