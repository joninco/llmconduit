# U5 — Test coverage for WEB_SEARCH_ROUNDS_HARD_CEILING=25

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** MEDIUM · **Surface:** `tests/gateway.rs` (rule in `src/engine.rs:1772-1781`) · **Thermo finding:** load-bearing AGENTS.md hard rule `WEB_SEARCH_ROUNDS_HARD_CEILING=25` and its `.min(25)` config cap have ZERO test coverage; the sibling `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING=8` IS tested — asymmetric blind spot.

## Purpose
`WEB_SEARCH_ROUNDS_HARD_CEILING` is an AGENTS.md hard rule that guarantees a server-tool loop terminates: a model that re-requests `web_search` every round must error out instead of hanging the turn forever. The production logic at `src/engine.rs:1772-1781` computes `effective_limit = configured_limit.min(WEB_SEARCH_ROUNDS_HARD_CEILING)` and returns `AppError::upstream("web search round limit exceeded")` once `web_search_rounds >= effective_limit` (the loop continues via `continue` at `src/engine.rs:1797`, re-detecting `had_web_search` from the emitted tool call at `src/engine.rs:1751-1755`). NONE of this is exercised: every `Config` literal in the suite sets `max_web_search_rounds: 5` (e.g. `tests/gateway.rs:3034`), no test queues more forced web_search rounds than the limit, and no test sets `max_web_search_rounds > 25` to prove the `.min(25)` cap overrides a higher configured value. The sibling ceiling is covered by `image_agent_round_ceiling_terminates_loop` (`tests/image_agent.rs:844-866`); web_search must get the equivalent guard so a future refactor that weakens or removes the ceiling fails CI. This is a TEST-ONLY task: no production code changes.

## Jobs to Be Done
- Add a `tests/gateway.rs` test that drives a forced `web_search` loop past the limit and asserts the turn terminates with `response.failed` ("web search round limit exceeded"), mirroring `image_agent_round_ceiling_terminates_loop`.
- Add a SECOND test that sets `max_web_search_rounds > 25` and asserts termination still occurs at round 25, proving the `.min(WEB_SEARCH_ROUNDS_HARD_CEILING)` cap overrides the higher configured limit.
- Assert bounded upstream/search call counts so the loop is proven finite (not merely "eventually fails").
- Leave `WEB_SEARCH_ROUNDS_HARD_CEILING`, the `.min(25)` cap, and all surrounding loop logic byte-identical — DO NOT lower the ceiling or touch production code.

## Acceptance criteria
- [ ] New test (e.g. `web_search_round_ceiling_terminates_loop`) in `tests/gateway.rs` queues a forced `web_search` `tool_call_chunk` for every round (count strictly greater than the effective limit, e.g. via a loop pushing `>limit` responses) using `MockSearch::default()` for canned results, and asserts `event_names(&events)` contains `"response.failed"` (web_search ceiling terminates the loop).
- [ ] The same test asserts a BOUNDED call count proving finiteness: `upstream.requests().await.len()` is exactly the default config's effective limit (`max_web_search_rounds: 5` → 5 upstream rounds) and the search client ran the matching bounded number of times — no unbounded loop.
- [ ] A SECOND test (e.g. `web_search_round_ceiling_caps_configured_limit`) builds a `Config` (via `test_gateway_with_config`) with `max_web_search_rounds` set ABOVE 25 (e.g. `100`), queues `> 25` forced `web_search` rounds, and asserts the turn fails with the ceiling AND `upstream.requests().await.len() == 25` (termination by round 25 via `.min(WEB_SEARCH_ROUNDS_HARD_CEILING)`, NOT round 100).
- [ ] Both tests keep `brave_api_key: Some(..)` set (web_search is only server-runnable when Brave is configured — `src/engine.rs:1750`), so the forced calls actually enter the round-counting branch.
- [ ] Both new tests MUST set `request.tools = vec![ToolSpec::WebSearch { external_web_access: Some(true), filters: None, user_location: None, search_context_size: None, search_content_types: None }]` and force `tool_choice` to the web_search function (as in `web_search_continuation_round_relaxes_forced_tool_choice`, `tests/gateway.rs:1320-1328`) so the emitted web_search call classifies as `ToolKind::WebSearch` (name→kind is registry-driven at `src/responses_to_chat.rs:632`) and ENTERS the round-counting branch (`src/engine.rs:1751-1755`). Without the declared tool the call is treated as a client Function, `had_web_search` stays false, and the test silently exercises a non-ceiling path. NOTE this differs from `image_agent_round_ceiling_terminates_loop`, which activates the server tool via `image_agent_config` + a vision session, NOT via `request.tools`.
- [ ] No production file is modified (`git diff --name-only` touches only `tests/`); `WEB_SEARCH_ROUNDS_HARD_CEILING` literal `25` at `src/engine.rs:1772` and the `.min(...)` cap at `src/engine.rs:1778` are unchanged.
- [ ] Tests added next to / consistent with `web_search_continuation_round_relaxes_forced_tool_choice` (`tests/gateway.rs:1297`) and styled after `image_agent_round_ceiling_terminates_loop` (`tests/image_agent.rs:844`).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED

## Integration points
- **Extends:** `tests/gateway.rs` — reuses `MockUpstream`, `MockSearch`, `test_gateway`, `test_gateway_with_config`, `test_config`, `tool_call_chunk`, `base_request`, `collect_stream`, `event_names`, `user_message`, `ToolSpec::WebSearch`.
- **New APIs:** none. Test-only additions; no production surface.
- **Depends on:** the existing `MockUpstream::requests()` accessor (`tests/gateway.rs:88`) and `MockSearch` canned-result behavior (`tests/gateway.rs:228-243`) for bounded-count assertions. Mirrors the proven pattern from `tests/image_agent.rs:844-866`.

## Constraints
- AGENTS.md HARD RULE: `WEB_SEARCH_ROUNDS_HARD_CEILING` MUST remain `25`. Do not lower it, do not parameterize it, do not weaken the `.min(25)` cap.
- The forced loop must be reproduced by queuing canned upstream chunks that EACH emit a `web_search` tool call (the loop re-detects `had_web_search` from the emitted call at `src/engine.rs:1751-1755`, independent of `tool_choice` relaxing to `auto` at `src/engine.rs:1796`) — exactly as the image test queues `analyzeImage` every round.
- `max_web_search_rounds == 0` ("unlimited") semantics and the independent `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING=8` branch are out of scope and must be left untouched.
- No change to wire output, redaction, failover, or routing behavior; this task adds coverage only.

## Out of scope
- Changing, lowering, or making configurable the `WEB_SEARCH_ROUNDS_HARD_CEILING` value.
- Any production code change in `src/engine.rs` or elsewhere.
- New coverage for the `max_web_search_rounds == 0` unlimited path or for `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING` (already tested at `tests/image_agent.rs:844`).
- Refactoring shared test helpers beyond what these two tests strictly need.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
