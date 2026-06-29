# Goal: Topic E — engine robustness (Task E1, hallucinated-tool-call repair)

Branch `worktree-dashboard`. Run autonomously — no stops between tasks. Stop only on STOP criteria or
exhausted Codex accounts. Run via `/ralph-orchestrate --no-review --agents 1` (serial; the per-task
Codex-xhigh review in `.ralph/REVIEW_PROTOCOL.md` REPLACES the built-in end-of-run review).

> The **Argus dashboard program (gaps 01–16) is COMPLETE** (see IMPLEMENTATION_PLAN.md "Status" + the
> `01..16` specs). This goal is the NEXT body of work: **Topic E — engine robustness**, added
> 2026-06-29 from a field incident + `/askcodex` xhigh design review. It is BACKEND (`src/*.rs`),
> independent of the dashboard gaps.

## Task
Execute `.ralph/IMPLEMENTATION_PLAN.md` **Topic E** to completion — currently **Task E1** only.
Spec: `.ralph/specs/E1-hallucinated-tool-call-repair.md` (acceptance criteria = the oracle).

**E1 in one line:** a fallback model emitted a tool_call (`Grep`) not in the offered set; `finalize()`
(`src/adapters/chat_to_responses.rs:263`) hard-errored and ABORTED the SSE stream mid-flight. Implement
the Codex-xhigh A2+C design: `finalize()` CLASSIFIES unknown tool names into `rejected_tool_calls`
(no `Err`); `ToolDeltaGate` buffers-then-drops unknown-tool deltas (never reach the client); the engine
taints the batch, injects synthetic tool results, runs ONE bounded in-gateway repair round
(`UNKNOWN_TOOL_REPAIR_CEILING`, default 1 / max 2 — mirror `WEB_SEARCH_ROUNDS_HARD_CEILING`); on
exhaustion emit a STRUCTURED terminal failure per inbound format (Responses `response.failed` /
Anthropic `error` / Chat SSE error), never a raw `?` abort; add a closed-tool-set prevention note.

## Per-task loop
1. Read `.ralph/specs/E1-*.md` (acceptance = oracle) + the Topic-E task block in the plan.
2. Implement to FULL — no stubs. Honor AGENTS.md "Hard rules in the engine":
   - canonical-Responses-only (no bespoke per-inbound-format handling; the 3 converters just render the
     canonical terminal event);
   - failover is pre-first-chunk ONLY — the repair round is a SAME-provider in-gateway extra turn, NOT a
     failover and NOT a token-duplicating retry of already-streamed content;
   - `parallel_tool_calls=false` forced; mixed provider-side + client-side calls in one batch rejected;
   - reuse the EXISTING `ToolDeltaGate` 256 KiB/1 MiB caps + O(1) running-byte accounting (U7/T3);
   - `MonitorHub::disabled()` zero-overhead — emit the new `unknown_tool_rejected` /
     `unknown_tool_repair_exhausted` phases via `emit_with` only;
   - preserve every `tx.closed()` / abort-token cancel select across repair rounds.
3. Gate **B** (backend): `cargo fmt` + `cargo test` + `cargo clippy --all-targets` clean. Add the tests
   listed in the spec (unknown→rejected-not-Err; deltas hidden on all 3 formats; tainted batch; repair
   self-correct; ceiling→structured terminal per format with text preserved; cancellation mid-repair).
4. Commit (`feat:` — E1 adds recoverable behavior + new terminal events).
5. **Codex-xhigh review** of the commit, diff on stdin, per `.ralph/REVIEW_PROTOCOL.md`:
   ```bash
   git show <task_commit> | codex exec -s read-only -c model_reasoning_effort="xhigh" "<prompt>"
   ```
   Prompt = judge the diff against the E1 spec acceptance criteria + AGENTS.md hard rules. Output
   `SEVERITY — file:line — problem — fix` per finding, or exactly `APPROVED`. Append the verdict to
   `/tmp/argus-engine-review.md`.
6. Findings → fix → re-run gate → amend/follow-up → **re-review**. Up to 3 rounds; if unresolved, record
   in the plan and halt. A task is NOT done until Codex-xhigh returns APPROVED.

## Live-verify (gates "done")
E1 touches the live `/v1/messages` tool path. After APPROVED:
- `cargo build --release`
- restart `llmconduit.service` (release binary, port 5022)
- live-verify on :5022: drive a real Claude Code agentic turn that previously triggered
  `unknown tool returned by upstream: Grep` and confirm the stream now RECOVERS (repair round) or ends
  with a STRUCTURED terminal error — never a raw mid-stream abort.

## Credits
Codex `ERROR: Your workspace is out of credits. Add credits to continue.` → run
`/home/jon/scripts/codex-account next`, retry the SAME command. Rotate across accounts. Halt only if all
are exhausted (report which tasks remain).

## STOP when
- Task E1 Codex-xhigh APPROVED + committed.
- `cargo test` + `cargo clippy --all-targets` clean.
- E1 live-verified on :5022.
- `/tmp/argus-engine-review.md` holds the E1 verdict.

Print a final summary: task done, commit(s), any deferred/halted items, final commit hash.
