# D2 — BackendChatRequest identity (response_id + serving token) + leaf on-wire body capture

> **Source:** DASHBOARD_PLAN.md rev 8 §4.4, §4.5. Topic 13.

**Priority:** HIGH · **Surface:** `src/upstream.rs` (BackendChatRequest, leaf `logged_send_chat_request`,
failover/routing rebuilds), `src/engine.rs` (`BackendChatRequest::new`), `src/lib.rs` (leaf handle)

## Purpose
Capture the TRUE on-wire upstream chat-completions body (layer 3 of the transformation inspector) and
tag every flow with the serving upstream. Fixes Codex blockers: the pre-leaf body at engine.rs:1486 is
wrong (mutated later by provider remap upstream.rs:813/1110, `finalize_request_for_backend`+
`sanitize_chat_request` upstream.rs:619-628, shrink-retry upstream.rs:642-660); `logged_send_chat_request`
(upstream.rs:587) only receives `(&self, url, request)` so `backend.response_id` is NOT in scope; an
`Arc<dyn Fn>` callback would break `BackendChatRequest`'s `#[derive(Debug, Clone)]` (upstream.rs:1914); a
client-wide serving token built in `Gateway::new` would race across concurrent responses.

## Jobs to Be Done
- `BackendChatRequest` gains `response_id: Option<String>` + `serving: Option<Arc<ServingToken>>` — both
  `Debug`/`Clone`-safe (no `dyn Fn`); `ServingToken { inner: Mutex<ServingInfo> }`,
  `ServingInfo { route: Option<String>, provider: Option<String> }`.
- The ENGINE allocates a fresh `Arc<ServingToken>` per `stream_responses` call (no cross-flow race) and
  sets `response_id`+`serving` at `BackendChatRequest::new` (engine.rs:1486). The two production rebuilds
  (failover upstream.rs:813, routing upstream.rs:1110) **clone the `Arc`s forward**. (`upstream.rs:3105`
  is a TEST helper — excluded; a test asserts production rebuilds keep both fields.)
- The leaf `ReqwestUpstreamClient` gains a `flow_store: Arc<DashboardFlowStore>` handle (zero-cost
  `Disabled` variant when dashboard off). **`response_id: Option<&str>` is passed explicitly to
  `logged_send_chat_request`** (callers at upstream.rs:628/658 pass
  `backend.response_id.as_deref()`). After `sanitize_chat_request` (upstream.rs:620), the leaf calls
  `self.flow_store.set_upstream(response_id, &request)` via the capped/redacting serializer; the
  shrink-retry path (upstream.rs:658) passes the same id + `&retry`.
- Each layer sets ONLY its own serving field: `RoutingUpstreamClient` sets `route`
  (upstream.rs:1664-1687); `FailoverUpstreamClient::mark_provider_success` (upstream.rs:982) sets
  `provider`. The bare single-upstreamleaf `ReqwestUpstreamClient` (lib.rs:195 — receives the engine's
  wrapper, does NOT bypass) sets a synthetic `"primary"` `provider` at POST time if still `None`.

## Acceptance criteria
- [ ] `BackendChatRequest` (upstream.rs:1914) has `response_id: Option<String>` + `serving:
      Option<Arc<ServingToken>>`; `#[derive(Debug, Clone)]` still compiles (no `dyn Fn`).
- [ ] `BackendChatRequest::new` (engine.rs:1486) is the single production construction point that takes
      + sets both fields; a test asserts the failover (upstream.rs:813) and routing (upstream.rs:1110)
      rebuilds preserve them (clone the `Arc`s); `upstream.rs:3105` test helper is excluded.
- [ ] `logged_send_chat_request` signature gains `response_id: Option<&str>`; both call sites
      (upstream.rs:628, :658) pass `backend.response_id.as_deref()`.
- [ ] A test asserts the stored `upstream_body` equals the POST-`sanitize` body (NOT the pre-leaf
      engine body) and equals the retry body on the shrink-retry path — proving capture is at the leaf.
- [ ] Concurrent-flow test: two flows with distinct serving providers write distinct
      `{route,provider}` (no overwrite — the rev2 race regression).
- [ ] Bare single-upstream path (lib.rs:195) tags flows with synthetic `provider="primary"`.
- [ ] Leaf `flow_store` handle: `Disabled` when `--with-debug-ui` off → `set_upstream` no-ops; enabled
      path stores via the D1 capped serializer.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D1 (`DashboardFlowStore.set_upstream` + capped serializer).
- **Extends:** `src/upstream.rs` leaf, failover, routing; `src/engine.rs` `BackendChatRequest::new`;
  `src/lib.rs` leaf construction (thread the `Arc<DashboardFlowStore>` handle).
- **New APIs:** `ServingToken`, two additive `Option` fields, one added `logged_send_chat_request` param.
- **Consumed by:** D3 (reads `{route,provider}` at finalize), D4 (topology node attrib.

## Constraints
- Additive only: no `UpstreamClient` trait signature change; no `dyn Fn`. The new trait method is D4.
- The 3 production `BackendChatRequest` sites are: engine.rs:1486 (new), upstream.rs:813, upstream.rs:1110.
  `upstream.rs:3105` (`family_backend`) is a TEST helper — do not wire dashboard fields through it.
- Preserve `finalize_request_for_backend` + `sanitize_chat_request` + shrink-retry exactly.
- Steering from AGENTS.md: failover is pre-first-chunk only; routing providers are not failover
  fallbacks. The serving token must reflect the ACTUAL serving provider, not a fallback that was tried
  and skipped.

## Out of scope
- `provider_health()` accessor + DTO (D4).
- Usage capture (D3); D2 only captures the body + serving identity.
- The transformation-inspector diff rendering (frontend, D10).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
