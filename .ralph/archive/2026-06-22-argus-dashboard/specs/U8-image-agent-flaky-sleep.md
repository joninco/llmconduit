# U8 — Replace flaky wall-clock sleep() test sync with deterministic Notify

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** LOW · **Surface:** `tests/image_agent.rs`, `tests/common/mod.rs` · **Thermo finding:** two flaky wall-clock `sleep()` test-sync points (50ms / 100ms) instead of the bounded `Notify`/timeout idiom that already exists in-repo.

## Purpose
Two image-agent integration tests synchronize on raw wall-clock `sleep()` instead of the deterministic, bounded `Notify`/timeout idiom already proven in `tests/gateway.rs:5764-5773`. (1) `image_agent_cancellation_drops_vision_work` (`tests/image_agent.rs:356`) drops the stream while the mock vision call is blocked, then `tokio::time::sleep(50ms)` (`:379`) before asserting `vision.requests().await.len() == 1` (`:381`) — it relies on the spawned turn observing the closed channel inside 50ms of unscheduled real time. (2) `upstream_request_log_redacts_image_data_when_agent_disabled` (`tests/image_agent.rs:1561`) drains the HTTP response, then `tokio::time::sleep(100ms)` (`:1629`, comment-admitted "give the … writer a moment to flush") before asserting `!contents.is_empty()` (`:1637`); the JSONL write is AWAITED inside `logged_send_chat_request` (`src/upstream.rs:628`) before the response streams, so the file is already on disk by the time the test drains the response and the 100ms sleep is at best redundant rather than guarding a tight race. The bounded poll-until-non-empty inside `timeout(1s)` is still the right fix (a deterministic wait that fails fast on real regression instead of relying on elapsed real time). For the cancellation test, by contrast, under CI thread-pool contention the 50ms sleep can elapse before the awaited drop work completes, producing flaky false failures. The mock already has a one-directional block hook (`MockVisionClient::block_on`, `tests/common/mod.rs:323`) but no signal back to the test for "analyze entered" or "turn dropped", which is why the first test must guess. This is a test-only soundness fix; no production code or wire behavior changes.

## Jobs to Be Done
- Both tests synchronize on explicit completion signals (bounded `timeout`), never on elapsed real time.
- `MockVisionClient` exposes an "analyze entered" signal so a test can await the vision future actually starting before it drops the stream, and a "vision-work dropped/observed-cancel" signal so it can await the spawned turn reacting — removing the 50ms guess.
- The log-redaction test awaits the JSONL becoming non-empty via a bounded poll (or a writer-flush signal), removing the 100ms guess.
- Tests pass deterministically under `tokio::time::pause()`-style/contended scheduling; sleeps are gone, not just shortened.

## Acceptance criteria
- [ ] `MockVisionClient` (`tests/common/mod.rs:301`) gains an `entered: Arc<Notify>` (notified at the top of `analyze`, after the request is recorded — `tests/common/mod.rs:334`) and a `dropped: Arc<Notify>` (fired from a drop guard inside `analyze` so it signals on cancellation, mirroring `NotifyOnDrop` at `tests/gateway.rs:179-187`), with accessors returning `.notified()` futures; existing `block_on`/`requests`/`push_outcome` semantics unchanged.
- [ ] `image_agent_cancellation_drops_vision_work` (`tests/image_agent.rs:356-382`) is rewritten so every `.notified()` future is captured BEFORE the action that fires it (`notify_waiters()` stores no permit, so a future created after its trigger can miss the wake and hang to the 1s timeout): obtain `let entered = vision.entered();` and `let dropped = vision.dropped();` (each the `notify.notified()` future) UP FRONT, then `timeout(Duration::from_secs(1), entered).await.expect(...)` BEFORE `drop(stream)` (so the assertion `requests().len() == 1` is no longer racing the analyze call recording its request), then `drop(stream)`, then `timeout(Duration::from_secs(1), dropped).await.expect(...)` AFTER the drop (the `dropped` future was already captured before the drop) — both `.expect(...)` with a descriptive message — fully replacing `tokio::time::sleep(Duration::from_millis(50))` at `:379`. The `len() == 1` assertion is preserved.
- [ ] `upstream_request_log_redacts_image_data_when_agent_disabled` (`tests/image_agent.rs:1561-1646`) replaces `tokio::time::sleep(Duration::from_millis(100))` at `:1629` with a bounded poll-until-non-empty wrapped in `timeout(Duration::from_secs(1), …)` (or an equivalent writer-flush signal), `.expect(...)` on timeout. All three existing content assertions (`!contents.is_empty()`, NOT-contains `iVBORw0KGgo`, contains `<redacted uri>` — `:1637-1645`) and the temp-dir cleanup are preserved unchanged.
- [ ] No `tokio::time::sleep` (or `std::thread::sleep`) remains in either test; grep-checkable. No production source file under `src/` is modified.
- [ ] Both tests still fail if their property regresses (drop the `entered`/`dropped` awaits or the redaction call and confirm a real failure, not a silent pass), preserving the mutation-verified value of the originals.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `MockVisionClient` in `tests/common/mod.rs` (new `entered`/`dropped` `Arc<Notify>` fields + accessors; `analyze` notifies + installs a drop guard).
- **Reuses (do not duplicate):** the bounded `timeout(Duration::from_secs(1), notify.notified())` pattern from `tests/gateway.rs:5764-5773` and the `NotifyOnDrop` drop-guard shape from `tests/gateway.rs:179-187`.
- **Depends on:** nothing; test-only, isolated to the two named tests and the mock.

## Constraints
- Test-only change: no production behavior, no on-the-wire bytes, and no `src/` file may change. The image-agent cancellation contract (drop the receiver, do not hang on the vision future) and the JSONL redaction contract (no raw `data:`/base64 image bytes on disk; `<redacted uri>` marker present) are the properties under test and must remain exactly as asserted.
- Use a bounded `timeout` (1s) on every new wait so a real hang/regression fails fast instead of deadlocking the suite; never an unbounded `notified().await`.
- Register every `.notified()` future before the action that fires it — `notify_waiters()` does not store a permit (mirrors gateway.rs:5707/5768).
- The `entered` notify must fire AFTER the request is pushed (`tests/common/mod.rs:334`) so awaiting it guarantees `requests()` already observes the call. The `dropped` notify must fire on the cancellation/drop path (drop guard), not only on normal return.
- The `dropped` drop guard is a plain RAII guard that fires when the `analyze` future is dropped at its blocked await point (`tests/common/mod.rs:337`); because `analyze` stays blocked in this test, the guard fires only on the cancellation path, so there is no need to disarm it on normal return.
- `MockVisionClient` keeps `#[derive(Clone, Default)]` working (the new `Arc<Notify>` fields default cleanly); `block_on` and the existing waiting behavior at `tests/common/mod.rs:336-338` are untouched.

## Out of scope
- Changing the `spawn_blocking` request-log writer (`src/upstream.rs:481`) or making the writer expose a production flush hook — the fix stays in the test via a bounded poll.
- Any other `sleep()`/timing pattern elsewhere in the suite not named here.
- Re-litigating adjudicated FINAL items (G8 `emit_thinking`, G5 `.jsonl` exclusion, the Topic-11 refactors).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
