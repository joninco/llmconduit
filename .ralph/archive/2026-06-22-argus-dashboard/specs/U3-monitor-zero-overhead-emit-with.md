# U3 — Restore MonitorHub zero-overhead: lazy emit_with choke-point

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** MEDIUM · **Surface:** src/engine.rs, src/monitor.rs · **Thermo finding:** Eager monitor-event construction runs on the DISABLED hot path — violates "MonitorHub::disabled() = zero-overhead no-op".

## Purpose
`MonitorHub::emit` early-returns when the hub is disabled (`src/monitor.rs:257-260`), and the project
invariant is that `MonitorHub::disabled()` is a **zero-overhead no-op** so production carries no
monitor cost. But several call sites in `engine.rs` build the `MonitorEventKind` argument *eagerly*,
before `emit` can early-return, so the work runs even when monitoring is off. The worst offender is
the `RequestStarted` event (`src/engine.rs:1061-1299`): it makes **nine** separate
`request.input.iter().filter(...).count()` passes over the whole input plus the `input_chars`
map/sum fold (`src/engine.rs:1165-1296`) — all on every request, disabled or not. The per-item sites are
worse per call: `summarize_response_item(&item)` + `preview_json(&item)` run a full
`serde_json::to_value` + `collect_data_image_cards` + image-URI redaction + `to_string_pretty` (up to
4 KB, `src/engine.rs:2579-2634`, `2725+`) for every streamed output item / tool phase. These run at
the **unguarded** sites `src/engine.rs:1310` (the `trailing_tool_output_items` ToolPhase loop), `1528`,
`1560`, `2093`, `2148`, `2239`, `2249`, `2322`, `2329`, `2384`. The request/upstream/final *payload*
previews at `src/engine.rs:1300`, `1412`, `1855` are already correctly wrapped in
`if self.monitor.is_enabled()` — the per-item sites were simply missed.

## Jobs to Be Done
- The disabled (`MonitorHub::disabled()`) path performs **zero** event-construction work: no input
  traversal/count passes, no `summarize_response_item`, no `preview_json`/`serde_json` serialization,
  no image-card collection or redaction.
- One lazy choke-point replaces the 3 explicit `is_enabled()` guards plus the per-site eager-build
  decisions across all 23 emit sites, so future `emit` sites cannot re-introduce the regression by
  accident.
- Enabled output is **byte-identical** on the wire and in the `/debug/ws` stream/snapshot.

## Acceptance criteria
- [ ] Add `MonitorHub::emit_with(&self, response_id: impl Into<String>, build: impl FnOnce() -> MonitorEventKind)`
      to `src/monitor.rs` that early-returns *before* invoking `build` when `!self.enabled`, and
      otherwise calls `build()` and feeds the owned `MonitorEventKind` through the exact same path as
      `emit` (sequence bump, `apply_event`, `prune_expired`, image-URI redaction at the broadcast
      choke point, `tx.send`). `emit` may delegate to `emit_with(id, || kind)` or remain — but the
      single redaction/broadcast logic must not be duplicated.
- [ ] Convert every `engine.rs` site whose `MonitorEventKind` argument does non-trivial work to
      `emit_with(..., || MonitorEventKind::… )`, moving the arg-building into the closure. At minimum
      the unguarded sites: `RequestStarted` (`:1061-1299`, all nine count passes plus the `input_chars`
      map/sum fold inside the closure), the `trailing_tool_output_items` ToolPhase loop (`:1310-1318`,
      `summarize_response_item`), and the per-item `ResponseItem`/`ToolPhase` sites at `:1528`,
      `:1560`, `:2093`, `:2148`, `:2239`, `:2249`, `:2322`, `:2329`, `:2384` (each
      `summarize_response_item` + `preview_json`), and the four ToolPhase sites that build `detail`
      eagerly via `format!`/`preview_text` on the disabled path at `:2346`, `:2434`, `:2491`, `:2531`.
      Trivial sites (e.g. `OutputTextDelta`/
      `ReasoningTextDelta`/`RefusalDelta`/`FunctionCallArgumentsDelta` whose only payload is a
      `delta.clone()` at `:636`, `:1545`, `:1577`,
      `:1670`, and `MonitorEventKind::Completed` at `:1881`) MAY remain on `emit`, but cloning a
      streamed delta only when enabled is preferred — call this out in the PR if any are left eager.
- [ ] The three already-guarded payload-preview sites (`:1300`, `:1412`, `:1855`) are converted to
      `emit_with` (moving the preview build into the closure) and their `if self.monitor.is_enabled()`
      wrappers removed, so there is exactly one disabled-check mechanism. Preserve the *upstream*
      `sanitize_chat_request` + `flatten_content` behavior (`:1413-1414`) and the
      `preview_json_limited_with_images(..., 128 * 1024)` limit at all three — byte-identical preview.
- [ ] **Disabled-path zero-work proof:** add a unit test in `src/monitor.rs` (and/or a focused
      `engine.rs` test) that calls `emit_with` on a `MonitorHub::disabled()` and asserts the closure
      is NEVER invoked — e.g. an `AtomicBool`/`Cell` flipped inside the closure stays `false`, or the
      closure `panic!`s and the call does not. Mirror with an enabled hub asserting the closure IS
      invoked and the event reaches `snapshot()`/a `subscribe()` receiver.
- [ ] **Byte-identical enabled output:** existing `src/monitor.rs` snapshot/replay tests and the
      debug-UI integration coverage in `tests/gateway.rs` and `tests/port_streaming_peek.rs` stay
      green unchanged; the `/debug/ws` message bytes for a representative streamed turn (RequestStarted
      + ResponseItem add/done + payload previews) are unchanged vs `master`.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/monitor.rs` (`MonitorHub::emit` choke point at `:257-285`, `is_enabled` at
  `:253`), `src/engine.rs` (all `self.monitor.emit(` sites — 23 total).
- **New APIs:** `MonitorHub::emit_with(&self, id, FnOnce() -> MonitorEventKind)` (returns owned kind;
  no borrow of `self.monitor` captured across the closure).
- **Depends on:** none. `MonitorEventKind` already derives `Clone + Serialize` (`src/monitor.rs:14`),
  so an owned kind from the closure is a drop-in for the current eager argument.

## Constraints
- `MonitorHub::disabled()` MUST remain a true zero-overhead no-op: with the hub disabled, NO closure
  runs and NO event-building work (traversal/count/`summarize_response_item`/`preview_json`/
  serde/redaction) executes. This is the load-bearing invariant the finding targets.
- The image-URI redaction at the single broadcast choke point (`src/monitor.rs:272-279`) MUST NOT be
  bypassed — `emit_with` routes through the identical redaction so no raw image data/URL leaks via
  `/debug/ws`.
- Wire/`/debug/ws` output for the enabled path MUST be byte-identical: same event kinds, same
  ordering, same payload-preview limits (4 KB for `preview_json`, 128 KB for the payload previews),
  same `sanitize_chat_request`/`flatten_content` upstream handling.
- Do not change `MonitorEventKind` field semantics or the snapshot/prune/sequence logic.

## Out of scope
- No changes to event *shape* or debug-UI rendering. No new monitor event kinds.
- No reordering or coalescing of emit sites beyond the eager→lazy conversion.
- G8 `emit_thinking` toggle (absent by design — FINAL), G5 `.jsonl` exclusion, and the Topic-11
  refactors are not revisited.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
