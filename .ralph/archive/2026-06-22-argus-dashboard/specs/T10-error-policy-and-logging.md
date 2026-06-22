# T10 — AppError failover policy + upstream retry logging

> **Source:** thermo-nuclear review (G1 MEDIUM #14, #15). See `/tmp/thermo-synthesis.md`.

**Priority:** MEDIUM · **Surface:** src/error.rs + src/upstream.rs · **Thermo findings:** G1 MEDIUM (failover_eligible policy leak), G1 MEDIUM (retry POST not logged)

## Purpose
`failover_eligible` is a global public boolean on `AppError` for ONE upstream orchestration case
(`error.rs:24`), leaking failover policy into every error constructor. Separately, upstream request
logging records only the first POST (`upstream.rs:524`); the G1 shrink-and-retry POST (with the
reduced budget) is invisible to the JSONL log + `analyze-log`.

## Jobs to Be Done
- Generic app errors stay policy-free; failover eligibility lives where failover is decided.
- Every actual upstream chat request (first + retry) is recorded in the JSONL request log.

## Acceptance criteria
- [ ] `failover_eligible` is removed from `AppError`; replaced with an upstream-attempt
      disposition/wrapper or an upstream-specific error variant so generic app errors carry no
      failover policy.
- [ ] The shrink-and-retry POST is logged (either by moving logging into the send helper or by adding
      a logged-send helper); `analyze-log` sees both the original + the reduced-budget request.
- [ ] Failover behavior unchanged (same errors still eligible/ineligible); G1 retry tests stay green.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/error.rs`, `src/upstream.rs` (send path + G1 retry), `analyze-log`.
- **New APIs:** possibly an `UpstreamAttemptDisposition` or upstream-specific `AppError` variant.

## Constraints
- Do not regress failover-pre-first-chunk semantics.
- Retry logging must redact/truncate consistently with the existing request logger.

## Out of scope
- G1 `estimated_input_tokens` (already fixed in `07117b2`).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
