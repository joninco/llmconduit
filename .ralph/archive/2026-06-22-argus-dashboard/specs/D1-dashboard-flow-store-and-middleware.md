# D1 — DashboardFlowStore + stateful middleware + api_call_id link

> **Source:** DASHBOARD_PLAN.md rev 8 (Codex-APPROVED) §4.2, §4.7. Topic 13.

**Priority:** HIGH · **Surface:** new `src/dashboard_flow.rs`; `src/http.rs` (middleware), `src/engine.rs` (`stream_responses` link), `src/lib.rs` (construction)

## Purpose
Authoritative store of per-flow records and the on-wire capture seam that feeds the transformation
inspector + metrics. Fixes Codex round-2/4/5 issues: stateless `from_fn` middleware has no `Gateway`
(§4.7 → `from_fn_with_state`); FlowStore records + LRU order under ONE lock; the `OpenL0→ClaimedL1→Finalized`
CAS lives in the record (D3 owns the guard; D1 owns the `claim` field + `open`/`link`/store shape);
bodies are owned capped `Arc<[u8]>` via a redacting streaming serializer that also caps retained scalar
strings, lived ONLY on the live record (never a snapshot).

## Jobs to Be Done
- Every whitelisted inference request (`/v1/responses`, `/v1/messages`, `/v1/chat/completions`) opens a
  flow record keyed by `api_call_id`; `/v1/completions`, `/dashboard*`, `/debug*`, `/health`, `/`,
  `/v1/models`, static assets are skipped (never orphan a record).
- The middleware is stateful (`Arc<Gateway>`) so it can call `flow_store.open(...)`.
- `api_call_id` (http.rs:90) is stashed in request extensions before http.rs:145 so handlers/engine
  can read it; the public `stream_responses(request)` becomes a thin wrapper over an internal
  `stream_responses_with_api_call_id(request, api_call_id)` (no test churn — existing callers keep the
  public signature).
- Bodies stored as `Arc<[u8]>` through a **capped + redacting streaming serializer** (CAP=128 KiB bodies,
  4 KiB per retained scalar string; redacts `Authorization`/`x-api-key`/`api-key` headers, the top-level
  JSON `api_key` field, `openai-beta` tokens, `data:`/`http(s)` image URIs), so peak serializer memory
  is O(CAP) not O(body). The `Bytes::slice` of the 256 MiB middleware buffer is NEVER retained (it
  keeps the backing alive).
- Records are `Arc<FlowRecord>` replaced COW on mutation; the live store enforces a **total live
  summary-byte quota** (bodies + capped scalar strings) evicting oldest bodies first.
- `response_id` (engine.rs:816, API contract) is NOT collapsed to `api_call_id`; `flow_store.link(
  response_id, api_call_id)` fires once at `RequestStarted`.

## Acceptance criteria
- [ ] New `src/dashboard_flow.rs`: `DashboardFlowStore { state: Mutex<DashboardFlowState> }`,
      `DashboardFlowState { by_id: HashMap<String, Arc<FlowRecord>>, order: VecDeque<String> }`,
      cap 512, TTL 30 min (reuse monitor.rs:10/12 constants). `FlowRecord` carries `{claim:
      Arc<AtomicU8>, api_call_id, response_id, method, uri, headers(redacted), inbound_body:
      Option<Arc<[u8]>>, normalized: Option<Arc<[u8]>>, upstream_body: Option<Arc<[u8]>>,
      model_requested, model_served, upstream_target: Option<String>, usage: Option<Usage>,
      status, started_at: Instant, started_ms, finished_ms, elapsed_ms, terminal_reason}`.
- [ ] `open`/`link`/`set_upstream`/`set_normalized`/`finalize`/`record_usage`/`list`/`detail`/
      `snapshot_summaries` APIs; `claim` field present for D3's CAS (D1 just allocates it `OpenL0`).
- [ ] `log_api_call` converted to `middleware::from_fn_with_state(Arc<Gateway>, log_api_call)` with
      `State(gateway): State<Arc<Gateway>>`; existing tracing byte-for-byte unchanged.
- [ ] Body capture uses the capped/redacting streaming serializer (NOT `Bytes::copy_from_slice` of the
      full body then truncate); a unit test asserts peak allocation ≤ CAP for a 10 MiB body.
- [ ] Endpoint whitelist: only the 3 inference paths call `open`; a test asserts NO record is opened
      for `/v1/completions`, `/health`, `/v1/models`, `/dashboard/*`.
- [ ] `api_call_id` stashed in `parts.extensions_mut()` before http.rs:145; `stream_responses`
      wrapper delegates to `stream_responses_with_api_call_id`; existing `tests/gateway.rs` callers
      compile unchanged (public signature preserved).
- [ ] `link(response_id, api_call_id)` is called exactly once per flow (test: concurrent flows link
      correctly); `detail` joins by either id pre-link.
- [ ] Live summary-byte quota (e.g. 64 MiB) evicts oldest bodies (sets their `Arc` to `None`); the
      record stays as a summary.
- [ ] Secret-persistence-prevention test: an inbound body containing `Authorization`/`x-api-key`/
      `api-key` headers and a top-level JSON `api_key` field is captured and the stored `inbound_body`
      + headers are asserted to contain NONE of the original secret values (redacted inline by the
      serializer); same for an upstream body carrying `api_key`.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** nothing (foundational). D2/D3/D5/D6/D7/D13 all build on this.
- **Extends:** `src/http.rs::log_api_call`, `src/engine.rs::stream_responses`, `src/lib.rs` construction.
- **New APIs:** `DashboardFlowStore` (public crate), `ApiCallId` extension, internal `stream_responses_with_api_call_id`.
- **Disabled path:** `DashboardFlowStore::disabled()` and `from_fn_with_state(gateway)` early-return
  when `--with-debug-ui` off — production hot path unchanged (bench in D5).

## Constraints
- Reuse monitor.rs:10/12 retention constants; do not invent new TTL values.
- `response_id` stays `resp_{uuid}` (Responses API contract) — never collapse to `api_call_id`.
- `MonitoreHub::disabled()` zero-overhead principle holds: FlowStore work runs only when
  `--with-debug-ui` is on.
- The capped serializer redacts secrets inline (§6 redaction) — no secret persists even in previews.
- Single-lock discipline: `by_id` + `order` mutated together under the one `Mutex` (no exterior LRU).

## Out of scope
- The L0/L1 telemetry-guard CAS mechanics (D3); D1 only allocates the `claim` field.
- Snapshot history (D5); D1 exposes `snapshot_summaries` but D5 owns the ring.
- Frontend; any REST route handlers (D13).

## Definition of done
- [ ] Acceptance criteria green; zero-cost-disabled-path benchmark (D5) passes; Codex-xhigh APPROVED.
