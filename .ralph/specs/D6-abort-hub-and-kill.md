# D6 — AbortHub + kill route

> **Source:** DASHBOARD_PLAN.md rev 8 §4.6 AbortHub, §5. Topic 13.

**Priority:** MEDIUM · **Surface:** `src/engine.rs` (cancel checks), `src/http.rs` (kill handler),
`src/lib.rs` (AbortHub on Gateway)

## Purpose
Let the dashboard cancel a stuck live stream server-side. Fixes Codex: AbortHub keyed by `api_call_id`
but kill routes use flow/response ids (mismatch); terminal cleanup absent. Decision: `:id` in
`/dashboard/api/flows/:id` IS `api_call_id`, so key+route align without rekeying. Replay is DEFERRED
(plan §2/§6) — kill is the only mutation surface for this phase.

## Jobs to Be Done
- `AbortHub { handles: Mutex<HashMap<String, CancellationToken>> }` on `Gateway`, keyed by
  `api_call_id` (= the flow `:id`).
- The D3 L1 `TelemetryGuard` registers its `CancellationToken` under `api_call_id` when it claims
  ownership (`OpenL0→ClaimedL1`).
- Engine checks `token.is_cancelled()` alongside existing `tx.is_closed()` checks (grep engine.rs); on
  cancel it surfaces `AppError::cancelled()` (HTTP 499) like a client hang-up — no token duplication
  (AGENTS.md "Failover only pre-first-chunk": mid-stream cancel is a cancel, not a retry).
- `POST /dashboard/api/flows/:id/kill` (`dashboard_flow_kill`) looks up by `api_call_id`, calls
  `cancel()`, returns 200. 404 if not active.
- **Cleanup:** the L1 guard REMOVES the AbortHub entry on finalize (Completed/Failed/drop-fallback) so
  finished flows don't leak entries; entries bounded by in-flight stream count, not the 512 history.

## Acceptance criteria
- [ ] `AbortHub` on `Gateway` keyed by `api_call_id`; `cancel(id)`/`register`/`remove`.
- [ ] D3 L1 guard registers on `ClaimedL1` and removes on finalize; a test asserts no entry leaks after
      a Completed flow.
- [ ] Kill cancels an active stream: `POST /dashboard/api/flows/:id/kill` → 200, the streaming request
      returns `AppError::cancelled()` (499) with no duplicated tokens to the client. 404 for an unknown
      / already-finished id.
- [ ] Engine `token.is_cancelled()` checked alongside ALL existing `tx.is_closed()` points (no point
      skipped — grep-verified); test for a cancel mid-chunk yields 499 cleanly.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D3 (L1 guard claims + owns lifecycle; registers/removes the AbortHub token).
- **Depended on BY (consumers, NOT deps of D6):** D7 applies the mutation+CSRF policy to the kill
  *route handler* (D7 depends on D6's `Gateway::abort`, NOT vice-versa); D13 *registers* the route
  after D6 ships. D6 itself is just the `AbortHub` + engine cancel checks + the `dashboard_flow_kill`
  handler logic — it compiles and tests against a mocked auth/CSRF gate, so it has NO dep on D7 or D13
  (this breaks the D6↔D7 and D6↔D13 cycles).
- **Extends:** `src/engine.rs` stream-loop cancel checks, `src/http.rs` kill handler logic,
  `Gateway::new` (additive AbortHub).
- **New APIs:** `AbortHub`, `Gateway::abort(id)`.
- **Constraint:** kill is the ONLY mutation route in this phase; replay is deferred (§6).

## Constraints
- Preserve every `tx.is_closed()` cancel point (AGENTS.md "Don'ts"); compose, don't replace.
- `AppError::cancelled()` (499) for cancel, matching existing client-hang-up semantics.
- Entries bounded by in-flight streams (not 512 history).

## Out of scope
- Replay (deferred — future phase with a bounded `0600` disk spool + redacting writer, plan §6).
- Frontend kill button wiring (D10/D12 — the SPA POSTs with the CSRF token from D7).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
