# T2 — Typed routing-candidate plan (delete G4 side-channel vision gating)

> **Source:** thermo-nuclear review (G4 HIGH #6). See `/tmp/thermo-synthesis.md`.

**Priority:** HIGH · **Surface:** engine + routing · **Thermo findings:** G4 HIGH (vision gating duplicates routing)

## Purpose
G4's native-vision gating (`engine.rs:759`, `request_model_genuinely_resolves`) is a second
model-routing implementation that must mirror `normalize_upstream_model`,
`RoutingModelCatalog::resolve`, routes, aliases, and failover semantics. The "must mirror" comments
are the smell; drift already caused bugs elsewhere (G7). Replace the side-channel resolution with a
typed backend-candidate plan produced by the real routing/failover layer, and reuse it for G4
gating.

## Jobs to Be Done
- Vision gating asks the routing layer "does this request resolve to a vision-capable backend?"
  instead of re-deriving resolution.

## Acceptance criteria
- [ ] `request_model_genuinely_resolves` and the side-channel resolution logic in `engine.rs` are deleted.
- [ ] The real routing/failover layer exposes a typed backend-candidate plan (or a query on it) that
      G4 vision gating consumes.
- [ ] Tests: routed/failover/aliased models gate vision exactly as routing resolves them — no
      separate truth to keep in sync.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** T1 (leaf-profile-resolution) — the typed resolver/API extracted there is the
  foundation; sequence after T1.
- **Extends:** G4 vision gating in `engine.rs`, `RoutingUpstreamClient`.
- **New APIs:** a backend-candidate plan type (or `resolve_backend_candidate(request_model)`).

## Constraints
- Do not regress G4's 47-behavior image-agent suite.
- Gating stays PROFILE-ONLY with no further `upstream_model` remap (AGENTS.md G4 note).

## Out of scope
- ToolDeltaGate extraction (T3). Vision module split (T4).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
