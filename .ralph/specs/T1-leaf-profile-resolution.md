# T1 — Leaf-side profile resolution (template_family + upstream_chat_kwargs)

> **Source:** thermo-nuclear code-quality review (per-gap G2/G7 BLOCKERs + G2 MEDIUM + G7 HIGH).
> See `/tmp/thermo-synthesis.md` findings #1, #2, #16, #10. Supersedes the shipped G2/G7
> engine-side resolution; final design of record once implemented.

**Priority:** HIGH · **Surface:** engine ↔ upstream leaf · **Thermo findings:** G2 BLOCKER, G7 BLOCKER, G2 MEDIUM, G7 HIGH

## Purpose
`reasoning_effort` was already moved to the upstream leaf (`finalize_request_for_backend`), but
two other profile-derived config knobs — `template_family` and `upstream_chat_kwargs` — are still
resolved in the engine against the *pre-routing* `upstream_model`. Routing/failover rewrite
`request.model` *after* this resolution (`upstream.rs:707`, `upstream.rs:1001`), so a route or
failover target whose family/profile differs from the engine-resolved one receives stale kwargs:
a cross-family fallback (e.g. a `glm-4.6` alias routed to an opaque `deepseek-v3` target) can get
Kimi `chat_template_kwargs` forced onto DeepSeek. The same root cause makes the model-precedence
ladder (`normalize_upstream_model` / `matches_model_route` / `RoutingModelCatalog::resolve`) triply
duplicated. Fix all three by moving profile resolution to the leaf and collapsing the precedence
truth.

## Jobs to Be Done
- A routed/failover model gets ITS OWN `template_family` + `upstream_chat_kwargs`, not the request
  alias's.
- Model precedence lives in ONE resolver; the engine + config + routing client all ask it.

## Acceptance criteria
- [ ] `template_family` is resolved inside `finalize_request_for_backend` from the FINAL
      `request.model` (post route/failover rewrite), via a per-model policy map mirroring
      `reasoning_effort_policies` (build once from config, attach to `ReqwestUpstreamClient`).
- [ ] `upstream_chat_kwargs` profile merge likewise moves to the leaf (or to the routing client's
      `request_for_provider`/`routed_request` where the final model is known) — the engine stops
      pre-merging profile kwargs.
- [ ] `ChatCompletionRequest` no longer carries `#[serde(skip)]` side-channel fields
      (`template_family`, `client_chat_template_kwargs`); finalization metadata lives on an internal
      `BackendChatRequest` wrapper (or equivalent) that wraps the wire request at the leaf boundary.
- [ ] Model precedence is extracted to a single typed resolver/API; `Gateway::normalize_upstream_model`
      and `Config::matches_model_route` are deleted or reduced to thin callers of it (the "must mirror"
      comments disappear).
- [ ] Tests: route-to-cross-family-model + failover-to-cross-family-model assert the FINAL model's
      family/kwargs are applied, not the alias's; opaque fallback model ids + cross-family cases covered.
- [ ] `claude --effort high/max/off` against GLM on :5022 still correct (live-verify — touches leaf).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** existing `reasoning_effort_policies` leaf pattern (mirror it).
- **Extends:** `finalize_request_for_backend`, `RoutingUpstreamClient::request_for_provider` /
  `routed_request`, `Config` profile accessors.
- **New APIs:** `template_family_policies()` / `upstream_chat_kwargs_policies()` (or a combined
  per-model finalization policy), `BackendChatRequest`.

## Constraints (load-bearing — see AGENTS.md)
- **TOUCHES THE EFFORT LEAF.** After implementation, rebuild, restart `llmconduit.service`, and
  live-verify `claude --effort high/max/off` → GLM `chat_template_kwargs` on :5022.
- Client-explicit `chat_template_kwargs` still wins over a forced family default (precedence:
  config < family < effort-map < client — preserve it at the leaf).
- Keep `parallel_tool_calls=false`; do not regress streaming cancellation / failover-pre-first-chunk.
- The `BackendChatRequest` wrapper must not leak through any public API or serde boundary.

## Out of scope
- Splitting `upstream.rs` into modules (separate task T6).
- G4's `request_model_genuinely_resolves` deletion (T2 — but T2 consumes this task's resolver).
- G3 budgeting layer move (T9 — interacts; sequence T1 before T9 so the resolver exists).

## Definition of done
- [ ] All acceptance criteria green; leaf live-verified on :5022.
- [ ] `cargo test` + `clippy --all-targets` + `fmt` clean; Codex-xhigh APPROVED.
- [ ] `IMPLEMENTATION_PLAN.md` updated with the final design + commit.
