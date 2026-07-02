# Goal: Topic F — durable per-turn capture (F1)

Branch `durable-turn-capture`. Run autonomously — no stops between tasks. Stop only on STOP criteria or
exhausted Codex accounts. Run via `/ralph-orchestrate --no-review --agents 1` (serial; **Sonnet-5
subagents**; the per-task Codex-xhigh review in `.ralph/REVIEW_PROTOCOL.md` REPLACES the built-in
end-of-run review).

> **Topic F — observability / debug durability.** Added 2026-07-01 from an operator need + `/askcodex`
> xhigh design review (all HIGH/MED findings folded into the spec). BACKEND (`src/*.rs`), independent of the
> completed Topic E / E2 work.

## Task
Execute `.ralph/IMPLEMENTATION_PLAN.md` **Topic F / F1** to completion: **F1a → F1b → F1c → F1d → F1e →
F1f** (serial). Spec: `.ralph/specs/F1-durable-turn-capture.md` (acceptance criteria AC-1..AC-17 = the
oracle).

**F1 in one line:** when a Claude Code session shows weird output (stray `<think>` tags, malformed tool
calls) there is NO durable record of the response today (it's a 200 OK; journal + JSONL are request-only;
the dashboard ring is capped/ephemeral). Add an opt-in `turn_capture_dir` that writes ONE atomic
`<dir>/<api_call_id>.json` per turn with the FULL inbound Anthropic request, translated OpenAI request, RAW
vLLM response, and served Anthropic bytes + outcome — bounded-memory (sections stream to temp files),
redacted (AGENTS.md:137/144), works WITHOUT `--with-debug-ui`, age-rotated via `debug_log_max_age_hours`.

## Per-task loop
1. Read `.ralph/specs/F1-durable-turn-capture.md` (acceptance = oracle) + the task row in the plan.
2. Implement to FULL — no stubs (except F1a's no-op-tested forward-decls). Honor AGENTS.md "Hard rules in
   the engine" + the spec's constraints:
   - OWN instrumentation gate — thread `api_call_id` + `capture.start()` on the whitelisted inference paths
     whenever `turn_capture` is enabled, independent of `flow_store().is_enabled()` (`http.rs:536`);
   - BOUNDED memory — sections stream INCREMENTALLY to per-turn temp files; NO `HashMap<_,fullbody>`; the
     single JSON is assembled by a streaming escaper; never retain a `Bytes` slice of the 256 MiB inbound
     buffer (AGENTS.md:144);
   - NEW logged surface ⇒ REDACT — route request bodies through `redact_image_uris_in_value` +
     `redact_payload_secrets` (AGENTS.md:137); do NOT bypass;
   - HYBRID finalize — engine terminal seam owns `status`/`terminal_reason` (incl. pre-spawn `engine.rs:1196`
     + a RAII Drop fallback mirroring `dashboard_flow.rs:1794/1917`); an HTTP response-`Body` wrapper owns
     served bytes (covers streaming + non-streaming `collect_anthropic_response` + error + disconnect);
     assemble the artifact only after BOTH report (flush/close barrier);
   - CARRIER = `capture: Option<Arc<TurnCaptureState>>` on `BackendChatRequest` (`upstream.rs:3288`), NOT
     `ServingToken`;
   - FINAL-attempt-only request+response (last-writer-wins) + the final failed HTTP body (gap-05 staged
     body); `attempts[]` is out of scope (documented follow-up);
   - ENCODING — UTF-8 SSE text; base64 + `_encoding` marker for non-UTF-8;
   - operator visibility is NOT a leak (REVIEW_PROTOCOL 2026-06-22) — the redaction is secrets-at-rest
     hygiene, don't flag operator exposure.
3. Gate **B** (backend): `cargo fmt --all` + `cargo test` + `cargo clippy --all-targets -- -D warnings`
   clean. Add the task's ACs as tests WITHIN the same task. Do not defer tests.
4. Commit: F1a `feat(config):`; F1b `feat(http):`; F1c `feat(engine):`; F1d/F1e `feat(upstream):`; F1f
   `feat(turn-capture):` + `docs:`.
5. **Codex-xhigh review** of the commit, diff on stdin, per `.ralph/REVIEW_PROTOCOL.md`:
   ```bash
   git show <task_commit> | codex exec -s read-only -c model_reasoning_effort="xhigh" "<prompt>"
   ```
   Prompt = judge the diff against the F1 spec ACs + AGENTS.md hard rules (esp. bounded-memory / no
   Bytes-slice, redaction, cancellation-no-hang, hybrid-finalize completeness). Output
   `SEVERITY — file:line — problem — fix` per finding, or exactly `APPROVED`. Append to
   `/tmp/argus-turn-capture-review.md`.
6. Findings → fix → re-run gate → amend/follow-up → **re-review**. Up to 3 rounds; if unresolved, record in
   the plan and halt. A task is NOT done until Codex-xhigh returns APPROVED.

## Live-verify (gates "done")
F1 touches the live `/v1/messages` path. After all tasks APPROVED:
- `cargo build --release`
- add `turn_capture_dir: /home/jon/.local/share/llmconduit/turns` to the `:5022` config; restart
  `llmconduit.service` (release binary).
- live-verify on :5022 (both with `--with-debug-ui` ON and, once, with it OFF):
  1. a normal CC turn → exactly one `<api_call_id>.json` with all four sections; `served_response` matches
     the client's bytes; no `.work` residue.
  2. a `<think>`-in-content response → the leak is visible in BOTH `upstream_response` and `served_response`
     (localizes upstream vs converter).
  3. a mid-stream Ctrl-C → `status:"cancelled"`, `served_response.partial=true`, written within ~1s (no hang).
  4. tiny `debug_log_max_age_hours` → old artifacts + orphan `.work` dirs pruned.

## Credits
Codex `ERROR: Your workspace is out of credits.` → run `/home/jon/scripts/codex-account next`, retry the
SAME command. Rotate across accounts. Halt only if all are exhausted (report which tasks remain).

## STOP when
- F1a–F1f each Codex-xhigh APPROVED + committed.
- `cargo test` + `cargo clippy --all-targets -- -D warnings` clean.
- F1 live-verified on :5022 (all four checks above, dashboard ON and OFF).
- `/tmp/argus-turn-capture-review.md` holds the F1a–F1f verdicts.

Print a final summary: tasks done, commit hashes, any deferred/halted items (e.g. `attempts[]` per-attempt
trace, `analyze-turn` CLI, chat/responses client-format parity), final commit hash.
