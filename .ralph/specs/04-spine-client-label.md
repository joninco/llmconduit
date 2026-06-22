# 04 — Spine: `client_label` / key-hash 🔭⚙️⭐

> Ralph gap spec — implementation-free. FEATURES.md **item 2 (data-contract pass / the spine)**.
> Backend. Sequence: spine seam — before spec 15 (client-attribution UI).

## Operator question
"Who is generating the cost, errors, latency — or abuse?"

## Current state (verified by code search)
- `src/http.rs:386` — `user_agent` logged to **trace only**, not captured into the flow.
- `src/http.rs:391-392` — `authorization_present` / `x_api_key_present` logged as **booleans**; raw key value redacted to `[redacted]` in captured headers (`dashboard_flow.rs:1289`).
- The **raw** auth header is present at the middleware **before** redaction → a key **hash** is derivable there.
- `log_api_call` `http.rs:343-459` → `store.open()` `:449-455` takes `(api_call_id, method, uri, headers_redacted, inbound_body)` — **no identity param.** `FlowRecord` has no `client_label`/`user_agent`/key field. No authenticated-principal concept on the proxy path.

## Scope — what to build
- Derive a stable `client_label` (+ its `source`) in `log_api_call` **before** redaction; emit on `FlowRecord` + the `/flows` summary.
- Priority — phase-2 sources that EXIST (there is **no** proxy auth-principal today, so don't list one): **API-key HASH** (non-reversible digest of the inbound key; raw key never stored) → an **optional configured non-secret header name** (from config/env; e.g. `x-client-id`) → **User-Agent fallback** (labelled weaker, *not* the identity model) → `None` (`—`). Defer an auth-principal source until such a seam exists. (Codex review.)

## Data quality (bake into acceptance)
- `measured` label, but tag the **source** so UA-fallback is visibly weaker than principal/key-hash. No identity → `None` → `—`.

## Acceptance criteria
- [ ] `client_label` + `source` on `FlowRecord` + `SnapshotFlowSummary`.
- [ ] Key-hash path: an inbound api-key header yields a **hash**; the **raw key never** appears in the record, summary, logs, or WS (assert redaction).
- [ ] Priority order honored; UA used only as a labelled fallback; no identity → `None` (renders `—`), never a fabricated id or `0`.
- [ ] **secret-safety**: the token/key is **never** stored in the persisted `Config` (AGENTS.md — `Config` is `Debug`/`Clone`); derivation is request-scoped in the middleware.
- [ ] **don't-lie-with-zeros**: absent identity = `unavailable`, never `0`/empty-string-as-id.
- [ ] Round-trip + redaction test.

## Constraints / invariants (AGENTS.md)
- Don't bypass `redact_payload_secrets`; never expose raw secrets; env-only secret posture.

## Out of scope
- Per-client rollup/filter UI (spec 15); abuse/secret-leak detection (later program).

## Validation gate
- **Backend:** `cargo test` (hash + redaction + priority) · `cargo clippy --all-targets` · `cargo fmt`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
