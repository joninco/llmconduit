# G2 — Backend model-family reshaping (Kimi / DeepSeek)

> **Status: SHIPPED** — historical design input. Acceptance criteria below are satisfied by the implemented code/tests; see `.ralph/IMPLEMENTATION_PLAN.md` (Task for this gap) for the final design. Open questions/verify-first notes are resolved.

**Priority:** MED · **Surface:** backend-routing · **GAPS.md:** G2

## Purpose
Detect the resolved backend model family and inject family-specific `chat_template_kwargs`:
Kimi → always `thinking:true` (+ `preserve_thinking`) to stop reasoning leakage via the identity
parser, even when the client is inactive; DeepSeek → `enable_thinking:true` (+ reasoning_effort).
llmconduit already has PARTIAL Kimi handling (sentinel cleanup + nested-thinking parsing in
`chat_to_responses.rs`) but no automatic family detection / kwargs injection.

## Reference (study, adapt — do NOT transliterate)
- claude-relay proven impl: `/home/jon/git/claude-relay/claude_relay/backend.py`
  (family detection, kimi/deepseek kwargs, `template_family` override, resolved-model-wins, reshape).
- claude-relay tests: `tests/test_backend.py::test_kimi_*`, `::test_deepseek_*`, `::test_template_family_*`,
  `::test_resolved_*` (~12 behaviors).
- llmconduit target: `src/engine.rs` (where upstream `chat_template_kwargs` are assembled),
  `src/config.rs` (if a `template_family`-style override is added), and reuse of existing Kimi logic in
  `src/adapters/chat_to_responses.rs`.

## Acceptance criteria (executable)
Two failing stubs already exist in `tests/port_routing.rs` — un-ignore and make green:
- [ ] `kimi_backend_forces_thinking_kwargs` — a Kimi-family backend sends `chat_template_kwargs.thinking = true`.
- [ ] `deepseek_backend_injects_enable_thinking` — a DeepSeek-family backend sends `enable_thinking = true`.
Then extend `tests/port_routing.rs`:
- [ ] family detected from the **resolved** model id (case-insensitive), not stale config.
- [ ] Kimi sends `thinking:true` even when the client request is "inactive" (no reasoning requested).
- [ ] (if implementing the override) a `template_family` setting forces the family regardless of model name.
- [ ] nested assistant `thinking{}` is reshaped to flat `reasoning_content` (compose with existing sentinel cleanup, don't duplicate it).

## Constraints (load-bearing — see AGENTS.md)
- **Compose with existing behavior**: there is already Kimi/vLLM sentinel cleanup + nested-thinking parsing — extend, don't fork (`principle-subtract-before-you-add`).
- **Explicit request fields still win** over injected family defaults, and family kwargs deep-merge with configured `upstream_chat_kwargs`/`model_profiles` rather than clobbering them.
- Family injection must not leak server-side reasoning into Chat-Completions output (Chat hides `web_search_call`; same discipline for thinking).
- Keep `parallel_tool_calls=false`.

## Dependencies
None hard, but touches `engine.rs` request-build like G1/G3 — run **serially** (no parallel edits to `engine.rs`).

## Definition of Done
- [ ] Both stubs un-ignored + green, plus the extended cases.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] Codex (xhigh) review passed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-subtract-before-you-add`, `principle-encode-lessons-in-structure`, `principle-fix-root-causes`.
