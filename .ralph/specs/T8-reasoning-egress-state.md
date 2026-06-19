# T8 — Extract ReasoningEgressState from responses_to_anthropic

> **Source:** thermo-nuclear review (G8 MEDIUM #27). See `/tmp/thermo-synthesis.md`.

**Priority:** MEDIUM · **Surface:** src/adapters/responses_to_anthropic.rs · **Thermo findings:** G8 MEDIUM (reasoning state scattered across converter fields)

## Purpose
G8 added more cross-cutting state to the already-2,020-line `responses_to_anthropic.rs`. Reasoning
policy now lives across `reasoning_buffer`, `reasoning_signature`, `content_started`,
`has_tool_calls`, and repeated `flush_*` calls in unrelated handlers. Extract a
`ReasoningEgressState` / terminal-disposition helper so the promotion/suppression matrix is one
typed state machine instead of scattered conditionals; split the stream converter, collector, and
tests into focused modules.

## Jobs to Be Done
- The reasoning promotion/suppression matrix reads as one typed state machine, not fields +
  conditionals spread across a 2k-line converter.

## Acceptance criteria
- [ ] A `ReasoningEgressState` (or terminal-disposition helper) owns `reasoning_buffer`,
      `reasoning_signature`, `content_started`, `has_tool_calls`, and the flush/promote/hold decisions.
- [ ] `AnthropicStreamConverter` / `AnthropicStreamCollector` delegate reasoning decisions to it.
- [ ] The converter, collector, and tests split into focused modules/files (e.g.
      `responses_to_anthropic/{stream,collector,reasoning}.rs` or separate files) under 1k lines each.
- [ ] No behavior change — G8 + G3-peek reasoning-deferral tests stay green.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/adapters/responses_to_anthropic.rs`.
- **New APIs:** `ReasoningEgressState` (internal).
- **Depends on:** T7 (typed terminal reason) if both touch the converter — sequence T7 first so the
  state machine consumes a typed reason.

## Constraints
- Pure structural extraction — NO behavior change.
- Preserve the `finalize()` synthetic-emit + progressive usage contracts (AGENTS.md G8).

## Out of scope
- Typed terminal reason (T7). `emit_thinking` (INVALID).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
