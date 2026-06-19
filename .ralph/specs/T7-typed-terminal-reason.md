# T7 — Typed terminal reason in the canonical response

> **Source:** thermo-nuclear review (G8 HIGH #12). See `/tmp/thermo-synthesis.md`.

**Priority:** MEDIUM · **Surface:** engine + responses_to_anthropic · **Thermo findings:** G8 HIGH (terminal reason type erased)

## Purpose
G8 gates reasoning promotion on `event_type == "response.completed"` vs `response.incomplete`
(`responses_to_anthropic.rs:468`). The engine emits `incomplete` only for `finish_reason:length`
(`engine.rs:1725`); all other finish reasons map to `completed`. This is correct for the spec's two
named cases (stop→promote, length→thinking) but encodes the rule at the wrong boundary: any future
non-stop terminal reason arriving as `response.completed` would wrongly promote. Preserve a typed
terminal reason in the canonical response so promotion gating uses an explicit reason.

## Jobs to Be Done
- Promotion gating asks "was this a clean stop?" via a typed reason, not by string-matching the
  event type.

## Acceptance criteria
- [ ] The canonical response carries a typed terminal reason (e.g. an enum: `Stop`, `Length`,
      `ContentFilter`, `ToolCall`, …), OR all non-stop terminal reasons map to non-clean status.
- [ ] `flush_reasoning_terminal` gates promotion on that explicit reason (clean stop only), not on
      `event_type == "response.completed"`.
- [ ] Existing G8 behavior preserved: reasoning-only@`stop` → text; @`length` → thinking; signature
      → never promoted; late reasoning → dropped.
- [ ] Tests: a future/non-stop terminal reason does NOT promote reasoning to text.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `engine.rs` (terminal event emission), `responses_to_anthropic.rs`
  (`flush_reasoning_terminal`), `models/responses.rs` (terminal reason field/enum).
- **Depends on:** nothing hard; coordinate with T8 (reasoning-egress-state) if both touch the
  converter — sequence T7 before T8 or merge the converter edits.

## Constraints
- `finalize()` MUST still emit synthetic `message_delta` + `message_stop` if the canonical stream
  ends without `response.completed` (AGENTS.md G8 constraint — clients must never hang).
- Preserve progressive output-usage estimation.

## Out of scope
- `emit_thinking` suppression (INVALID — toggle does not exist; spec criterion conditional).
- ReasoningEgressState extraction (T8).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
