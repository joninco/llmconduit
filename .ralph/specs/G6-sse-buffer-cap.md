# G6 — SSE per-frame buffer cap (DoS guard)

**Priority:** MED · **Surface:** streaming-sse · **GAPS.md:** G6 (ASSUMPTION — verify first)

## Purpose
Bound the upstream SSE read path so a hostile/buggy upstream cannot stream an oversized or
unterminated frame and exhaust memory. claude-relay capped frame assembly at a configurable
`max_buffer_bytes` (default 1 MB) and rejected frames over the limit.

## VERIFY FIRST (do not assume a gap — see GAPS.md G6 is flagged ASSUMPTION)
Before implementing, a subagent must confirm the real state:
- How does llmconduit read upstream SSE? (`eventsource-stream` in `src/upstream.rs` /
  `src/adapters/chat_to_responses.rs`.) Does `eventsource-stream` already bound buffer growth?
- Is the only existing limit the 256 MiB HTTP **request body** cap (`http.rs:51-52`)? That does NOT
  cover the upstream-**response** read path.
**Record the finding in IMPLEMENTATION_PLAN.md.** If the dependency already caps frames, the scope
shrinks to: expose/document the limit + a test asserting oversized-frame rejection. If not, add the guard.

## Reference (study, adapt — do NOT transliterate)
- claude-relay behavior source: `/home/jon/git/claude-relay/claude_relay/sse.py`;
  tests `tests/test_sse.py` (`buffer_overflow`, `exactly_at_limit`, `just_over_limit`,
  `oversized_unterminated`, `custom_max_buffer`).
- llmconduit target: the upstream stream loop (`src/upstream.rs` / `src/adapters/chat_to_responses.rs`).

## Acceptance criteria (executable)
Add `tests/port_streaming.rs` (drive the upstream-read path with a crafted byte stream):
- [ ] A frame at exactly the cap succeeds.
- [ ] A frame one byte over the cap is rejected with a clean error (not OOM/panic).
- [ ] An oversized **unterminated** frame is rejected before unbounded accumulation.
- [ ] The cap is configurable; normal-sized streaming is unaffected.

## Constraints (load-bearing — see AGENTS.md)
- Do not break normal streaming or the `*.delta` raw-output path.
- **Preserve cancellation**: the stream loop selects on `tx.closed()` so client hang-up cancels upstream work — keep that.
- Rejection must surface as an `AppError`, not a silent truncation of model output.

## Dependencies
Touches the streaming path (independent of `engine.rs` request-build), so it can run parallel to engine-side gaps if scheduled that way — but default schedule is serial.

## Definition of Done
- [ ] VERIFY-FIRST finding recorded in `IMPLEMENTATION_PLAN.md`.
- [ ] Tests green · `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] Codex (xhigh) review passed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-foundational-thinking` (verify the assumption before building), `principle-prove-it-works`.
