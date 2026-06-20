# U9 — TerminalReason tool_calls wire-string: delegate consumer + pin producer test

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** LOW · **Surface:** `src/models/responses.rs`, `src/adapters/responses_to_anthropic/mod.rs`, `tests/port_response_translation.rs` · **Thermo finding:** both sides of the `tool_calls` terminal-reason wire-string contract are unpinned/duplicated

## Purpose
The typed `TerminalReason` (T7) crosses an internal serialized boundary as a bare wire string, and BOTH ends of that contract are fragile. **Producer:** `TerminalReason` is `Serialize`-only (`src/models/responses.rs:506`) and its `ToolCall` variant carries a LOAD-BEARING `#[serde(rename = "tool_calls")]` (`responses.rs:519`) that overrides `#[serde(rename_all = "snake_case")]` — without it `serde` would emit `"tool_call"`. The engine serializes this enum onto the resource (`src/engine.rs:1853`, `terminal_reason: Some(terminal_reason)`) and the converter re-reads the `"tool_calls"` string. NO test pins the serialized spelling: drop the rename and the engine emits `"tool_call"`, the consumer match falls to `Other`, every G8 tool-call-promotion assertion silently still passes (they assert the *non-promoted* outcome, which `Other` also produces) — the suite goes blind to the regression. **Consumer:** `response_terminal_reason` (`src/adapters/responses_to_anthropic/mod.rs:814-826`) hand-rolls the exact `&str → TerminalReason` map (lines 819-825) that canonical `TerminalReason::from_finish_reason` (`responses.rs:538-546`) already provides. A NEW variant added to the enum + `from_finish_reason` would silently fall to `Other` in this duplicate match (latent G8 promotion-gate spec-drift). One pass dedups both ends: delegate the consumer to canonical, and pin the producer spelling with a unit test.

## Jobs to Be Done
- The producer's wire spelling of every `TerminalReason` variant is locked by an assertion, so dropping/altering the `#[serde(rename = "tool_calls")]` (or any future rename) fails a test instead of silently degrading the whole converter to `Other`.
- The consumer stops duplicating the canonical string→variant map and instead delegates to `TerminalReason::from_finish_reason`, so a future variant is mapped in exactly one place.
- Behavior is byte-identical on the wire and at the G8 promotion gate (no live-stream output changes).

## Acceptance criteria
- [ ] In `src/adapters/responses_to_anthropic/mod.rs`, `response_terminal_reason` replaces the inline `match reason { ... }` (lines ~819-825) with `.and_then(Value::as_str).map(|r| TerminalReason::from_finish_reason(Some(r)))` (or equivalent delegating call). The PRESENT-but-unrecognized → `Other`, and ABSENT → `None`, semantics are preserved exactly (`from_finish_reason(Some("other")) == Other`; `from_finish_reason(Some("anything_else")) == Other`; field absent ⇒ `None` — the doc comment at `mod.rs:807-813` stays accurate).
- [ ] No remaining hand-rolled `"stop"/"length"/"tool_calls"/"content_filter"` → `TerminalReason` match exists in `responses_to_anthropic/`; the only authoritative string→variant map is `TerminalReason::from_finish_reason` (`responses.rs:538`).
- [ ] A new producer unit test in `src/models/responses.rs` (`#[cfg(test)] mod tests`, ~:724) asserts `serde_json::to_value(&TerminalReason::X)` yields EXACTLY: `Stop`→`"stop"`, `Length`→`"length"`, `ToolCall`→`"tool_calls"`, `ContentFilter`→`"content_filter"`, `Other`→`"other"`. The `ToolCall`→`"tool_calls"` assertion is the load-bearing one (it fails if the `#[serde(rename = "tool_calls")]` at `responses.rs:519` is removed and snake_case would emit `"tool_call"`).
- [ ] (Recommended, optional belt-and-braces) A round-trip assertion that `from_finish_reason(serde_json::to_value(&v).unwrap().as_str().unwrap())` reproduces `v` for each variant, locking producer + canonical-mapper agreement.
- [ ] The existing converter behavior is unchanged: `reasoning_only_at_tool_calls_stays_thinking` (`tests/port_response_translation.rs:230`) still passes (reasoning prefacing a `tool_calls` terminal stays a `thinking` block, never promoted), and all G8 promotion tests (`stop`→text, `length`→thinking, signature→never, late reasoning→dropped) are unaffected.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/adapters/responses_to_anthropic/mod.rs` (`response_terminal_reason`), `src/models/responses.rs` (new producer serialization unit test in `mod tests`).
- **New APIs:** none. Reuses existing `TerminalReason::from_finish_reason` (`responses.rs:538`).
- **Depends on:** T7 (typed terminal reason) — FINAL/APPROVED. No other Topic-12 task touches these regions; no ordering constraint.

## Constraints
- The `#[serde(rename = "tool_calls")]` on `ToolCall` (`responses.rs:519`) is LOAD-BEARING and MUST remain — the wire string is the ONLY contract between the engine serializer (`engine.rs:1853`) and the converter re-reader. This task ADDS a test that pins it; it does NOT remove or relax it.
- G8 promotion gating MUST stay byte-identical: `is_clean_stop()` is `Stop`-only, every non-stop / unknown reason gates as non-clean (never promote). The delegation must preserve PRESENT-but-unrecognized → `Other` (NOT `None`) so the converter never falls back to the event-type string for a tagged-but-unknown reason (the T7 R1 invariant in the `mod.rs:807-813` doc comment).
- `TerminalReason` stays `Serialize`-only (no `Deserialize`) — the consumer parses via `as_str` + `from_finish_reason`, not via `serde::Deserialize`. Do not add a `Deserialize` derive.
- Live-stream wire output is unchanged; this is an internal-contract dedup + test, not a wire-contract fix.

## Out of scope
- Any change to `from_finish_reason` itself, the `TerminalReason` variant set, or G8 promotion logic.
- Removing or altering the `#[serde(rename = "tool_calls")]` attribute (the test exists to FORBID that drift, not enable it).
- Adding new terminal-reason variants or touching `engine.rs` terminal-emission logic.
- Re-raising adjudicated items (G8 emit_thinking toggle, G5 `.jsonl` exclusion) — FINAL.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
