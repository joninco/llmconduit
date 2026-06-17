# G1 — Context-window-limit retry

**Priority:** HIGH · **Surface:** error-mapping · **GAPS.md:** G1

## Purpose
When an upstream returns a context/token-limit error (a 4xx/5xx whose body says the request
exceeds the model's context or completion budget), classify it and **retry once** with a reduced
`max_completion_tokens` (honoring a configurable minimum floor) instead of surfacing the failure.
This is real resilience for local vLLM/llama.cpp backends, which routinely 400 on overflow.

## Reference (study, adapt — do NOT transliterate)
- claude-relay proven impl: `/home/jon/git/claude-relay/claude_relay/backend.py`
  (the `_non_200_retry` / context-limit parsing logic).
- claude-relay tests (behavior source): `tests/test_backend.py::test_non_200_retry_*` (9 behaviors).
- llmconduit targets: `src/error.rs` (maybe a `RetryHint`/`ContextLimit` type), `src/upstream.rs`
  (non-2xx response handling), `src/engine.rs` (`run_turn` request-build + retry orchestration).

## Acceptance criteria (executable)
Create `tests/port_errors.rs` porting the 9 error-classification behaviors. Parse these patterns
into a structured retry decision; assert the recomputed `max_completion_tokens`:
- [ ] "cannot be greater than max_model_len" → completion-limit retry with adjusted limit.
- [ ] completion-limit retry uses `input_tokens` from the error to reduce the quota.
- [ ] vLLM "maximum context length … (N) … (M)" → context-limit retry.
- [ ] vLLM "at least" variant → set an `input_tokens_is_lower_bound` flag.
- [ ] "at least" boundary uses a LARGER safety margin.
- [ ] OpenAI-style "X in the messages, Y in the completion" → context-limit retry.
- [ ] "requested token count exceeds" with input/output breakdown → retry.
- [ ] recomputed `max_completion_tokens` is capped to the configured **min floor**.
- [ ] unrelated error text → no retry (returns the original error).
Then wire the classifier into the upstream non-2xx path so a real overflow triggers exactly one
retry (integration test with wiremock returning a context-limit 400, then 200).

## Constraints (load-bearing — see AGENTS.md "Hard rules in the engine")
- **Retry MUST happen pre-first-chunk only.** Failover is pre-first-chunk (`upstream.rs:407-419`); a
  mid-stream failure is surfaced, never retried — never duplicate already-streamed tokens.
- `AppError` client/internal split: the eventual client-facing error must not leak internal detail.
- Must not interfere with `FailoverUpstreamClient`/`RoutingUpstreamClient` semantics (routing providers
  are NOT failure fallbacks).
- Replay (`store=true`) must still behave; do not poison the cache with a failed attempt.
- Keep `parallel_tool_calls=false` and the web-search hard ceiling untouched.

## Dependencies
Shares "context limit / token budget" knowledge with **G3** (proactive cap). Implement G1 first
(reactive safety net), then G3 can reuse the classifier's notion of context length.

## Definition of Done
- [ ] `tests/port_errors.rs` green + one wiremock integration test proving a single retry.
- [ ] `cargo test` whole suite green · `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] Codex (xhigh) review passed, findings addressed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-fix-root-causes`, `principle-boundary-discipline`, `principle-prove-it-works`, `principle-foundational-thinking`.
