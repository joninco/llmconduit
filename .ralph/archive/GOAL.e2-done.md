# Goal: Topic E — engine robustness (Task E2, graceful image degradation)

Branch `graceful-image-degradation`. Run autonomously — no stops between tasks. Stop only on STOP criteria
or exhausted Codex accounts. Run via `/ralph-orchestrate --no-review --agents 1` (serial; **Sonnet-5
subagents**; the per-task Codex-xhigh review in `.ralph/REVIEW_PROTOCOL.md` REPLACES the built-in
end-of-run review).

> **Topic E — engine robustness.** Task E2 added 2026-07-01 from a field incident + `/askcodex` xhigh
> design review (blocking findings folded into the spec). BACKEND (`src/*.rs`), independent of the
> dashboard gaps and the completed Anthropic-SSE-conformance work (archived plan:
> `.ralph/archive/IMPLEMENTATION_PLAN.anthropic-sse.md`).

## Task
Execute `.ralph/IMPLEMENTATION_PLAN.md` **Topic E / E2** to completion: **E2a → E2b → E2c** (serial).
Spec: `.ralph/specs/E2-graceful-image-degradation.md` (acceptance criteria AC-1..AC-9 = the oracle).

**E2 in one line:** an image in a claude-cli tool chain hit a text-only upstream → vLLM `400 "not a
multimodal model"` → the 400 tripped a 30 s provider cooldown → every request `502`'d. Two independent
fixes: **E2a** — a request-intrinsic 4xx `{400,413,415,422}` becomes `FailoverDisposition::Terminal` (no
cooldown, no failover), while `401/403/404/408/429`/5xx keep today's failover+cooldown (disposition matrix
in the spec is the oracle); **E2b** — a role-agnostic residual-image pass at the engine canonical layer
replaces any `InputImage` (both `image_url` and `file_id`) reaching a non-native-vision backend with an
instructive text placeholder (default) or rejects the turn with a 4xx (`unsupported_image_policy`);
degraded turns bypass the replay cache; **E2c** — docs.

## Per-task loop
1. Read `.ralph/specs/E2-graceful-image-degradation.md` (acceptance = oracle) + the task row in the plan.
2. Implement to FULL — no stubs. Honor AGENTS.md "Hard rules in the engine" + the spec's constraints:
   - canonical-Responses-only (both transforms on `ResponsesRequest`, not per-inbound-format, not at the
     leaf);
   - failover is pre-first-chunk ONLY — E2a only reclassifies an already-failed attempt's disposition (no
     retry, no token duplication);
   - the residual-image pass is a TRUE choke point (role-agnostic; covers non-user + `file_id` +
     old-history + `tool_choice="none"` residuals) and must NOT fire on native-vision passthrough nor
     double-transform images the active agent already stripped;
   - placeholder is byte-identical across turns (no `Uuid`/clock/`HashMap`-order); degraded turns skip the
     replay cache (`replay.rs:77` collision);
   - `Reject` → a 4xx bad-request `AppError`, NOT `AppError::upstream` (which is 502);
   - config in BOTH `Config` and `PersistedConfig` + default + env override + `configure()` preserve;
   - `MonitorHub::disabled()` zero-overhead — emit the new degradation phase via `emit_with` only;
   - out of scope (documented, do NOT implement): `ContentItem::Other`, non-image binaries,
     `/v1/completions`, `count_tokens`.
3. Gate **B** (backend): `cargo fmt --all` + `cargo test` + `cargo clippy --all-targets -- -D warnings`
   clean. Add the ACs as tests WITHIN the same task (E2a → AC-1/2/3; E2b → AC-4/5/7/8/9). Do not defer
   tests to a later task.
4. Commit: E2a `fix(upstream):`, E2b `feat(engine):`, E2c `docs:`.
5. **Codex-xhigh review** of the commit, diff on stdin, per `.ralph/REVIEW_PROTOCOL.md`:
   ```bash
   git show <task_commit> | codex exec -s read-only -c model_reasoning_effort="xhigh" "<prompt>"
   ```
   Prompt = judge the diff against the E2 spec ACs + the disposition matrix + AGENTS.md hard rules. Output
   `SEVERITY — file:line — problem — fix` per finding, or exactly `APPROVED`. Append to
   `/tmp/argus-engine-review.md`.
6. Findings → fix → re-run gate → amend/follow-up → **re-review**. Up to 3 rounds; if unresolved, record in
   the plan and halt. A task is NOT done until Codex-xhigh returns APPROVED.

## Live-verify (gates "done")
E2 touches the live `/v1/messages` path. After all tasks APPROVED:
- `cargo build --release`
- restart `llmconduit.service` (release binary, port 5022)
- live-verify on :5022 against a text-only upstream:
  1. `unsupported_image_policy=placeholder` → a `/v1/messages` turn carrying an image is served `200`; the
     upstream JSONL shows the placeholder text (no image bytes); a follow-up text request is served (the
     provider is NOT in cooldown).
  2. a synthetic upstream `400` on a non-image request → the provider is NOT cooled; an unrelated request
     still returns `200`.
  3. `unsupported_image_policy=reject` → the turn ends with an HTTP 4xx Anthropic `error` body; provider not
     cooled.

## Credits
Codex `ERROR: Your workspace is out of credits.` → run `/home/jon/scripts/codex-account next`, retry the
SAME command. Rotate across accounts. Halt only if all are exhausted (report which tasks remain).

## STOP when
- E2a, E2b, E2c each Codex-xhigh APPROVED + committed.
- `cargo test` + `cargo clippy --all-targets -- -D warnings` clean.
- E2 live-verified on :5022 (all three checks above).
- `/tmp/argus-engine-review.md` holds the E2a/E2b verdicts.

Print a final summary: tasks done, commit hashes, any deferred/halted items (e.g. AC-3b non-streaming
status-preservation, AC-6 header/dashboard follow-up), final commit hash.
