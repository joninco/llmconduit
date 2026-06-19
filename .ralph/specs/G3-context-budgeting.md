# G3 — Pre-flight context budgeting

> **Status: SHIPPED** — historical design input. Acceptance criteria below are satisfied by the implemented code/tests; see `.ralph/IMPLEMENTATION_PLAN.md` (Task for this gap) for the final design. Open questions/verify-first notes are resolved.

**Priority:** MED · **Surface:** server-http / engine · **GAPS.md:** G3

## Purpose
Proactively cap `max_completion_tokens` to `(context_limit − input_tokens − margin)` before calling
upstream, and return a clean error when `input_tokens >= context_limit`. Complements G1: G1 reacts to
overflow errors, G3 prevents most of them. claude-relay used a **fixed 128-token margin**.

## Reference (study, adapt — do NOT transliterate)
- claude-relay behavior source: `/home/jon/git/claude-relay/claude_relay/server.py`
  (`_completion_token_margin` = 128, `_cap_max_completion_tokens`, `ContextWindowError`).
- claude-relay tests: `tests/test_server.py::test_completion_token_margin_*`,
  `::test_cap_max_completion_tokens_*` (5 behaviors).
- llmconduit target: `src/engine.rs` (request build in `run_turn`).

## OPEN QUESTION — resolve FIRST (do not assume)
llmconduit may not currently track per-model context length. Before implementing, search the codebase:
- Does `UpstreamModelCatalog` / `/v1/models` expose a context-length field? Does any config carry it?
- If NO source of context length exists, the spec's scope is: (a) add a config/catalog field for
  context length, OR (b) implement the margin/cap logic gated on context length being known, and
  no-op when unknown. **Pick the minimal correct option and record the decision in IMPLEMENTATION_PLAN.md.**

## Acceptance criteria (executable)
Add tests to `tests/port_server.rs`:
- [ ] margin is a fixed 128-token reserve (model-independent).
- [ ] `max_completion_tokens` lower than available is left unchanged.
- [ ] `max_completion_tokens` above available is reduced to `(context − input − 128)`.
- [ ] `input >= context` → a clean 4xx (`AppError::bad_request`-style), not a panic.

## Constraints (load-bearing — see AGENTS.md)
- Explicit request fields still win; do not silently override a caller value except to *cap* it down.
- Don't break streaming or the `stream_options.include_usage` path.
- Token counting: prefer the upstream-reported `usage`/catalog over guessing; if estimating input
  tokens, document the heuristic and keep it conservative.

## Dependencies
**G1** (implement first — shared context-length notion).

## Definition of Done
- [ ] Tests green · `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] OPEN QUESTION resolution recorded in `IMPLEMENTATION_PLAN.md`.
- [ ] Codex (xhigh) review passed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-foundational-thinking` (resolve context-length source first), `principle-fix-root-causes`, `principle-prove-it-works`.
