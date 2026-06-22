# T11 — Streaming + logging test-quality cleanups

> **Source:** thermo-nuclear review (G3-peek MEDIUM #28, #29; G5 MEDIUM #22; G3 LOW #31; G7 LOW #32). See `/tmp/thermo-synthesis.md`.

**Priority:** LOW–MEDIUM · **Surface:** tests · **Thermo findings:** G3-peek MEDIUM (keepalive partial coverage), G3-peek MEDIUM (scheduler-magic test), G5 MEDIUM (false-positive race test), G3 LOW (catalog parser reparse), G7 LOW (port_config.rs 1,304 lines)

## Purpose
Five test-only quality findings from the thermo-nuclear review. No behavior risk, but each is a
maintenance / false-positivity hazard:
- G3-peek keepalive is only behaviorally tested on `/v1/messages`; `/v1/responses` checks headers
  only, `/v1/chat/completions` not covered (`port_streaming_peek.rs:250`).
- The keepalive proof uses a bespoke scheduler protocol (manual `poll!`, 256-iteration drain, 16
  consecutive `Pending` polls, side exit) — brittle (`port_streaming_peek.rs:168`).
- G5's "removal race" test never exercises a removal error because it deletes `raced.json` before
  cleanup reads the dir (`port_logging.rs:157`) — false acceptance.
- `extract_model_context_limits` reparses the same model entries that
  `extract_supported_model_catalog` already parses (`upstream.rs:3038`).
- `tests/port_config.rs` is 1,304 lines over the 1k smell threshold with repeated wiremock setup.

## Jobs to Be Done
- Keepalive coverage reflects the stated contract; the test harness is explicit, not scheduler-magic.
- The G5 race test actually triggers a removal error.
- The catalog parser parses once.
- `port_config.rs` is split by concern.

## Acceptance criteria
- [ ] G3-peek: parameterize the idle-ping harness across Anthropic, Responses, AND Chat streaming
      routes (or narrow the stated contract to what's actually tested).
- [ ] G3-peek: replace the 256-iteration scheduler protocol with a small explicit idle-body helper /
      focused stream harness (drain known prologue frames, assert idle once, advance time, assert
      the comment frame).
- [ ] G5: inject a remover/file-ops seam (or a deterministic post-metadata removal/permission
      failure) so `remove_file` actually returns `Err` while cleanup continues — the race test
      fails the cleanup-continues path for real.
- [ ] G3: store parsed `UpstreamModelEntry`/context metadata in the routing catalog; delete
      `extract_model_context_limits` (the second parser).
- [ ] G7: split `tests/port_config.rs` — pure config/TOML tests separate from HTTP routing tests;
      extract small route-target helpers.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `tests/port_streaming_peek.rs`, `tests/port_logging.rs`, `src/upstream.rs` (catalog),
  `tests/port_config.rs`.
- **Depends on:** T1/T9 for the catalog parser dedup (the routing catalog shape may change) —
  sequence the G3 LOW item after T1.

## Constraints
- Test-only changes must not alter production behavior (except the G3 LOW catalog-parser dedup, which
  is a pure refactor preserving the parsed result).
- Preserve the mutation-verified property of the G3-peek tests (deleting `.keep_alive(...)` still
  fails them).

## Out of scope
- G8 `emit_thinking` (INVALID). G5 `.jsonl` exclusion (INVALID — spec criterion).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
