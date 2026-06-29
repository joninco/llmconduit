# E1 — Bounded soft-reject repair for hallucinated upstream tool calls ⚙️

> Ralph gap spec — implementation-free. Engine robustness (NOT a dashboard gap).
> **Source:** field incident 2026-06-29 + `/askcodex` gpt-5.5 xhigh design review.
> **Surface (backend):** `src/adapters/chat_to_responses.rs` (`finalize`), `src/engine.rs` (tool-loop
> at `state.finalize(&tool_registry)?`), `src/tool_delta_gate.rs`, `src/monitor.rs` (phase),
> tests in `tests/`. **Depends on:** nothing (independent of the dashboard program 01–16). Plays well
> with the dashboard — its new monitor phases + counter feed the existing flow/topology surfaces.

## Operator question
"A request died with `API Error: unknown tool returned by upstream: Grep` — why, and why did the
whole stream break instead of recovering?"

## Incident / root cause (verified)
Claude Code sent `/v1/messages` with 64 tools (Bash/Read/Edit/Write/Agent/Task*/mcp__*) but NOT
`Grep` (CC DEFERS `Grep`/`Glob`/`ToolSearch`/`TodoWrite` behind a meta `ToolSearch` tool — their
schemas load on demand, so they are NOT offered each turn). The request fell back to the loaded vLLM
model (DeepSeek-V4-Flash standing in for `claude-opus-*` — expected per the claude-relay-parity
config). DeepSeek emitted a tool_call named `Grep` — a canonical CC tool it knows from training but
that was NOT in the offered set this turn. `finalize()` (`src/adapters/chat_to_responses.rs:263`)
does `registry.get("grep") → None → AppError::upstream("unknown tool returned by upstream: Grep")`;
the engine tool-loop (`src/engine.rs:2253` `state.finalize(&tool_registry)?`) propagates `?` and
ABORTS the SSE stream. Because this happens MID-STREAM (200 + SSE headers already sent), it cannot
become a clean HTTP 4xx — it lands in the SSE body / kills the session. It is currently INVISIBLE in
`journalctl` (no tracing line; surfaces only to the client).

## Decision (Codex xhigh): A2 + C — bounded soft-reject REPAIR, not pass-through/alias/skip
Treat an unoffered tool name as a RECOVERABLE upstream generation error:
1. **Classify, don't throw** — `finalize()` collects unknown-tool calls into a `rejected_tool_calls`
   list instead of returning `Err`. (Hard errors STAY for malformed KNOWN-tool args, missing function
   name, invalid `local_shell`, etc.)
2. **Hide from the client** — a hallucinated tool call (and its streamed argument deltas) MUST NOT
   reach the caller. Extend `ToolDeltaGate` so unknown-tool deltas are buffered until the name is
   known+validated, then dropped on reject (today the gate only protects `analyzeImage`).
3. **Repair in-gateway** — when any call in a batch is rejected, taint the WHOLE assistant tool batch
   (execute no server tool, hand off no client tool from it), append an internal-only assistant
   message with the attempted calls + a synthetic `tool` result per call, and do ONE extra internal
   upstream round so the model can self-correct (same loop shape as the web-search round at
   `src/engine.rs:2253-2356`). Cap = 1 repair round default, 2 max.
4. **Bounded terminal** — past the ceiling, emit a STRUCTURED terminal failure per inbound protocol
   (Responses `response.failed`, Anthropic `error`, Chat SSE error), NOT a raw `?` abort.
5. **Prevention (C)** — add a developer/system note that the tool set is a closed set ("use only the
   provided tools; if `ToolSearch` is available, use it to request deferred tools").

Rejected alternatives (Codex): A1 plain-skip (misleading empty success if the model emitted ONLY the
bad tool); B alias/auto-inject (tool names imply schema/permissions/audit — a correctness+safety bug;
also violates CC's deliberate deferred-tool design); D pass-through (pushes the upstream bug to the
client, violates the offered-tools contract); E hard-error (current — worst; broken stream).

## Scope — what to build
- `FinalizedAssistantTurn` (`chat_to_responses.rs:52`) gains
  `rejected_tool_calls: Vec<RejectedToolCall { call_id, name, raw_arguments, reason }>` with
  `enum ToolRejectionReason { UnknownTool }`. `finalize()` routes `registry.get(name_lc) == None` into
  it (NO JSON parse of unknown args — carry raw, capped text for logging only).
- `ToolDeltaGate` (`src/tool_delta_gate.rs`): generalize the buffer-until-name mechanism so unknown
  client-tool names are buffered+dropped (not just `analyzeImage`); reuse the EXISTING per-call /
  total pending-byte caps (256 KiB / 1 MiB) — no new caps, no O(n²).
- Engine tool-loop (`src/engine.rs:2253`): on non-empty `rejected_tool_calls`, run the repair round
  (synthetic tool results + extra upstream turn), bounded by a new `UNKNOWN_TOOL_REPAIR_CEILING`
  constant (mirror the `WEB_SEARCH_ROUNDS_HARD_CEILING` pattern at `engine.rs:2340`); on exhaustion
  emit the structured terminal failure through the canonical Responses path.
- `src/monitor.rs`: emit monitor phases `unknown_tool_rejected` and `unknown_tool_repair_exhausted`
  via the existing `emit_with` choke-point (zero-overhead when disabled).

## Acceptance criteria
- [ ] `finalize()` no longer returns `Err` for an unknown tool name; it populates `rejected_tool_calls`.
      Malformed KNOWN-tool args / missing name / invalid `local_shell` STILL hard-error (unchanged).
- [ ] A hallucinated tool call and ALL its streamed argument deltas are NEVER emitted to the client
      (gate buffers-until-name, drops on reject) — asserted on all three inbound formats.
- [ ] Mixed valid+hallucinated calls in ONE batch ⇒ the whole batch is tainted: no server tool runs,
      no valid client tool is handed off; a synthetic `not_executed` tool result is supplied for the
      valid ones so the repair round can re-issue them.
- [ ] Repair round: a synthetic `tool` result (`tool_unavailable` for the unknown call) + extra
      upstream turn lets the model self-correct; bounded by `UNKNOWN_TOOL_REPAIR_CEILING`
      (default 1, max 2). Re-using the same loop as web-search (no new spawn, cancellation preserved).
- [ ] A model that ONLY ever emits the bad tool ⇒ ceiling reached ⇒ STRUCTURED terminal failure:
      Responses `response.failed`, Anthropic `error`, Chat SSE error (code e.g. `invalid_tool_call`) —
      NOT a raw mid-stream `?` abort. Already-streamed text is preserved (never retracted).
- [ ] No JSON parse of unknown-tool arguments (it was never executable); raw args capped for logging.
- [ ] Cancellation: every extra repair round selects on `tx.closed()` / the abort token, like the
      existing loop.
- [ ] All policy lives in the canonical Responses flow — NO bespoke per-inbound-format handling in the
      anthropic/chat adapters (AGENTS.md: canonical-Responses-only).
- [ ] Observability: a `tracing::warn!` on reject (response_id, provider, served_model, unknown tool
      name, offered-tool count, repair round) + a bounded counter `unknown_tool_call_total{provider,
      served_model,outcome}` (outcome ∈ repaired|exhausted; do NOT label raw tool names — cardinality)
      + monitor phases `unknown_tool_rejected` / `unknown_tool_repair_exhausted`.
- [ ] Prevention note (C): a closed-tool-set developer/system note is added to the upstream request.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Constraints / invariants (AGENTS.md — load-bearing)
- Canonical Responses is the single internal protocol — no direct non-canonical converter; the repair
  + terminal-failure logic lives in the engine/canonical layer, the three output converters just
  render the canonical terminal event in their format.
- `parallel_tool_calls = false` stays forced.
- Failover is pre-first-chunk ONLY — the repair round is an in-gateway SAME-provider extra turn, NOT a
  failover and NOT a token-duplicating retry of already-streamed content.
- Mixed provider-side(`web_search`)+client-side calls in one batch are still rejected — this spec adds
  the unknown-tool taint as a sibling rule, not a replacement.
- Reuse the EXISTING `ToolDeltaGate` byte caps; preserve its O(1) running-byte accounting (U7/T3).
- `MonitorHub::disabled()` zero-overhead: emit phases via `emit_with` only.

## Out of scope
- Changing the model-fallback behavior (`claude-opus-* → loaded vLLM model` is by design).
- Reducing hallucination at the model level beyond the prevention note (model/serving choice).
- Aliasing or auto-injecting any Claude Code tool (explicitly rejected — option B).
- Dashboard UI for the new counter/phases (the existing flow/topology surfaces pick them up; a
  dedicated "hallucinated tool" tile is a later `/ralph-guide-update` if wanted).

## Validation gate
- **Backend:** `cargo test` (add: unknown-tool→rejected not Err; gate hides unknown deltas on all 3
  formats; tainted-batch; repair-round self-correct; ceiling→structured terminal per format;
  text-not-retracted; cancellation during a repair round) · `cargo clippy --all-targets` · `cargo fmt`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review of the commit before this task is DONE.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED; committed.
