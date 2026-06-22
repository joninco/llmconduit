# 05 — Spine: gated upstream response/error-body capture 🔭⚙️

> Ralph gap spec — implementation-free. FEATURES.md **item 2 (data-contract pass / the spine)**.
> Backend. Sequence: spine seam — before spec 14 (failure taxonomy).

## Operator question
"When a request failed, what did the upstream actually say back?"

## Current state (verified by code search)
- Only **three request layers** are captured: inbound request `http.rs:449`, normalized request `engine.rs:1128` (`set_normalized()`), upstream **request** `upstream.rs:918` (`set_upstream()`).
- **No upstream RESPONSE/ERROR body field exists** (confirmed — `upstream_body` holds the upstream *request*, post-sanitize).
- Capture whitelist = `/v1/responses`, `/v1/messages`, `/v1/chat/completions`; `/v1/completions` is **not** instrumented (AGENTS.md).

## Scope — what to build
- Add a **separately-gated** capture of the upstream response/error body into the live `FlowRecord` (new field, **OFF by default**, distinct flag from request-capture).
- Capped + redacted via the **capped/redacting streaming serializer** — must **not** retain a `Bytes` slice of the 256 MiB middleware buffer (AGENTS.md anti-pattern: a slice keeps the whole allocation alive).

## Data quality (bake into acceptance)
- `measured` when captured. Distinguish three states: **capture disabled**, **captured-but-empty**, **captured body** — each renders honestly. Never a fabricated body.

## Acceptance criteria
- [ ] New gated upstream-response/error-body field on `FlowRecord`; **OFF by default**, populated only when its flag is set.
- [ ] Capped + redacted; uses the streaming/capped serializer — **does not** hold a slice of the middleware body buffer (copy, don't slice).
- [ ] **Not** added to `SnapshotFlowSummary` / historical snapshots (body-free invariant; the 135 GiB worst-case guard, AGENTS.md).
- [ ] `/v1/completions` remains **not** instrumented (whitelist unchanged).
- [ ] **don't-lie-with-zeros**: capture-off vs captured-empty are distinguishable states; neither implies "no error".
- [ ] Capture on/off test + redaction test + eviction-safety (D5 evict-safe under claim CAS).

## Constraints / invariants (AGENTS.md)
- No blocking IO on the tokio runtime; don't bypass redaction; snapshots stay body-free; copy-not-slice the body buffer.

## Out of scope
- The failure-taxonomy UI (spec 14); retention/sampling policy knobs (later program).

## Validation gate
- **Backend:** `cargo test` (on/off + redaction + evict-safety) · `cargo clippy --all-targets` · `cargo fmt`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
