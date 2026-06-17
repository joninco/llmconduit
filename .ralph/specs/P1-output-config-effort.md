# P1 — Anthropic output_config.effort → reasoning.effort

**Priority:** quick-win (stub already exists) · **Surface:** request-translation · **GAPS.md:** P1

## Purpose
Map an Anthropic request's `output_config.effort` (when adaptive thinking is active) onto
the canonical `reasoning.effort`. Today llmconduit derives effort only from `thinking`, and
treats `output_config` purely as structured-output `format`. There is currently no effort
path through `output_config`.

## Reference (study, adapt — do NOT transliterate)
- claude-relay behavior source: `/home/jon/git/claude-relay/claude_relay/convert_request.py`
  (output_config → reasoning_effort mapping; adaptive-vs-enabled thinking gating).
- llmconduit target: `src/adapters/anthropic_to_responses.rs`
  (`convert_request`, `convert_output_config`, `convert_thinking`).

## Acceptance criteria (executable)
The failing stub already exists — make it pass:
- Un-ignore `anthropic_output_config_effort_maps_to_reasoning_effort` in
  `tests/port_translation.rs` (remove the `#[ignore = "GAP: …"]`).
- [ ] `output_config.effort = "high"` + `thinking.type = "adaptive"` → `reasoning.effort = Some("high")`.
- [ ] Same for `"low"` / `"medium"` / `"max"` (add cases).
- [ ] When `thinking.type` is NOT adaptive/active, `output_config.effort` is ignored (no effort injected).
- [ ] `output_config` WITHOUT `effort` is unaffected (existing `format` path stays correct).

## Constraints (load-bearing — see AGENTS.md)
- `output_config.format` json_schema → `text` controls MUST keep working; effort and format can coexist in one `output_config`.
- All currently-green `anthropic_output_config_*` tests in `tests/port_translation.rs` MUST stay green.
- Do not add `#[serde(deny_unknown_fields)]`; unknown keys must still flow to `extra_body`.

## Dependencies
None. Smallest gap — good loop-validation candidate.

## Definition of Done
- [ ] Stub un-ignored and green.
- [ ] `cargo test` whole suite green (no regressions).
- [ ] `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] Codex (xhigh) review of the diff passed, findings addressed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-fix-root-causes`, `principle-prove-it-works`.
