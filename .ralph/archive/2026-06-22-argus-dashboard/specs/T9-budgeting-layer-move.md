# T9 — Move G3 budgeting behind route/provider resolution + single request builder

> **Source:** thermo-nuclear review (G3 HIGH #18, G3 MEDIUM #19 ×2). See `/tmp/thermo-synthesis.md`.

**Priority:** HIGH · **Surface:** engine + upstream · **Thermo findings:** G3 HIGH (budgeting in wrong layer), G3 MEDIUM (shadow request construction), G3 MEDIUM (self-referential test oracle)

## Purpose
G3 budgets against `resolved_model` BEFORE provider routing/failover rewrites (`engine.rs:596`), so
backend-specific context policy lives in the wrong layer: route-only/exposed-fallback models no-op,
and failover can use a different window than the cap/reject decision. Separately,
`estimate_request_from_lowered` hand-builds a shadow `ChatCompletionRequest` (`engine.rs:199`) while
`run_turn` builds the real one (`engine.rs:1165`) — drift-prone, kept correct by comments + selective
omissions. And the test oracle reuses the production estimator (`tests/port_server.rs:63`), so
estimator-vs-wire drift is self-confirming.

## Jobs to Be Done
- Budgeting runs against the SAME model the upstream will actually send to (post route/provider
  resolution).
- The estimate and the dispatch share ONE first-upstream-request builder.

## Acceptance criteria
- [ ] G3 budgeting moves into upstream dispatch AFTER route/provider resolution, OR a resolver
      exposes the exact pre-first-chunk candidate context set and budgeting uses a conservative min
      with unknown=no-op.
- [ ] One first-upstream-request builder is used for BOTH budgeting and dispatch (delete
      `estimate_request_from_lowered`'s shadow construction, or compute the estimate from the actual
      sanitized first request).
- [ ] The test oracle computes expected budgets from an INDEPENDENTLY normalized recorded request
      (not by calling the production estimator); add routing/upstream_model/failover context cases
      where the current layer placement is most fragile.
- [ ] No false 400 on known-good requests; G3 never synthesizes a cap / never raises (reactive G1
      stays the net).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** T1 (leaf-profile-resolution) — the typed resolver/API from T1 is the foundation
  for "which model will actually be used." Sequence after T1.
- **Extends:** `engine.rs` (budgeting + `estimate_request_from_lowered` / `estimate_input_tokens`),
  `upstream.rs` (dispatch), `tests/port_server.rs`.
- **New APIs:** possibly a `first_upstream_request_builder` / candidate-context resolver.

## Constraints
- Keep G3 OUT of the kwargs-merge seam (existing invariant — estimate omits ADDITIVE leaf merges +
  `reasoning_effort` so it stays a safe lower bound).
- Count the bytes the LEAF POSTs (post-`sanitize_chat_request`); estimating earlier representations
  is whack-a-mole (AGENTS.md / plan Discovery).
- Context-length source stays the `/v1/models` snapshot in `UpstreamModelCatalog`.

## Out of scope
- G1 reactive retry (already fixed in `07117b2` + T10).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
