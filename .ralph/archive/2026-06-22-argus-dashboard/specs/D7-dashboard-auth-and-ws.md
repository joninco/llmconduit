# D7 — Dashboard auth + CSP + batched WS envelope (+ protect /debug)

> **Source:** DASHBOARD_PLAN.md rev 8 §6, §5 WebSocket. Topic 13.

**Priority:** HIGH (security gate — any route exposing bodies/headers/kill must be authed) · **Surface:**
new `src/dashboard_auth.rs`, `src/dashboard_ws.rs`, `src/http.rs` (route gating), `src/main.rs`
(startup refusal), `src/monitor.rs`/`src/debug_ui.rs` (envelope + /debug protection), `src/config.rs`
(env reads — NOT a persisted Config field)

## Purpose
Make the dashboard (and the existing `/debug`) access-controlled end-to-end, with a browser-WS-capable
session-cookie flow, and define the dashboard WS wire envelope. Fixes Codex blockers across rounds:
`--with-debug-ui` is route-registration only, not auth; browser `WebSocket` can't set `Authorization`
(needs a cookie login flow); `/debug` + `/debug/ws` also expose transcripts unauthenticated; an
`include_dir!`/`Path=/dashboard` cookie blocks `/debug`; `debug.html` inline script vs strict CSP;
non-loopback HTTP-only server serving creds; `DebugUpdate` carries a `Vec<DebugWsMessage>` under ONE
`sequence` (monitor.rs:87) so per-frame envelope dedup drops siblings → use a BATCHED envelope.

## Jobs to Be Done
**Secrets (env-only, never a `Debug+Clone` config struct):**
- `LLMCONDUIT_DASHBOARD_TOKEN` (constant-time compare via `subtle::ConstantTimeEq`). On non-loopback,
  REQUIRED (else refuse to register `/dashboard` AND `/debug` at startup). Loopback without token
  → allowed with a logged warning (dev).
- `LLMCONDUIT_DASHBOARD_SESSION_KEY` (≥32 bytes b64, HMAC-SHA256 cookie signing). Auto-generate +
  log temporary on loopback-dev; required on non-loopback; never logged.
- `LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN` (exact public origin, MUST be `https://`). On non-loopback a
  validated `https://` origin is REQUIRED (rev4 fix: formerly only warned). Override only with
  `LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1` (loud warning; air-gapped LAN).

**Login + session cookie:**
- `POST /dashboard/login` (`{token}`): constant-time compare; on success set `HttpOnly; SameSite=Strict;
  Secure`-when-`PUBLIC_ORIGIN`; **`Path=/`**; `Max-Age=3600`; value =
  `base64url(HMAC-SHA256(key,"{exp}:{nonce}")) + "." + "{exp}:{nonce}"` (signed, stateless). Response
  `no-store`. `/dashboard` SPA shell renders a **login shell** (token-entry form) to unauthenticated
  clients, the dashboard once authed.
- `POST /dashboard/logout`: clear cookie. Stateless caveat: a copied cookie valid until `exp` (≤1 h) —
  documented; revocable sessions are future work.

**WS auth (the browser gap):** `/dashboard/ws` + `/debug/ws` validate (1) signed session cookie, (2)
`Origin` against the allow-list (served origin, or exact `PUBLIC_ORIGIN`). Each WS connection records the
cookie `exp`; a per-connection tokio timer closes the socket at `exp` (no WS outliving the cookie).

**Mutation gating + CSRF:** `POST /dashboard/api/flows/:id/kill` (D6) requires BOTH
`LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS=1` (default off → 403) AND a double-submit CSRF token
(non-HttpOnly cookie + SPA bootstrap echo) in `X-CSRF-Token`, constant-time compared. `GET` reads need
only the session cookie. Replay is NOT registered.

**CSP + headers:** `/dashboard` + static: `default-src 'self'; script-src 'self'; connect-src 'self'
ws: wss:; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; base-uri 'self';
frame-ancestors 'none'`. `/debug` CSP: the existing `src/debug.html` is one file with an inline module
script — externalize to `/debug/app.js` (recommended) OR a `/debug`-specific
`script-src 'sha256-<hash>'`; `/debug/ws` contract stays bare `DebugWsMessage` (unchanged). Plus
`nosniff`/`no-referrer`/`X-Frame-Options: DENY`/`Cache-Control: no-store` on API + `/debug`.

**Batched WS envelope (dashboard only; /debug/ws untouched):**
```rust
DashboardFrame { domain: Domain, seq: u64, batch: Vec<DashboardPayload> }
enum Domain { Flow, Metrics, Topology, Monitor }
enum DashboardPayload {
    Monitor(DebugWsMessage),      // one per message in the originating DebugUpdate batch
    Usage { response_id, prompt, completion, total, cached, reasoning },
    MetricTick { /* /api/metrics shape */ },
    FlowStatus { response_id, status, served_model, upstream_target, usage, elapsed_ms },
    TopologyUpdate { /* ProviderHealth snapshot */ },
}
```
Dedup **per-domain whole-frame**: `{domain} => seq <= last_seq[domain]` drops the whole `batch`,
processes if `> last_seq`. The `Monitor` frame's `seq` IS the originating `DebugUpdate.sequence` (one
envelope per `DebugUpdate`, `batch` = its messages) → no sibling frame dropped.

## Acceptance criteria
- [ ] Env-only secrets; non-loopback without `LLMCONDUIT_DASHBOARD_TOKEN` + validated `https://`
      `PUBLIC_ORIGIN` → startup REFUSES to register `/dashboard` + `/debug` (test via `main.rs` path);
      loopback-dev concession logs a warning and still serves.
- [ ] `/dashboard/login` constant-time compare; sets the signed `Path=/` HttpOnly SameSite=Strict
      (Secure when PUBLIC_ORIGIN) cookie; `/dashboard` serves a login shell when unauthed.
- [ ] `/dashboard/api/*` + `/dashboard/ws` + `/debug` + `/debug/ws` all require the signed session
      cookie (or bearer fallback for non-browser, constant-time); invalid/expired → 401 `no-store`.
- [ ] WS: valid cookie + Origin pass; bad/expired cookie → reject; cross-origin → reject; a connection
      whose cookie `exp` passes is closed (test with mock clock).
- [ ] Kill requires `ALLOW_MUTATIONS=1` + CSRF `X-CSRF-Token` (constant-time); off → 403.
- [ ] `/debug` CSP blocks its inline script UNLESS externalized (option a) or sha256-hashed (option b);
      `/dashboard` CSP as spec; security headers present; `no-store` on API + `/debug`.
- [ ] Batched `DashboardFrame`: one envelope per `DebugUpdate` (seq=`DebugUpdate.sequence`, batch=its
      messages); per-domain whole-frame dedup; a test streams a `DebugUpdate` with multiple sibling
      `DebugWsMessage`s and asserts ALL arrive (none dropped by dedup). `/debug/ws` still bare.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points (two stages — breaks the former D6↔D7 cycle)
- **Stage D7a — auth+login+CSP foundation (parallel-start in Phase A):** env-only secrets, login/
  logout handlers, HMAC-SHA256 cookie, `/dashboard` login-shell serving, `/debug`+`/debug/ws` auth +
  CSP + inline-script fix. Depends on: D1 (routes exist) **only**. This stage is mock-parallel with
  D1/D8/D9 (it can gate stub routes).
- **Stage D7b — WS envelope + frame wiring (after the data seams):** the batched `DashboardFrame`
  envelope, `Usage`/`FlowStatus`/`MetricTick`/`TopologyUpdate` payload arms, per-domain dedup, WS
  cookie+Origin+expiry. Depends on: D3 (Usage/FlowStatus), D4 (TopologyUpdate), D5 (MetricTick).
- **D6 relationship (NO cycle):** D7 **depends on** D6's `Gateway::abort` (it applies the
  `LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS=1` + CSRF policy to the kill route handler), AND D6 does NOT
  depend on D7 (D6's `dashboard_flow_kill` logic is gated by a pluggable `MutationPolicy` trait D7
  provides, so D6 compiles/tests against a mock policy). One-way edge D7→D6 only — no cycle.
- **Extends:** `src/http.rs` gating, `src/main.rs` startup refusal, new `src/dashboard_auth.rs` +
  `src/dashboard_ws.rs`, `src/monitor.rs` (envelope), `src/debug_ui.rs` (/debug protection/CSP).
- **New APIs:** login/logout handlers, `DashboardFrame`/`DashboardPayload`/`Domain`, WS envelope
  builder, CSRF issuance+verify, `MutationPolicy` trait (D6 + D13 consume).
- **Note:** secrets are env-only — do NOT add them to the persisted `Config` struct (avoid `Debug+Clone`
  secret exposure).

## Constraints
- `/debug/ws` wire contract UNCHANGED (existing debug client keeps working) — only its auth gate +
  headers change.
- Stateless HMAC-SHA256 cookie; rotation invalidates all sessions (documented).
- No external CDNs; all assets embedded (D8).
- `--with-debug-ui` off → none of these routes/flows register (production untouched).

## Out of scope
- The REST route handlers themselves (D13) — D7 owns ONLY auth/CSP/envelope/wiring the gate.
- Frontend auth UX (D9 renders the login shell + CSRF echo).
- Replay route (deferred).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED (security-sensitive — extra scrutiny on
      constant-time, Origin, cookie attrs, expiry).
