# Goal: Implement Topic 11 — thermo-nuclear code-quality follow-ups

Branch `ralph/thermo-followups`. Run autonomously — no stops between tasks. Only stop on STOP criteria or exhausted Codex accounts.

## Task
Execute `.ralph/IMPLEMENTATION_PLAN.md` Topic 11 (tasks 11.1–11.11) to completion. Specs are in `.ralph/specs/T*.md` (T1=leaf-profile-resolution … T11=streaming-test-quality).

Follow the plan's sequencing verbatim:
- **T1 first** — it builds the typed model-resolver that T2 and T9 consume.
- Then **T2** and **T9** (both depend on T1).
- Then **T7 → T8** (T8 consumes T7's typed terminal reason).
- **T5 ↔ T6 coordinate** — if both land, T5's `Bytes` specialization goes in T6's new guard module.
- **T3, T4, T10, T11** independent (schedule after their deps; T11's catalog-parser item depends on T1).

## Per-task loop
1. Read the task's `.ralph/specs/T<N>.md` (acceptance criteria = the oracle).
2. Implement.
3. `cargo fmt` + `cargo test` + `cargo clippy --all-targets` clean.
4. Commit (`refactor:` for structural moves/extractions, `feat:` only if a task adds behavior — most are `refactor:`).
5. **Codex-xhigh review** — serial, read-only, diff on stdin, per `.ralph/REVIEW_PROTOCOL.md`:
   ```bash
   git show <task_commit> | codex exec -s read-only -c model_reasoning_effort="xhigh" -c model="gpt-5.5" "<prompt>"
   ```
   Prompt = thermo-nuclear lens (structural simplification, 1k-line smell, spaghetti, magic mechanisms, boundary cleanliness, canonical-helper reuse) + judge against the task's spec acceptance criteria + AGENTS.md "Hard rules in the engine". Output ACTIONABLE findings (`SEVERITY — file:line — problem — fix`) or `APPROVED`.
6. Append each task's raw Codex verdict to `/tmp/thermo-followup-review.md`.
7. If findings: fix → re-run fmt/test/clippy → amend or follow-up commit → **re-review**. Up to **3 review rounds** per task; if still unresolved, record the open findings in `.ralph/IMPLEMENTATION_PLAN.md` and halt for human input.
8. A task is **NOT done** until Codex-xhigh returns APPROVED (or findings are explicitly deferred in the plan).

## T1 live-verify (gates T2)
T1 (`leaf-profile-resolution`) touches the effort leaf (`upstream::finalize_request_for_backend`). After T1 is APPROVED:
- `cargo build --release`
- restart `llmconduit.service` (release binary at `/usr/local/bin/llmconduit`, port 5022)
- live-verify `claude --effort high` / `--effort max` / `--effort off` → GLM `chat_template_kwargs` correct on :5022
- only then proceed to T2.

## Credits
If Codex returns `ERROR: Your workspace is out of credits. Add credits to continue.`: run `/home/jon/scripts/codex-account next`, retry the same command. Rotate across accounts. Halt only if all accounts exhausted (report which tasks remain).

## STOP when
- All 11 tasks (11.1–11.11) Codex-xhigh APPROVED + committed.
- `cargo test` + `cargo clippy --all-targets` clean.
- T1 live-verified on :5022.
- `/tmp/thermo-followup-review.md` holds 11 verdicts.

Print final summary: tasks done (by ID), commits per task, any deferred/halted items, final commit hash.
