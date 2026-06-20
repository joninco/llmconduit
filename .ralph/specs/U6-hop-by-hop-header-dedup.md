# U6 — Single canonical hop-by-hop header filter for both proxy halves

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** MEDIUM · **Surface:** src/http.rs, src/upstream.rs · **Thermo finding:** `is_hop_by_hop_header` + `header_name_eq` are byte-identical duplicates in both halves of the `/v1/completions` raw proxy; the copies can drift so inbound/outbound strip different header sets (silent correctness hazard).

## Purpose
The `/v1/completions` raw proxy strips hop-by-hop headers in two independent directions, and each direction carries its own private copy of the same RFC 7230 §6.1 connection-header list. `src/upstream.rs:2376-2389` (`is_hop_by_hop_header`, used by `should_proxy_request_header` at `2369-2374` on the outbound request) and `src/http.rs:937-950` (`is_hop_by_hop_header`, used by `should_proxy_response_header` at `933-935` on the inbound response) are byte-for-byte identical: the same 8-element array (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`) in the same order, plus an identical `header_name_eq` helper (`src/http.rs:952-954`, `src/upstream.rs:2391-2393`). Nothing pins the two lists together, so a future edit to one direction (e.g. adding `host` or a vendor connection header) silently desyncs request vs. response filtering — a correctness hazard that no test would catch today (there is no parity assertion). Hoisting one canonical pair into a shared module makes drift impossible by construction while keeping the wire behavior byte-identical.

## Jobs to Be Done
- One canonical `is_hop_by_hop_header` + `header_name_eq` lives in a single shared location; both proxy halves call it.
- The second (duplicate) copy is deleted; `should_proxy_request_header` and `should_proxy_response_header` keep their own direction-specific extra filters (request also strips `authorization`/`host`/`content-length`; response strips `content-length`).
- A test guarantees both directions filter the identical hop-by-hop set, so future drift is caught.

## Acceptance criteria
- [ ] A single canonical `is_hop_by_hop_header(&HeaderName) -> bool` and `header_name_eq(&HeaderName, &str) -> bool` pair lives in one shared location (e.g. a new `src/proxy_headers.rs` module registered in `src/lib.rs`, or a `pub(crate)` item re-used from one half). Both `src/http.rs::should_proxy_response_header` and `src/upstream.rs::should_proxy_request_header` call the canonical functions.
- [ ] The duplicate `is_hop_by_hop_header` + `header_name_eq` definitions are deleted (exactly one definition of each remains in the crate — verify e.g. `grep -rn "fn is_hop_by_hop_header" src/` returns one hit).
- [ ] The hop-by-hop list contents AND order are unchanged: the same 8 lowercase header names (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`) in the same order; `header_name_eq` stays an ASCII-case-insensitive compare.
- [ ] Direction-specific extra filters are preserved byte-identically: `should_proxy_request_header` still additionally drops `authorization`, `host`, and `content-length`; `should_proxy_response_header` still additionally drops `content-length`. No header that was previously proxied becomes stripped (or vice-versa) in either direction.
- [ ] Tests assert the parity invariant. Because `should_proxy_request_header` (`src/upstream.rs`) and `should_proxy_response_header` (`src/http.rs`) are both PRIVATE and live in different modules, no cross-crate `tests/` file can call either — assert the canonical `is_hop_by_hop_header` returns `true` for the full hop-by-hop set in a `#[cfg(test)]` block beside the canonical module, AND verify each direction function in an inline `#[cfg(test)] mod tests` within its OWN module (`src/http.rs` for `should_proxy_response_header`, `src/upstream.rs` for `should_proxy_request_header`): for the full hop-by-hop set both return `false` (strip), and a representative passthrough header (e.g. `content-type`) returns `true` in both directions. Optionally, add an end-to-end parity assertion via an existing `tests/` HTTP-level proxy test that observes the stripped header set on the wire. The tests would fail if the two directions diverged on the hop-by-hop set.
- [ ] Both call sites compile despite the differing imports today (`axum::http::HeaderName` in `src/http.rs:25` vs `http::HeaderName` in `src/upstream.rs:15` — the same re-exported type); the shared signature uses a `HeaderName` both halves can pass without conversion.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/http.rs` (`should_proxy_response_header`, `copy_proxy_response_headers`), `src/upstream.rs` (`should_proxy_request_header`, `copy_proxy_request_headers`).
- **New API:** one shared `pub(crate) fn is_hop_by_hop_header` + `pub(crate) fn header_name_eq` (e.g. in a new `pub(crate) mod proxy_headers` declared in `src/lib.rs` alongside `sse_guard`/`tool_delta_gate`).
- **Depends on:** none.

## Constraints
- Wire behavior MUST be byte-identical: the exact same header set is stripped in each direction before and after this change. This is a pure dedup/refactor, not a wire-contract change.
- Preserve `parallel_tool_calls=false` and all other AGENTS.md HARD RULES (unaffected here, but no incidental edits to request flow).
- Keep `header_name_eq` ASCII-case-insensitive (`eq_ignore_ascii_case`); do not switch to a different comparison.
- Do not change the direction-specific extra filters; only the shared hop-by-hop predicate is hoisted.

## Out of scope
- Adding/removing any header from the hop-by-hop list (the RFC list and order are FINAL for this task).
- Touching non-`/v1/completions` proxy paths or any header logic outside `should_proxy_request_header`/`should_proxy_response_header`.
- Reconciling the two differing `HeaderName` import styles project-wide.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
