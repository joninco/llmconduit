# Review protocol — Codex-only, xhigh reasoning, per-gap

Per-gap review gate for this Ralph run. **One gap at a time** (`--agents 1` serial), and **each gap's
commit is reviewed before the next gap starts**. Reviewer = **Codex at xhigh reasoning ONLY** — no
Gemini, no Opus (Opus is the implementer/orchestrator; it does not also review here).

> This REPLACES `ralph-orchestrate`'s built-in end-of-run multi-model review. Run the orchestrator
> with `--no-review` and `--agents 1`; this protocol is the review instead.

## Scope note — this is a DIAGNOSTIC dashboard (product-owner ruling 2026-06-22)
The Argus dashboard is an auth-gated DIAGNOSTIC tool; showing full request detail (headers, keys,
bodies) to the operator is the INTENDED PURPOSE, not a leak. When constructing the Codex review
prompt, instruct the reviewer to NOT flag "the dashboard exposes a credential / header / body to the
operator" as a finding. (Gap 04's review burned 3 rounds on spurious HIGH "key-leak" findings before
this was ruled out — don't repeat it.)
STILL load-bearing — reviewers MUST flag these (they are real for NON-security reasons):
memory/perf invariants (no retention of `Bytes` slices of the 256 MiB middleware body buffer — copy
via the capped/redacting serializer; no body on historical snapshots — body-free `SnapshotFlowSummary`
only), per-domain `{domain,seq}` cursors, cancellation selects on `tx.closed()`,
failover-pre-first-chunk-only, canonical-Responses-only, replay integrity, correctness, and the
cross-cutting **don't-lie-with-zeros** / data-quality-tag contract (the heart of every gap).

## When
After a gap's build subagent has: executable test green → `cargo test` green → `cargo clippy --all-targets`
clean → `cargo fmt` → committed. THEN, before selecting the next gap, run the review on that commit.

## How — primary (CLI, confirmed installed)
Pipe the gap's diff to Codex in read-only sandbox at xhigh reasoning:

```bash
git show <gap_commit_hash> | codex exec \
  -s read-only \
  -c model_reasoning_effort="xhigh" \
  "You are reviewing ONE gap implementation for the llmconduit Rust gateway. The unified diff is in
   the <stdin> block. Judge it against:
     - the gap spec: .ralph/specs/<GAP_ID>.md  (acceptance criteria + constraints)
     - AGENTS.md 'Hard rules in the engine' (load-bearing invariants — flag any violation)
   Focus: correctness, security, and regressions to existing behavior (esp. streaming cancellation,
   failover-pre-first-chunk, canonical-Responses-only, parallel_tool_calls=false, replay integrity).
   Output ACTIONABLE findings only, each as 'SEVERITY — file:line — problem — fix'. If clean, output
   exactly 'APPROVED'."
```

(`codex review` is also available; `codex exec` with a piped diff is preferred for determinism.)

## If Codex runs out of credits
If a `codex` invocation returns `ERROR: Your workspace is out of credits. Add credits to continue.`,
run `/home/jon/scripts/codex-account next` to switch to a funded account, then retry the SAME review
command. Repeat the rotation if the next account is also empty. Do not skip or substitute the review because of a credit error.

## How — alternative (in-session subagent)
When orchestrating in-session, spawn the `codex-agent` subagent instead, instructing it to consult
Codex at **xhigh reasoning** with the same diff + spec + AGENTS.md context. Do NOT also spawn
gemini-agent or an Opus reviewer for this gate.

## Acting on findings
- **APPROVED / no findings** → mark the gap done, proceed to the next gap.
- **Findings** → spawn a fix subagent (Opus) to address them, re-run `cargo test`/clippy/fmt, amend or
  add a follow-up commit, then **re-review with Codex-xhigh**. Repeat up to **3 review rounds** per gap;
  if still unresolved, record the open findings in `IMPLEMENTATION_PLAN.md` and stop for human input.
- Record each review outcome (commit hash, verdict, rounds) in the orchestrator state file.

## Non-negotiables
- A gap is NOT "done" until Codex-xhigh returns APPROVED (or findings are explicitly deferred in the plan).
- Never proceed to the next gap with an un-reviewed commit.
