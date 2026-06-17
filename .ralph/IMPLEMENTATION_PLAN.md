# IMPLEMENTATION_PLAN.md — llmconduit gap implementation

Implements 7 of the 9 gaps in `GAPS.md` (G4 image-agent + G7 route-config deferred — LOW priority and
architecturally divergent). Specs: `.ralph/specs/*`. Review gate: `.ralph/REVIEW_PROTOCOL.md`.

## Executive summary
**Status: 0/7 complete, 7 pending.**
Execution: **serial (`--agents 1`)** — G1/G2/G3 all edit `src/engine.rs`, so parallel runs would
conflict; serial also keeps the per-gap Codex-xhigh review gate clean. Run with
`/ralph-orchestrate --agents 1 --no-review` (built-in multi-model review disabled; this plan's
per-gap Codex review replaces it).

## Working agreement (every task)
1. Study the gap's `.ralph/specs/<ID>.md` AND the referenced claude-relay impl in
   `/home/jon/git/claude-relay/claude_relay/` — adapt to Rust + canonical-Responses, do NOT transliterate.
2. Confirm with code search before assuming something is missing (some gaps are PARTIAL).
3. Obey AGENTS.md "Hard rules in the engine" — they are load-bearing.
4. Definition of Done per task = executable test green · `cargo test` whole suite green · `cargo clippy
   --all-targets` clean · `cargo fmt` · **Codex-xhigh review APPROVED** (`.ralph/REVIEW_PROTOCOL.md`) · commit.
5. Record discoveries/decisions back here (use a subagent); keep AGENTS.md operational-only.

---

## Pending tasks (priority order)

### Task 1 — P1: output_config.effort → reasoning.effort  ⟶ loop validation (quick win)
- **Spec:** `.ralph/specs/P1-output-config-effort.md` · **Files:** `src/adapters/anthropic_to_responses.rs`
- **Acceptance:** un-ignore `anthropic_output_config_effort_maps_to_reasoning_effort` in
  `tests/port_translation.rs`, add low/medium/max + adaptive-gating cases, make green.
- **Why first:** smallest, stub exists, isolated to one adapter — proves the loop + review gate.

### Task 2 — G5: debug-dump log rotation  (isolated)
- **Spec:** `.ralph/specs/G5-debug-log-rotation.md` · **Files:** `src/request_log.rs` (or new module), `src/config.rs`, wiring in `src/lib.rs`/`src/main.rs`
- **Acceptance:** new `tests/port_logging.rs` porting the 8 cleanup behaviors; cleanup off the async runtime (`spawn_blocking`).

### Task 3 — G1: context-window-limit retry  (HIGH)
- **Spec:** `.ralph/specs/G1-context-window-retry.md` · **Files:** `src/error.rs`, `src/upstream.rs`, `src/engine.rs`
- **Acceptance:** new `tests/port_errors.rs` porting 9 error-classification behaviors + 1 wiremock integration test (context-limit 400 → single retry → 200).
- **Constraint highlight:** retry pre-first-chunk ONLY; never duplicate streamed tokens.

### Task 4 — G3: pre-flight context budgeting  (MED · depends Task 3)
- **Spec:** `.ralph/specs/G3-context-budgeting.md` · **Files:** `src/engine.rs` (+ maybe `config.rs`/catalog)
- **OPEN QUESTION first:** find llmconduit's source of per-model context length; record the decision here before implementing.
- **Acceptance:** add cap/margin/overflow tests to `tests/port_server.rs`.

### Task 5 — G2: Kimi/DeepSeek family reshaping  (MED · serial after engine tasks)
- **Spec:** `.ralph/specs/G2-model-family-reshaping.md` · **Files:** `src/engine.rs`, reuse `src/adapters/chat_to_responses.rs`, maybe `src/config.rs`
- **Acceptance:** un-ignore the 2 stubs in `tests/port_routing.rs` + extend (resolved-model detection, always-on Kimi thinking, nested-thinking reshape composed with existing sentinel cleanup).

### Task 6 — G8: reasoning promotion/suppression heuristics  (MED · independent file)
- **Spec:** `.ralph/specs/G8-reasoning-stream-handling.md` · **Files:** `src/adapters/responses_to_anthropic.rs`
- **Acceptance:** new `tests/port_response_translation.rs` using shared SSE collectors; promote@stop / keep@length / signature-stays / late-drop.

### Task 7 — G6: SSE per-frame buffer cap  (MED · verify-first)
- **Spec:** `.ralph/specs/G6-sse-buffer-cap.md` · **Files:** `src/upstream.rs` / `src/adapters/chat_to_responses.rs`
- **VERIFY FIRST:** confirm whether `eventsource-stream` already caps frame growth; record finding here. Then add guard + `tests/port_streaming.rs` (or shrink scope to expose/document the cap).

---

## Completed tasks
_(none yet — compressed here as they finish; full detail archived to `.ralph/COMPLETED_TASKS.md`)_
