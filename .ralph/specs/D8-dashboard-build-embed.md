# D8 — build.rs stub + include_dir! embedding + node-less-host safety + dashboard_ui.rs serving

> **Source:** DASHBOARD_PLAN.md rev 8 §3 build integration. Topic 13.

**Priority:** HIGH (gates the frontend; must be sound before D9) · **Surface:** `build.rs`,
`Cargo.toml`, new `src/dashboard_ui.rs`, `src/http.rs` (static asset routes)

## Purpose
Embed the React+TS+Vite SPA into the single Rust binary WITHOUT forcing node on every build host, and
fix the Codex `include_dir!` compile-error blocker (a bare `include_dir!(concat!(env!(...)))` does not
compile: the macro takes one string literal, and the result needs a `static` binding of type
`Dir<'static>`).

## Jobs to Be Done
- `build.rs` **always** ensures `$OUT_DIR/dashboard_dist/` exists with a minimal stub `index.html`
  ("dashboard not built; build with LLMCONDUIT_BUILD_DASHBOARD=1"). Emit
  `cargo:rerun-if-env-changed=LLMCONDUIT_BUILD_DASHBOARD` + `cargo:rerun-if-changed` for
  `dashboard-frontend/src`, `package.json`, lockfile, `vite.config.ts`, `index.html`.
- When `LLMCONDUIT_BUILD_DASHBOARD=1`: **clear** `$OUT_DIR/dashboard_dist/` (no stale assets from a
  prior enabled build linger), shell to `npm ci && npm run build` with Vite `outDir` =
  `$OUT_DIR/dashboard_dist/`; fail the build loudly if npm is missing. When OFF: clear/rewrite the stub.
- `src/dashboard_ui.rs`:
  ```rust
  use include_dir::{include_dir, Dir};
  static DASHBOARD_DIST: Dir<'static> = include_dir!("$OUT_DIR/dashboard_dist");
  ```
  (the `static Dir` binding is required — rev3/rev4 fix). Serve `/dashboard` (SPA shell) +
  `/dashboard/assets/*` from `DASHBOARD_DIST`.
- Add the `include_dir` crate to `Cargo.toml`.
- Route registration is gated by `--with-debug-ui` (http.rs:75 block) — D8 adds the static-asset routes
  + the `/dashboard` index; auth (D7) wraps them.

## Acceptance criteria
- [ ] `cargo build` (no `LLMCONDUIT_BUILD_DASHBOARD`) succeeds on a node-less host; the stub
      `$OUT_DIR/dashboard_dist` is always present; `include_dir!("$OUT_DIR/dashboard_dist")` compiles
      (the `static Dir<'static>` binding is correct).
- [ ] `LLMCONDUIT_BUILD_DASHBOARD=1 cargo build` runs `npm ci && npm run build` → embeds real dist;
      if npm absent → build fails loudly with a clear message.
- [ ] A prior enabled build's assets do NOT linger when rebuilding without the flag (build.rs clears
      `$OUT_DIR/dashboard_dist`).
- [ ] `GET /dashboard` serves the embedded shell; `/dashboard/assets/*` serves static assets; all gated
      behind `--with-debug-ui` (off → not registered).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** nothing (build infra — parallelizable with D1-D7 Rust seams). Unblocks D9 (frontend
  scaffold produces `dist/`).
- **Extends:** `build.rs` (currently git-provenance only), `Cargo.toml` (`include_dir`), new
  `src/dashboard_ui.rs`, `src/http.rs` static routes.
- **New APIs:** `DASHBOARD_DIST`, asset-serving handlers.
- **CI/release contract:** CI sets `LLMCONDUIT_BUILD_DASHBOARD=1`; local Rust-only devs do not need node.

## Constraints
- Single-binary purity preserved for Rust-only builds (no node required unless the operator opts into
  the embedded SPA).
- `$OUT_DIR/dashboard_dist` guaranteed at compile time so `include_dir!` is always sound.
- Reuse the existing `--with-debug-ui` gate; do NOT add a new runtime flag for embedding (the env var
  is build-time only).

## Out of scope
- The SPA itself (D9-D12); D8 embeds whatever `dist/` exists.
- Auth/CSP on the routes (D7).
- Production of a real `dist/` in this task (D9 scaffolds the frontend that builds it).

## Definition of done
- [ ] Acceptance criteria green; `cargo build` succeeds node-less; Codex-xhigh APPROVED.
