# Goal: Argus dashboard phase 2 — FEATURES.md build-order items 1–10

Branch `worktree-dashboard`. Run autonomously — no stops between gaps. Stop only on STOP criteria or
exhausted Codex accounts. Run via `/ralph-orchestrate --no-review --agents 1` (serial; the per-gap
Codex-xhigh review in `.ralph/REVIEW_PROTOCOL.md` REPLACES the built-in end-of-run review).

## Task
Execute `.ralph/IMPLEMENTATION_PLAN.md` gaps **01–16** to completion, in order. Specs in
`.ralph/specs/NN-*.md` (acceptance criteria = the oracle). Scope = FEATURES.md PROPOSED (🔭) features
only; items 11–15 are explicitly deferred to a later `/ralph-guide-update`.

## Sequencing (verbatim — FEATURES.md "Suggested build order")
- **01 first** (stats-strip 🐞) — the foundation every gauge reads off.
- **Then the spine (02–07)** — backend data-contract seams; mutually independent; ALL before any UI.
- **Then surfaces (08–16)** — each gated on its backend dep: 08←07, 09←06, 10←02, 11←03, 12←03,
  13←12, 14←05, 15←04, 16←02/03/04/07/**12**. **16 last.**
- **Confirm with code search before assuming a seam is missing** — several already partly exist (see the
  plan's verified-state table). Extend, don't rebuild; do NOT add a second max-context parser.

## Per-gap loop
1. Read `.ralph/specs/<NN>-*.md` (acceptance = oracle) + the plan's verified-state row for that gap.
2. Implement to FULL — no stubs/placeholders. Honor AGENTS.md "Hard rules in the engine" + the dashboard
   Don'ts (per-domain `{domain,seq}` cursors; body-free snapshots; no secrets in `Config`;
   copy-not-slice the body buffer).
3. **Every gap, non-negotiable:** data-quality tags (`measured`/`derived`/`estimated`/`unavailable`) +
   **don't-lie-with-zeros** (an unmeasurable value renders `unavailable`/`—`, NEVER a fabricated `0`;
   a genuine measured `0` is distinct from `unavailable`).
4. Gate green:
   - **Backend gaps** (02, 03, 04, 05, 12; the backend half of 01): `cargo fmt` + `cargo test` + `cargo clippy --all-targets`.
   - **Frontend gaps** (08–11, 13–16; the frontend half of 01), run in `dashboard-frontend/`:
     `npm run typecheck` + `npm run lint` + `npm run test` + `npm run e2e`.
   - **B+F contract-migration gaps** (06, 07): BOTH gates above must pass in the SAME commit — these change
     a dashboard JSON contract (Rust `context_limit`/`FlowUsage` + the TS types/mocks/WS atomically), so a
     backend-only commit would leave the React app stale. (Codex review.)
5. Commit (`fix:` for 01; `feat:` for new seams/surfaces; `refactor:` for structural).
6. **Codex-xhigh review** — serial, read-only, diff on stdin, per `.ralph/REVIEW_PROTOCOL.md`:
   ```bash
   git show <gap_commit> | codex exec -s read-only -c model_reasoning_effort="xhigh" "<prompt>"
   ```
   Prompt = judge the diff against the gap spec's acceptance criteria + AGENTS.md hard rules + the two
   discipline rules (data-quality tags + no fabricated zeros). Output `SEVERITY — file:line — problem —
   fix` for each finding, or exactly `APPROVED`.
7. Append each verdict to `/tmp/argus-phase2-review.md`.
8. Findings → spawn a fix subagent → re-run the gate → amend/follow-up commit → **re-review**. Up to
   **3 rounds** per gap; if still unresolved, record the open findings in `.ralph/IMPLEMENTATION_PLAN.md`
   and halt for human input.
9. A gap is NOT done until Codex-xhigh returns APPROVED (or findings explicitly deferred in the plan).
   Never start the next gap with an un-reviewed commit.

## Live-verify (after 01, and again after the spine 02–07)
Release binary on :5022 (`--with-debug-ui`, `/dashboard`). Confirm the stats strip is honest under live
streaming traffic, and the inspector shows real phase/attempt data — nothing renders a fabricated `0`.

## Credits
Codex `ERROR: Your workspace is out of credits. Add credits to continue.` → run
`/home/jon/scripts/codex-account next`, retry the SAME command. Rotate across accounts. Halt only if all
are exhausted (report which gaps remain).

## STOP when
- All 16 gaps Codex-xhigh APPROVED + committed.
- `cargo test` + `cargo clippy --all-targets` clean; `dashboard-frontend` typecheck/lint/test/e2e green.
- 01 + the spine live-verified on :5022.
- `/tmp/argus-phase2-review.md` holds a verdict per gap.

Print a final summary: gaps done (by ID), commit per gap, any deferred/halted items, final commit hash.
