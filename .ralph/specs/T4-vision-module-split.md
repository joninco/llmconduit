# T4 — Split vision.rs + image-agent test suite

> **Source:** thermo-nuclear review (G4 MEDIUM #20, #21). See `/tmp/thermo-synthesis.md`.

**Priority:** MEDIUM · **Surface:** src/vision.rs + tests/gateway.rs · **Thermo findings:** G4 MEDIUM (vision.rs grab bag), G4 MEDIUM (image-agent suite in 8k-line file)

## Purpose
`src/vision.rs` is a new 1,364-line grab bag: prompts, tool schema, cache, request mutation, HTTP
client, redaction, and tests all live together. The G4 image-agent integration suite was added
inline to `tests/gateway.rs` (8,193 lines) despite being separable by topic. Split both so each
concern is independently scannable.

## Jobs to Be Done
- A reader finds vision cache, strip logic, HTTP client, and redaction in separate focused modules.
- The image-agent test suite lives in its own file, sharing only common gateway builders/helpers.

## Acceptance criteria
- [ ] `src/vision.rs` split into `vision/cache.rs`, `vision/strip.rs`, `vision/client.rs`, and a
      non-vision `src/redaction.rs`; `vision/mod.rs` (or `vision.rs`) is the public seam re-exporting
      the API.
- [ ] No public API change — `vision::*` consumers compile unchanged.
- [ ] The image-agent suite + `MockVisionClient` move to a focused `tests/image_agent.rs`, sharing
      only common gateway builders/helpers from `tests/common/mod.rs`.
- [ ] `tests/gateway.rs` shrinks by the moved suite; no behavioral test changes.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/vision.rs`, `tests/gateway.rs`, `tests/common/mod.rs`.
- **New files:** `src/vision/{cache,strip,client}.rs`, `src/redaction.rs`, `tests/image_agent.rs`.

## Constraints
- Pure structural move + visibility tidying — NO behavior change. Tests stay green unchanged.
- Keep `ImageCache` LRU+TTL semantics intact.

## Out of scope
- ToolDeltaGate extraction (T3). Routing-candidate plan (T2).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
