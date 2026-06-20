# DASHBOARD_PLAN.md — Argus: the all-seeing LLM proxy dashboard

> Working title **Argus** for the dashboard (project may later rename `llmconduit` → `Argus`).
> Base branch: `ralph/thermo-followups` @ `24c97f4` (Topic 12 COMPLETE, 10/10).
> All `file:line` anchors below re-verified against that tree on 2026-06-20.
> Revision 8 — incorporates seven adversarial Codex rounds. Round 6 = first APPROVE-WITH-CONDITIONS
> (4 impl-detail conditions). Round 7 confirmed conditions 2-4 closed but flagged condition 1: a
> `DebugUpdate` carries a `Vec<DebugWsMessage>` under ONE `sequence` (monitor.rs:87-93), so
> per-frame envelope wrapping + `seq <= last_seq` dedup would drop sibling frames. Rev8 fixes with a
> **batched** `DashboardFrame { domain, seq, batch: Vec<payload> }` — one envelope per
> `DebugUpdate` (seq = its sequence, batch = its messages), per-domain dedup whole-frame.

---

## 1. Context & goals

llmconduit is a Rust (axum) LLM gateway that accepts three inbound wire formats —
OpenAI Responses, OpenAI Chat Completions, Anthropic Messages — normalizes all to an
internal Responses stream, and forwards to an OpenAI-compatible `/v1/chat/completions`
upstream. It supports multi-upstream routing, failover w/ per-provider cooldown, a union
model catalog, Brave web search, and a vision image-strip agent.

Today the only observability is:
- a `/debug` HTML UI (`src/debug.html`, 2833-line vanilla-JS file served via `include_str!`)
  that is a **per-request live transcript viewer** — no aggregates, no charts, no routing
  or cost insight;
- `tracing` logs per request (status, served_model, elapsed_ms) that are discarded;
- an offline `analyze-log` JSONL prefix-stability command.

**Goal:** build a flagship, professional, realtime web dashboard that makes the gateway's
unique value visible — *what it transforms, how it routes, what it costs, and what it is
streaming right now* — packaged as a React + TS + Vite SPA embedded into the single Rust
binary.

**Thesis:** the transformation inspector defines the product. The other five views (upstream
topology, token-flow Sankey, live stream theater, stats strip, time-travel scrubber) form the
operating story around it.

### Six components

1. **Transformation inspector** (centerpiece) — flow table + 3-layer side-by-side detail:
   raw inbound → normalized internal Responses → upstream chat-completions payload actually
   sent, scroll-synced, JSON-highlighted, diff-tinted. Plus streamed-deltas sub-panel.
2. **Upstream topology map** — live node graph; gateway center, upstream nodes on a ring;
   node color = health (healthy/cooling/down); animated edges show throughput; click node
   filters the flow table.
3. **Token-flow Sankey** — client → gateway → upstream-model, band width = tokens/time,
   color = cost; makes spend visceral; click band filters.
4. **Live stream theater** (the "wow", fullscreen) — dark cinematic rivers of active streams:
   output bright, reasoning dim, tool calls as cards; per-stream tokens/sec meter; multi-grid
   to watch N models at once.
5. **Stats strip** (always-on top bar) — req/s, active streams, error%, p50/p95/p99, tokens/s,
   $/min; sparkline trends; 1m/5m/1h selector. Connective tissue.
6. **Time-travel scrubber** — timeline under the strip; drag playhead → entire dashboard
   renders as-of that moment; LIVE toggle returns to realtime.

---

## 2. Sequencing — Rust vertical slice FIRST (was: frontend-first)

> **REVERSAL from rev 1.** Codex correctly identified that frontend-first freezes a contract
> the Rust side cannot satisfy (true on-wire body, serving provider, terminal usage/latency).
> Build order is now: a minimal Rust vertical slice proving the data is producible, THEN the
> contract is real, THEN the frontend (mock is still used for view R&D in parallel late, but
> the contract is anchored to working Rust, not a wish).

### Phase 0 — Rust vertical slice (prove the contract)
- api_call_id ↔ response_id link (§4.2).
- authorititative FlowStore + MetricsLayer w/ per-domain sequence cursors (§4.1, §4.8).
- true on-wire upstream-body capture via a leaf instrumentation callback (§4.4).
- serving-provider identity surfacing through all 3 `BackendChatRequest` constructors +
  synthetic name for bare `ReqwestUpstreamClient` (§4.5).
- terminal usage + status + latency recorded from engine/leaf, NOT middleware headers (§4.3).
- ONE end-to-end flow: a streamed request populates a flow record viewable via `GET
  /dashboard/api/flows/:id` + its live deltas over `/dashboard/ws`. Auth on (§6).

### Phase 1 — frontend foundation + transformation inspector
Stats strip, virtualized flow table, 3-pane inspector + deltas. Against the now-real contract.
### Phase 2 — topology (radial + particles).
### Phase 3 — theater + Sankey.
### Phase 4 — time-travel scrubber (needs stable `/snapshot`).

---

## 3. Frontend — React + TS + Vite

### Decision
React 18 + TypeScript + Vite, built to static assets embedded into the Rust binary. Chosen
for the highest quality ceiling on data-viz-heavy ops dashboards. **Trade-off accepted:** a
permanent `node`/`npm` toolchain in the release build path (opt-in via build flag, §6).

### Layout
```
dashboard-frontend/
  package.json            # React18, TS, Vite, d3, d3-sankey, uPlot,
                          # @tanstack/react-virtual, @tanstack/react-query, zustand,
                          # highlight.js, tailwind, shadcn/ui   (framer-motion DEFERRED — §3.3)
  vite.config.ts          # build outDir ../OUT_DIR-relative, base './', mock mode via env
  tsconfig.json
  index.html
  src/
    main.tsx  App.tsx
    api/{client,ws,types,mock}.ts
    store/{flows,metrics,topology,session}Store.ts
    hooks/
    views/{Flows,Topology,Sankey,Theater}View.tsx
    components/{StatsStrip,Scrubber,FlowTable,FlowDetail}/
    components/viz/{JsonPane,RadialTopology,TokenSankey,Sparkline}.tsx
```

### Build integration (build.rs) — STUB-FIRST, sound on node-less hosts
> Fix for Codex blockers #9 (rev1) and #5 (rev2): `include_dir!` takes a single **string
> literal** (the crate expands `$VAR`), NOT a `concat!(env!(...))` which won't compile.
- `build.rs` **always** ensures a stub asset directory exists under `OUT_DIR` (e.g.
  `$OUT_DIR/dashboard_dist/`) with a minimal `index.html` ("dashboard not built; build with
  LLMCONDUIT_BUILD_DASHBOARD=1"). Emit `cargo:rerun-if-env-changed=LLMCONDUIT_BUILD_DASHBOARD`
  and `cargo:rerun-if-changed=dashboard-frontend/src`, `package.json`, lockfile, `vite.config.ts`,
  `index.html`.
- When `LLMCONDUIT_BUILD_DASHBOARD=1`: **clear** `$OUT_DIR/dashboard_dist/`, then shell to
  `npm ci && npm run build` with Vite `outDir` = that same `$OUT_DIR/dashboard_dist/`. Fail the
  build loudly if npm is missing. (Clearing first prevents stale assets from a prior build
  lingering.)
- `src/dashboard_ui.rs`:
  ```rust
  use include_dir::{include_dir, Dir};
  static DASHBOARD_DIST: Dir<'static> = include_dir!("$OUT_DIR/dashboard_dist");
  ```
  The macro needs the `static` binding (rev3 #6 — a bare `include_dir!(...)` is an expression of
  type `Dir`, not a usable item). The path is guaranteed to exist (build.rs stub) and `$OUT_DIR`
  is expanded by the macro, so it compiles on node-less hosts. Serve `/dashboard/*` from
  `DASHBOARD_DIST`; SPA shell at `/dashboard`. When the build flag is OFF, build.rs **clears/rewrites
  the stub** (not just leaves the dir) so assets from a prior enabled build don't linger embedded.
- Add the `include_dir` crate to `Cargo.toml` (cheap; route registration still gated by
  `--with-debug-ui`).

### State & data plumbing
- **TanStack Query** for REST: `/flows`, `/flows/:id`, `/metrics`, `/topology`, `/catalog`,
  `/snapshot`. Cache + invalidate-on-WS-event (a `flow_status` frame carrying its per-domain
  `flow_seq` cursor invalidates the detail query for that response_id).
- **zustand** for WebSocket-derived live state (flows map, metrics, topology).
- **Single `DashboardSocket`** on `/dashboard/ws`. Snapshot-then-live, plus `usage` and
  `metric_tick` frames (each carrying its per-domain `{domain, seq}` cursor — §4.8). On time-travel
  **seek**, pause applying frames (shadow-buffer), switch to `/snapshot?at=` (rAF-throttled,
  LRU-keyed by second bucket); LIVE toggle replays shadowed frames.
- `useSyncExternalStore` bridges zustand → React 18 concurrent.

### 3.3 Viz + React 18 correctness (fix for Codex #11)
- Isolate ALL imperative viz (d3-force, d3-sankey, uPlot) behind `useLayoutEffect` with full
  cleanup (destroy sim / dispose uPlot / remove SVG). D3 computes layout where possible; React
  renders the resulting DOM; d3 mutates only guarded refs.
- StrictMode-safe: effects must be idempotent — re-running must not leak force simulations or
  duplicate SVG nodes. Guard with a ref-owned mount node.
- **Defer framer-motion** until a view demonstrably needs it; do NOT use FLIP animations
  inside virtualized rows (react-virtual reflows fight FLIP).
- highlight.js for JSON; a hand-rolled structural-diff overlay pass (no extra dep).

### Viz libraries (sizes gzip-approx)
- **d3-force** + **d3-sankey** — topology + token Sankey.
- **uPlot** (~45 KB) — sparklines + latency trends.
- **@tanstack/react-virtual** — virtualized flow table.
- **highlight.js** — JSON syntax. **shadcn/ui + Tailwind** — components.
- **framer-motion** — deferred (add only when the theater genuinely needs it).

### Design system (dark ops — Grafana density × Linear polish × mitmweb clarity)
- Palette: bg `#0d0f12`, panel `#16191e`/`#1e2329`, line `#2a313a`; status healthy `#58d68d`,
  cooling `#f6c453`, down `#ff6b6b`; accent `#6bb6ff`; meta `#c58bd1`; diff added/changed/removed.
- Type: system-sans UI; `ui-monospace, "JetBrains Mono", SF Mono` for payloads;
  `font-variant-numeric: tabular-nums` on numeric chips. 4 px grid, 6/10 px radii, 1 px lines
  + `rgba(0,0,0,.24)` shadows.
- Motion: 120 ms hover, 200 ms panels, cubic playhead, edge particles on topology;
  `prefers-reduced-motion` cuts particles. Glow reserved for the active path only.

---

## 4. Instrumentation seams (verified anchors, fixes applied through codex round 2)

### 4.1 Authoritative state + coordinated 5 s snapshots (fixes Codex #8 rev1 & #3 rev2)
> Three independent live stores (FlowStore, MetricsLayer, topolog) read separately are NOT an
> atomic cut; filtering current flow records by `started_ms <= at` exposes later bodies,
> provider, usage, and terminal status (they mutate after `started`). MonitorHub keeps its own
> separate sequence domain. A monotonic `seq` alone does not make time-travel consistent.

**Authoritative stores** (authoritative for bodies/usage/provider/metrics):
- **`src/metrics.rs` — `MetricsLayer`**: per-window ring buffers 1m/5m/1h (60/300/3600 slots @1 s),
  buckets `{status_class, model, endpoint, upstream}`; latency histogram 30 log-spaced buckets
  1 ms..120 s, p50/p95/p99 by linear interpolation; tokens summed per slot.
- **`src/dashboard_flow.rs` — `DashboardFlowStore`**: the flow records (§4.2).
- **topology** is served live from `provider_health()` (§4.6); it has no time history of its own.

**Time-travel via coordinated immutable snapshots (fix for #3 rev2, #2/#3 rev3, #2/#3 rev5):**

Atomicity + memory are solved together — and snapshots are **body-free**:
- FlowStore stores `HashMap<String, Arc<FlowRecord>>` — on mutation a new `Arc<FlowRecord>` is
  built and swapped (COW). **Bodies (`Arc<[u8]>`) live ONLY in the live FlowRecord**, referenced
  by the live store's bounded body cache; a snapshot NEVER holds body `Arc`s. This is the fix for
  the 135 GiB error: snapshots retain evicted `FlowRecord`s in rev4 because their body `Arc`s kept
  the 128 KiB allocations alive (720 × 512 × 3 × 128 KiB). A body-free snapshot holds only the
  scalar/summary fields (api_call_id, response_id, method, uri, model_requested/served,
  upstream_target, usage, status, started/elapsed, terminal_reason) → worst case is
  ~720 × 512 × <1 KiB ≈ 360 MiB of *summaries*, independently bounded by a **snapshot-summary
  quota** (not 135 GiB). The time-travel inspector renders state/metrics/usage/provider for the
  selected flow; its body panel reads from the **live** store (shown if the flow is still
  retained, else "body evicted — view live"). This is an acceptable trade for the lowest-priority
  view and eliminates the unbounded retention.
- **Atomicity (rev3/rev4 #2):** the snapshot task takes the stores' locks **simultaneously in a
  fixed order** — FlowStore mutex, then MetricsLayer mutex — in ONE critical section, reads both,
  captures a **single `Arc<ProviderHealthSnapshot>`** for topology (rev5 #3: reading
  `provider_health()` field-by-field over multiple atomics would tear; instead the engine publishes
  a versioned immutable `Arc<ProviderHealthSnapshot>` swapped in whenever any provider state
  changes, and the snapshot task captures it once under the critical section), assembles the
  immutable snapshot, releases. This is a true atomic cut across all three stores; it does NOT
  depend on writers taking a barrier lock. The global critical section is brief (pointer + scalar
  copies, no body copy) and runs every 5 s — acceptable. FlowStore and MetricsLayer mutations use
  their own locks; the documented lock **order** (FlowStore → MetricsLayer → topology-Arc-capture)
  applies to code that ever holds more than one — only the snapshot task does, so no deadlock.
- Each snapshot entry is a distinct body-free `SnapshotFlowSummary { api_call_id, response_id,
  method, uri, model_requested, model_served, upstream_target, usage, status, started_ms,
  elapsed_ms, terminal_reason }` — no `Arc<[u8]>`, no body, no reference into the live store.
- `SnapshotRing` retains 720 body-free summary snapshots (1 h). `DashboardSnapshot` carries
  **per-domain cursors** `{flow_seq, metrics_seq, topology_seq, monitor_seq}` (rev4 #5), NOT a
  singular `seq`.
- `GET /dashboard/api/snapshot?at=` returns the nearest body-free snapshot ≤ at — the only source
  for time-travel.

**Monotonic seq (fix for #10 rev2 — scoped, per-domain, not global-contention):** each
authoritative store keeps its own internal ordering; broadcast WS frames are tagged with a
`{domain, seq}` per-domain cursor (§5) so clients detect gaps per domain without a single global
watermark discarding valid frames (rev3 #7). Avoid a single global `AtomicU64` fanned into every
transcript delta (contention); MonitorHub keeps its existing sequence under `domain="monitor"`
unchanged. Confirm with a load benchmark (§7) that per-domain seq updates are not a hot-spot; if
they are, batch deltas per tick within a domain.

MonitorHub (src/monitor.rs) stays ONLY for transcript/streaming-delta broadcast (theater rivers
reuse segment deltas). It is NOT the source of truth for bodies, usage, provider, or metrics.

### 4.2 FlowStore: single-lock + endpoint whitelist + owned capped bodies (fixes Codex #7 rev1, #7 rev2)
`DashboardFlowStore { state: Mutex<DashboardFlowState> }` where
`DashboardFlowState { by_id: HashMap<String, Arc<FlowRecord>>, order: VecDeque<String> }` —
**records + LRU order under ONE lock** (rev2 had `order` outside the mutex → unsound `&self`
mutation; rev6 #4: `by_id` holds `Arc<FlowRecord>` to match the COW design). Cap 512, TTL 30 min.

`FlowRecord`: `{seq, claim: Arc<AtomicU8>, api_call_id, response_id, method, uri, headers(redacted), inbound_body:
Option<Arc<[u8]>>, normalized: Option<Arc<[u8]>>, upstream_body: Option<Arc<[u8]>>,
model_requested, model_served, upstream_target: Option<String>, usage: Option<Usage>,
status, started_at: Instant, started_ms, finished_ms, elapsed_ms, terminal_reason}`.
Records live in the live store as `Arc<FlowRecord>`, replaced (COW) on mutation. **Bodies
(`Arc<[u8]>`) live ONLY in the live FlowRecord** — they are NOT shared with snapshots (rev5 #2:
an earlier draft said snapshots shared body `Arc`s; that would restore the 135 GiB failure).
Snapshots store a distinct **body-free** `SnapshotFlowSummary` (§4.1) holding only scalar/summary
fields. The live FlowStore enforces a **total body-byte quota** (e.g. 64 MiB across all retained
bodies); on overflow it evicts oldest bodies (sets their `Option<Arc<[u8]>>` to `None`, record
stays as a summary). An additional **snapshot-summary quota** (e.g. 400 MiB across the 720
snapshots' `SnapshotFlowSummary` vecs) caps historical summaries independently.

> **Endpoint whitelist (rev2 #7):** the dashboard middleware opens a flow ONLY for the inference
> endpoints that go through the engine — `/v1/responses`, `/v1/messages`, `/v1/chat/completions`.
> It MUST skip `/v1/completions` (raw passthrough, bypasses the engine — http.rs:763), `/dashboard*`,
> `/debug*`, `/health`, `/`, `/v1/models`, and static assets — those never get a guard and would
> orphan records. Match on `uri.path()`. (`/v1/completions` is explicitly OUT — see §4.3.)

> **Memory (rev1 #7, rev6 #3):** owned, copied-once, capped `Arc<[u8]>` via a **capped/redacting
> serializer** — NOT `Bytes::slice` (keeps the 256 MiB backing alive) and NOT "serialize the whole
> huge request then truncate" (allocates the full body first, rev2 #9). The serializer writes
> through a cap-aware `Write` that stops at CAP (128 KiB bodies, **and caps every retained scalar
> string** — `model_requested`, `model_served`, `uri`, headers, `last_error`, `terminal_reason` —
> at e.g. 4 KiB so a pathological model id/URI can't blow the `<1 KiB` summary estimate), redacting
> image URIs/secrets inline, so peak memory is O(CAP) not O(body). **The live summary-byte quota
> (§4.2) covers ALL dynamic allocations** — bodies + capped scalar strings — so it bounds total live
> FlowStore memory, not body bytes alone. The single capped `Arc<[u8]>` lives only on the live
> `FlowRecord` (never on a snapshot — §4.1/§4.2). Replay — which needs full bodies — is **deferred**
> (§4.6, §6); capped previews can't faithfully replay oversized requests.

**api_call_id engine.rs:816 (`resp_{uuid}`, API contract) — not collapsed.** Mechanism:
- dashboard middleware (stateful, §4.7) mints `api_call_id` (http.rs:90), stashes it in
  `parts.extensions_mut().insert(ApiCallId(...))` before http.rs:145, and calls
  `flow_store.open(api_call_id, method, uri, headers, inbound_preview)` for whitelisted paths.
- handlers extract `Extension<ApiCallId>`, thread `api_call_id: Option<String>` into the engine.
  Public `stream_responses(request)` (engine.rs:705) becomes a wrapper over internal
  `stream_responses_with_api_call_id(request, api_call_id)` so tests/callers keep their signature.
- engine calls `flow_store.link(response_id, api_call_id)` once, at `RequestStarted` emission.

### 4.3 Terminal telemetry guard — spans pre-spawn AND spawned (fixes Codex #6 rev1 & rev2)
> The streamed path spawns `run_turn` at engine.rs:817 (run_turn at :1041); provider selection +
> usage + true latency land INSIDE that task, after handlers built headers (http.rs:717/753).
> Worse, validation failures at engine.rs:809 (context-window bad_request) and midstream failures
> (engine.rs:1609/2197…) happen at varying points; some skip `into_response_usage()` (engine.rs:1805),
> losing partial usage; latency from epoch-ms is non-monotonic. Middleware cannot record any of
> this via headers.

**Terminal guard — state-machine ownership transfer (fixes #6 rev1/#2, #4 rev3, #1 rev4):**
> rev4 gap: only `ApiCallId` crosses `next.run` into the handler — the middleware-owned L0 guard
> object does NOT, so a separate L1 guard inside the engine cannot "disarm" it by reference. The
> hand-off must be *state in the shared FlowStore*, not object ownership.

The record carries an `Arc<AtomicU8> claim` with states `OpenL0 → ClaimedL1 → Finalized`:
- **L0 (middleware, RAII):** `flow_store.open(...)` creates the record with `claim = OpenL0` and
  returns a `MiddlewareGuard` holding the `api_call_id` + a handle. Its `Drop` runs a
  `compare_exchange(OpenL0 → Finalized-Failed("unhandled"))` — it finalizes **only if still
  OpenL0** (i.e. the engine never claimed it: extractor/conversion failure, or `next.run` errored
  before the handler ran). If L1 already claimed, L0's Drop is a no-op. This is race-free via CAS.
- **L1 (engine):** at the top of `stream_responses` the `TelemetryGuard` does
  `compare_exchange(OpenL0 → ClaimedL1)`. If it succeeds, L1 owns finalization; L0's Drop is now a
  no-op. (If it fails — record already gone — L1 proceeds without a record, rare.) L1 holds
  `{api_call_id, response_id: Option<String>, started: Instant}` and finalizes on every exit path
  (pre-spawn :809, spawned :817, Completed/Failed) → `ClaimedL1 → Finalized`. RAII `Drop` fallback
  finalizes only if still `ClaimedL1`. `/v1/completions` is NOT whitelisted → never opens a record
  → no orphan, no instrumentation.
- **Incremental usage (rev3 #4, rev4 #4, rev5 #1):** OpenAI `usage` is **cumulative for the
  stream** (the final chunk carries the total), so `accumulated_usage.add(chunk.usage)` must NOT
  run on every chunk (double-count), and `accumulated_usage.snapshot()` at engine.rs:1513 excludes
  the current turn because `add()` runs only after the loop (engine.rs:1676) — a midstream failure
  would record zero/previous-round. Correct scheme: keep a `turn_base = accumulated_usage.snapshot()`
  captured at turn start; on each usage-bearing chunk compute
  `total = turn_base + chunk.usage` (turn-local cumulative, no double-count) and **upsert** that
  into the record; after the loop (engine.rs:1676) do the single authoritative
  `accumulated_usage.add(turn_usage)` + final snapshot so the next turn's `turn_base` is correct.
  A midstream cancel/`next_upstream_chunk…await?` keeps the last upserted `total`. Behavior when no
  usage chunk has arrived: `usage = None` (display "no usage reported").

### 4.4 On-wire upstream-body capture at the leaf, keyed by response_id (fixes Codex #1 rev1 & rev2)
> The body built at engine.rs:1486 (`BackendChatRequest::new`) is PRE-leaf. The actual on-wire body
> is mutated later by failover/routing provider remap (upstream.rs:813/:1110), leaf
> `finalize_request_for_backend` + `sanitize_chat_request` (upstream.rs:619-628), and shrink-and-retry
> (upstream.rs:642-660). Capturing at the engine captures the WRONG layer. Also rev2's
> `Arc<dyn Fn>` callback broke `BackendChatRequest`'s `#[derive(Debug, Clone)]` (upstream.rs:1914)
> and `logged_send_chat_request` (upstream.rs:587) couldn't reach it; and `response_id` was never
> on `BackendChatRequest`, so the leaf couldn't key the store.

Fix — **typed instrument handle, not a `dyn Fn`:**
- Add `response_id: Option<String>` to `BackendChatRequest` (upstream.rs:1914). It's `Option<String>`
  → `Debug`/`Clone` still derive. The engine sets it at `BackendChatRequest::new` (engine.rs:1486);
  the failover (upstream.rs:813) and routing (upstream.rs:1110) rebuilds **preserve** it via `Clone`
  + explicit set. (`upstream.rs:3105` is a test helper only — not a production site; cover with a
  test that production rebuilds keep `response_id`.)
- The **leaf** `ReqwestUpstreamClient` gains a `flow_store: Arc<DashboardFlowStore>` handle
  (zero-cost `Disabled` variant when dashboard off). The capture must happen where the FINAL body
  exists: `logged_send_chat_request` (upstream.rs:587) currently takes only `(&self, url, request)`
  — `backend.response_id` is NOT in scope there. So **pass `response_id: Option<&str>` as an
  explicit parameter** to `logged_send_chat_request` (the callers at upstream.rs:628/:658 already
  have `backend`/`request` in scope to pass `backend.response_id.as_deref()`). Inside, AFTER
  `sanitize_chat_request` produces the on-wire `request`, the leaf calls
  `self.flow_store.set_upstream(response_id, &request)` — capped/redacting serializer (§4.2),
  storing the true on-wire body. On the shrink-retry path (upstream.rs:658) pass the same
  `response_id` and call again with `&retry`, so the inspector shows the actual retried body.
  `None` response_id (dashboard off) → `Disabled` handle no-ops. No `dyn Fn`, no Debug breakage,
  one added parameter on a private method.

**normalized (layer B):** serialized (same capped/redacting path) at the engine where the
normalized Responses object is materialized, before `build_upstream_chat_request`.

### 4.5 Serving-provider identity — PER-RESPONSE structured token (fixes Codex #5 rev1 & #2 rev2)
> A client-wide token built in `lib.rs`/`Gateway::new` (rev2) lets concurrent responses OVERWRITE
> each other's provider — a race. And `Option<String>` can't represent route+leaf identity safely.
> The bare single-upstream `ReqwestUpstreamClient` (lib.rs:195) does NOT bypass
> `BackendChatRequest` — it still receives the engine's wrapper.

Fix — **allocate per response_id, structured, layer-scoped:**
- Add `serving: Option<Arc<ServingToken>>` to `BackendChatRequest` (upstream.rs:1914), where
  `ServingToken { inner: Mutex<ServingInfo> }`, `ServingInfo { route: Option<String>,
  provider: Option<String> }` — `Debug`/`Clone` derive (`Arc` of a small Mutex is Debug/Clone).
- The ENGINE allocates a **fresh** `Arc<ServingToken>` per `stream_responses` call (so concurrent
  responses can't share/overwrite — fixes the race) and sets it on `BackendChatRequest::new`
  (engine.rs:1486); the failover (upstream.rs:813) and routing (upstream.rs:1110) rebuilds clone
  the `Arc` forward.
- Each layer updates **only its own field**: `RoutingUpstreamClient` sets `route` when it selects a
  routing provider (upstream.rs:1664-1687); `FailoverUpstreamClient::mark_provider_success`
  (upstream.rs:982) sets `provider` to the failover provider name. The bare single-upstream leaf
  sets `provider` to a synthetic `"primary"` (or `upstream_base_url` host) at POST time if still
  `None` — so single-upstream deployments still tag flows (covers the lib.rs:195 path, which uses
  the engine wrapper).
- At finalize (§4.3) the guard reads `{route, provider}` → `upstream_target =
  format!("{route}/{provider}")` (or just `provider`) into the flow record.

### 4.6 provider_health + topology DTO + async catalog (fixes Codex #6 rev1 & #8 rev2)
> A non-async default `fn provider_health` is dyn-safe with `Arc<dyn UpstreamClient>`
> (lib.rs:123/engine.rs:529), BUT the routing catalog is `Arc<AsyncMutex<…>>` (upstream.rs:343) —
> a sync method CANNOT read its age/size. `Down` is also undefined.

Fix: catalog metadata + per-provider counters are **shared behind `Arc`** (so the derived `Clone`
on `RoutingUpstreamClient`/`FailoverUpstreamClient` — upstream.rs:340/212 — still works; bare
`AtomicU64` fields would break it), and the metadata pair is published coherently:
- `Arc<ProviderMetrics>` per provider, holding the counters; cloned by the parent structs (Arc
  clone, `Clone` preserved). Counters: `served_count`, `failover_count` (cumulative), and a
  `consecutive_failures` reset to 0 at `mark_provider_success` (the basis for `Down`).
- Catalog metadata: a small `Arc<CatalogMeta{fetched_ms, size}>` swapped atomically inside
  `refresh_catalog` (under the existing `AsyncMutex` hold) then read lock-free by the sync
  accessor — both fields together, no torn pair.
- `provider_health()` stays a non-async default trait method (upstream.rs:85, mirrors
  `supported_model_catalog` at :115). `ProviderHealth` is owned/serializable (epoch-ms, not
  `Instant`):
  ```rust
  pub struct ProviderHealth {
      pub id: String, pub name: String, pub route: Option<String>, pub base_url: String,
      pub status: ProviderStatus,            // Healthy | Cooling | Down
      pub cooling_until_ms: Option<u64>,
      pub last_error: Option<String>,
      pub served_count: u64,                  // cumulative
      pub failover_count: u64,                // cumulative
      pub consecutive_failures: u64,          // reset on success
      pub catalog_fetched_ms: Option<u64>,
      pub catalog_size: usize,
  }
  ```
- `Down` semantics (rev2 #8 defined, now with the counter to back it): `Cooling` =
  `cooling_until > now`; `Down` = `Cooling` AND `consecutive_failures >= DOWN_THRESHOLD` (default 3);
  `Healthy` = neither. `FailoverUpstreamClient::provider_health()` reads `self.states`
  (upstream.rs:212/437) + its `Arc<ProviderMetrics>`; `RoutingUpstreamClient` aggregates per nested
  provider with `route = Some(route_id)` and the `Arc<CatalogMeta>`. `Gateway::upstream_health()`
  exposes it.
- **Topology publication cadence (rev6 #2):** the immutable `Arc<ProviderHealthSnapshot>` is NOT
  republished on every `served_count` increment (would allocate O(provider-count) per request).
  Instead: a background task republishes the snapshot on a **coalesced 1 s tick**, and is also
  woken by a deadline timer at the next `cooling_until` expiry — so a cooldown expiring with no
  traffic still flips the node from Cooling→Healthy (an idle provider isn't stuck displayed as
  cooling). Counters (`served_count`/`failover_count`) update in the `Arc<ProviderMetrics>`
  atomically; the published snapshot reads them at tick time. WS `TopologyUpdate` piggybacks the
  same 1 s cadence.

**Replay/Kill (revised):** kill stays (AbortHub, defined in §5). **Replay is DEFERRED to a later
phase** — when revived it requires a bounded `0600` disk spool with quota, TTL, cleanup, and a
redacting streaming writer (§6). It is removed from Phase 0 / Phase 1 to avoid shipping an
unsafe resource surface; the inspector + kill remain the mutation surface for now.

### 4.7 Stateful middleware + endpoint whitelist (fixes Codex #4 rev1 & #7 rev2)
`log_api_call` is `middleware::from_fn` (http.rs:85) — stateless. Replace with
`middleware::from_fn_with_state(Arc::clone(&gateway), log_api_call)`,
`async fn log_api_call(State(gateway): State<Arc<Gateway>>, request: Request, next: Next)`.
Existing tracing unchanged. For whitelisted inference paths only: stash `ApiCallId` in extensions
before http.rs:145, hand the owned-capped inbound preview to `flow_store.open(...)`. Non-whitelisted
paths skip flow-store work entirely.

### 4.8 Construction + zero-cost disabled path + benchmark (fixes Codex #10 rev2)
In the `with_debug_ui` branch build `DashboardFlowStore`, `MetricsLayer`, `AbortHub`, the 5 s
snapshot task, and the leaf `flow_store` handle; pass into `Gateway::new` (lib.rs:232; additive to
the current 8 params, engine.rs:526). In the `disabled()` branch build `Disabled` handles whose
`open`/`record`/`set_upstream`/`set_provider` are early-return no-ops and whose `BackendChatRequest`
`response_id`/`serving` are `None`. **Qualify "zero-cost" (rev2 #10):** add a criterion micro-bench
comparing a streaming request with dashboard off vs on-and-off-path, asserting no added allocation
and no added clone of the wrapper in the disabled path (if the `Option` fields enlarge it, ensure
they're `Option<Arc<…>>` so clone is a refcount bump, or skip construction entirely when disabled).

---

## 5. Rust API (contract frozen AFTER Phase 0 slice proves it)

All new routes gated behind `--with-debug-ui` in the existing `if options.with_debug_ui` block
(http.rs:75). New module `src/dashboard_ui.rs` mirrors `src/debug_ui.rs`. **All REST + WS
require dashboard auth (§6).**

### REST
| Route | Handler | Response |
|-|-|-|
| `GET /dashboard` | `dashboard_index` | SPA shell (embedded `dist/index.html`) |
| `GET /dashboard/api/flows?status=&model=&upstream=&page=&limit=` | `dashboard_flows` | `{flows:[FlowSummary], total, flow_seq}` |
| `GET /dashboard/api/flows/:id` | `dashboard_flow_detail` | `{flow_seq, inbound_body, inbound_headers, normalized, upstream_body, model_requested, model_served, upstream_target, usage, deltas:[...], terminal_reason, started_ms, elapsed_ms}` |
| `GET /dashboard/api/metrics` | `dashboard_metrics` | `{metrics_seq, reqs_per_sec, active_streams, error_pct, p50,p95,p99, tokens_per_sec, cost_per_min, windows:{m1,m5,h1}}` |
| `GET /dashboard/api/topology` | `dashboard_topology` | `{topology_seq, nodes:[ProviderHealth...], edges:[{from,to,throughput,tokens_per_sec,cost_per_sec}]}` |
| `GET /dashboard/api/catalog` | `dashboard_catalog` | `[{id,context_limit}]` |
| `GET /dashboard/api/snapshot?at=<unix_ms>` | `dashboard_snapshot` | `{cursors:{flow_seq,metrics_seq,topology_seq,monitor_seq}, at_ms, summaries:[SnapshotFlowSummary], metrics, topology}` body-free frozen cut |
| `POST /dashboard/api/flows/:id/replay` | — | **DEFERRED** (§4.6/§6); route not registered in Phase 0/1. |
| `POST /dashboard/api/flows/:id/kill` | `dashboard_flow_kill` | **gated** (§6): cancel active stream via AbortHub; requires CSRF token. |

### WebSocket
`GET /dashboard/ws` mirrors `debug_ws` (debug_ui.rs:25-48): snapshot-then-live with **per-domain**
dedup. The existing `DebugWsMessage` frames carry no per-frame seq — a `DebugUpdate` carries a
whole `Vec<DebugWsMessage>` under **one** `sequence` (monitor.rs:87-93), so per-frame wrapping
would give sibling frames the same seq and `seq <= last_seq` would drop all but the first. The
dashboard therefore uses a **BATCHED envelope per update** (rev6→rev7 #1):
```rust
// dashboard-only; /debug/ws keeps the bare DebugWsMessage contract unchanged.
DashboardFrame { domain: Domain, seq: u64, batch: Vec<DashboardPayload> }
enum Domain { Flow, Metrics, Topology, Monitor }
enum DashboardPayload {
    Monitor(DebugWsMessage),      // one per message in the originating DebugUpdate batch
    Usage { response_id, prompt, completion, total, cached, reasoning },
    MetricTick { /* metrics-shape mirror of /api/metrics */ },
    FlowStatus { response_id, status, served_model, upstream_target, usage, elapsed_ms },
    TopologyUpdate { /* ProviderHealth snapshot */ },
}
```
Dedup is **per-domain per-batch**: `{domain} => seq <= last_seq[domain]` drops the whole frame,
processes it if `> last_seq`. The `Monitor` frame's `seq` IS the originating `DebugUpdate.sequence`
(one envelope per DebugUpdate, `batch` = its messages) — so dedup matches existing semantics and
no sibling frame is lost. The `metric_tick` fires every 1 s from MetricsLayer (its own metrics seq);
`TopologyUpdate` on the coalesced 1 s tick (§4.6, own topology seq); `FlowStatus`/`Usage` carry the
flow-domain seq (batched when multiple finalize in one update).

### AbortHub + lifecycle (fixes Codex #9 rev3)
`AbortHub { handles: Mutex<HashMap<String, CancellationToken>> }` on `Gateway`, keyed by
**`api_call_id`** (which IS the flow id used by `/dashboard/api/flows/:id` — so the kill route's
`:id` matches without rekeying). The engine's L1 TelemetryGuard registers its `CancellationToken`
under the `api_call_id` when it claims ownership (§4.3). Engine checks `token.is_cancelled()`
alongside existing `tx.is_closed()` checks (grep in engine.rs). `dashboard_flow_kill` looks up by
`api_call_id` and calls `cancel()`. **Cleanup:** the L1 guard **removes** the entry from AbortHub on
finalize (so completed/failed flows don't leak entries); `Drop` fallback also removes. Entries are
bounded by the in-flight stream count (not the 512 flow history).

### Price table (config, additive)
Add `pub price_table: HashMap<String, ModelPrice{input_per_1k,output_per_1k,cached_per_1k}>` to
`Config` (src/config.rs), from YAML `price_table:` + env `LLMCONDUIT_PRICE_TABLE_JSON` (mirror
the `upstream_chat_kwargs` env pattern). `Gateway::price_for(model)` accessor; flow detail
computes `cost = usage × price`; `/topology` returns the table; `cost_per_min` rolls up.

---

## 6. Auth, CSP, and the authenticated browser-WebSocket contract (fixes Codex #3 rev1, #4 rev2, #5 rev3)
> `--with-debug-ui` only gates route registration (http.rs:75-79, cli.rs:22-24); it is NOT access
> control. The dashboard exposes raw bodies, headers, and kill. A browser `WebSocket` CANNOT set
> `Authorization`, so a real session-cookie login flow is required. The existing `/debug` and
> `/debug/ws` (http.rs:77-78) ALSO expose transcripts and are currently unauthenticated — they
> must be protected under the same auth. The server is HTTP-only, so `Secure` cookies require an
> explicit HTTPS-edge contract.

### Protect `/debug` too
- When dashboard auth is in force, `/debug` and `/debug/ws` require the SAME session cookie (or an
  `Authorization: Bearer` fallback). No unauthenticated transcript route remains. (If the operator
  doesn't want to set a token on loopback, the loopback-dev concession below applies to both
  `/dashboard` and `/debug` — but never on non-loopback.)

### Secret configuration (env-only — never in a `Debug + Clone` config struct)
- `LLMCONDUIT_DASHBOARD_TOKEN` (env only): the shared secret. Constant-time compared. Required on
  non-loopback; if unset there, **refuse to register `/dashboard` AND `/debug` routes** at startup
  (hard error). Loopback without a token → allowed with a logged warning (dev).
- `LLMCONDUIT_DASHBOARD_SESSION_KEY` (env only): ≥32 bytes base64 for HMAC-SHA256 cookie signing.
  Auto-generate + log a temporary one on loopback-dev if unset; required on non-loopback. Never
  logged. Rotation: changing it invalidates all sessions (acceptable); document it.
- `LLMCONDUIT_DASHBOARD_PUBLIC_ORIGIN` (env): the exact public origin (e.g.
  `https://argus.example.com`) when served behind an HTTPS edge. On **non-loopback**, a validated
  `https://` `PUBLIC_ORIGIN` is **required** (rev4 #8 — rev4 only warned, which still served creds
  over HTTP). If unset on non-loopback → **refuse to register `/dashboard` + `/debug`** at startup
  (hard error). Override only with an explicit `LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1` (logs a loud
  warning; intended for isolated air-gapped LANs). When set, the login cookie is `Secure` + the WS
  `Origin` allow-list is this exact origin. On loopback, cookie is NOT `Secure`, `Origin`
  allow-list is the served origin. **HTTPS-edge contract:** the binary is HTTP-only; non-loopback
  without a TLS-terminating reverse proxy + `PUBLIC_ORIGIN` is unsupported.

### Login flow + session cookie
- `POST /dashboard/login` (JSON `{token}`): constant-time compare (`subtle::ConstantTimeEq`); on
  success set a cookie `HttpOnly; SameSite=Strict; Secure`-when-`PUBLIC_ORIGIN`; **`Path=/`** (rev4
  #6 — `Path=/dashboard` would stop the browser sending it to `/debug` + `/debug/ws`, and browser
  WebSockets can't use the bearer fallback); `Max-Age=3600`; value =
  `base64url(HMAC-SHA256(key, "{exp}:{nonce}")) + "." + "{exp}:{nonce}"` (signed, stateless — no
  server session table). Response `Cache-Control: no-store`. Failure → 401, no-store, generic error.
- `POST /dashboard/logout`: clear the cookie. **Stateless-session caveat (rev4 #9):** a copied
  cookie remains valid until `exp` (≤1 h) — clearing can't revoke it. This 1 h bearer-window risk
  is documented + bounded by the short `Max-Age`; revocable server-side sessions are a future
  option if needed.
- **Login UI:** `/dashboard` (the SPA shell) is served to unauthenticated clients as a **login
  shell** (the embedded `dist/index.html` detects no session and renders a token-entry form posted
  to `/dashboard/login`); once authenticated, the same shell renders the dashboard. So there is a
  public login surface by design — only that shell + `/dashboard/login` are reachable without auth.

### WebSocket auth (the browser gap)
- `/dashboard/ws` + `/debug/ws` upgrade validates, in order: (1) the signed session cookie (HMAC
  verify); (2) the `Origin` header against the allow-list (served origin, or exact
  `PUBLIC_ORIGIN`). No `Origin` → reject on non-loopback, allow on loopback-dev.
- **Session expiry on a live WS:** each connection records the cookie's `exp`; a per-connection
  tokio timer closes the socket at `exp` (so an established WS can't outlive the cookie). Clients
  re-login + reconnect. (rev3 #5.)

### Mutation gating + CSRF
- **Kill** (`POST /dashboard/api/flows/:id/kill`) requires BOTH
  `LLMCONDUIT_DASHBOARD_ALLOW_MUTATIONS=1` (env; default off → 403) AND a double-submit CSRF token
  (non-HttpOnly cookie + SPA bootstrap echo) in `X-CSRF-Token`, constant-time compared. Replay is
  not registered (deferred). `GET` reads need only the session cookie.

### CSP + security headers (`/dashboard`, `/debug`, static assets)
- `/dashboard` + static assets: `Content-Security-Policy`:
  `default-src 'self'; script-src 'self'; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline';
  img-src 'self' data:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'`.
  (`'unsafe-inline'` for Tailwind until nonced.)
- **`/debug` CSP (rev4 #7):** the existing `src/debug.html` is a single file with an **inline
  module script**, which `script-src 'self'` would block. Two options — pick one at impl: (a)
  externalize the inline script to a `/debug/app.js` asset and keep strict CSP, or (b) serve a
  `/debug`-specific CSP adding `'sha256-<hash-of-inline-script>'` to `script-src` (hash pinned at
  build). Recommended: (a) externalize (cleaner,将来 nonce-friendly).
- `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`,
  `Cache-Control: no-store` on all `/dashboard/api/*` + `/debug` responses. No external CDNs.

### Secrets redaction at capture
- Beyond the existing `redact_ws_message_image_uris` redactor: scrub `Authorization`, `x-api-key`,
  `api-key`, `openai-beta` value tokens, and top-level `api_key` JSON fields before storing
  inbound bodies/headers and before serializing upstream bodies. Redaction happens in the capped
  serializer (§4.2), so secrets never persist even in previews.

---

## 7. Verification
- **Phase 0 Rust:** `cargo test` — existing 579+ suites stay green. New tests:
  - MetricsLayer ring math + p-quantile interpolation; FlowStore single-lock cap/TTL + LRU.
  - `BackendChatRequest` `response_id` + `serving` token **propagation through all production
    rebuilds** (engine.rs:1486, upstream.rs:813, upstream.rs:1110) — cover with a test that
    asserts the leaf receives them (and that `upstream.rs:3105` test helper is excluded).
  - **Per-response serving-token isolation:** two concurrent flows write distinct providers,
    assert no overwrite (the rev2 race regression).
  - on-wire upstream-body capture: assert the stored body equals the POST-`sanitize`/retry body,
    NOT the pre-leaf engine body; `logged_send_chat_request` receives `response_id` explicitly.
  - **TelemetryGuard (state-machine):** an extractor-failure path (axum rejects `Json`) leaves
    the record `OpenL0` and the L0 `Drop` finalizes it (no orphan); a budget-failure (engine.rs:809)
    + a midstream-cancel finalize via L1 (`ClaimedL1→Finalized`); `/v1/completions` opens no record;
    assert the L0/L1 CAS transitions are race-free under concurrency.
  - **Usage (cumulative-aware):** assert each usage-bearing chunk upserts `turn_base + chunk.usage`
    (no double-count), the single post-loop `add` advances `turn_base`, and a midstream-cancel after
    a usage chunk retains the last upserted total (not zero/previous-round).
  - **5 s coordinated snapshot:** snapshot task holds FlowStore+MetricsLayer locks simultaneously
    (fixed order) + captures ONE `Arc<ProviderHealthSnapshot>` → `/snapshot?at=` returns an
    internally-consistent **body-free** frozen cut (distinct `SnapshotFlowSummary`, no `Arc<[u8]>`);
    assert peak memory ≤ ~400 MiB (summaries), **NOT 135 GiB**; bodies live only in the live store
    (inspector body panel reads live, "evicted" if gone); per-domain `{flow_seq,metrics_seq,
    topology_seq,monitor_seq}` cursors on snapshot + all REST/WS responses (no singular `seq`).
  - **provider_health:** `Arc<ProviderMetrics>` keeps `RoutingUpstreamClient: Clone` compiling;
    `Arc<CatalogMeta>` publishes fetched_ms+size coherently (no torn pair); `consecutive_failures`
    resets on success; `Down` = cooling + consecutive ≥ threshold.
  - **AbortHub:** keyed by `api_call_id` (= flow `:id`); kill cancels; finalize removes the entry.
  - **seq:** per-domain `{domain,seq}` cursors; no valid frame discarded.
  - **Auth:** login sets/rejects cookie constant-time (HMAC-SHA256); `/debug` + `/debug/ws` require
    the same cookie; WS rejects bad cookie / cross-origin + closes at cookie expiry; CSRF on kill;
    mutations off → 403; non-loopback without token → startup refuses both `/dashboard` and `/debug`.
  - secrets-redaction in capped serializer.
- **Zero-cost bench (§4.8):** criterion micro-bench, dashboard-off vs disabled-handles path —
  assert no added allocation / no wrapper clone cost. Plus a seq-contention check under load.
- **Frontend (mock, parallel late):** `npm run dev` against the in-browser mock for view R&D.
- **Frontend (real):** `LLMCONDUIT_BUILD_DASHBOARD=1 cargo run -- --with-debug-ui` (token set)
  → `/dashboard` serves the embedded SPA hitting live Rust endpoints.
- **Node-less host:** `cargo build` (no flag) MUST succeed — the stub `$OUT_DIR/dashboard_dist`
  is always present; `include_dir!("$OUT_DIR/dashboard_dist")` compiles.
- **End-to-end:** streamed `/v1/chat/completions` → flow appears in table → 3-pane inspector
  shows inbound/normalized/upstream (post-finalize) bodies → usage populates the Sankey →
  upstream node reflects health → stats strip ticks → time-travel rewinds to a consistent cut →
  kill (mutations enabled + CSRF) aborts a live stream.

---

## 8. Risks (updated through rev3)
- **node-in-build purity break** — opt-in `LLMCONDUIT_BUILD_DASHBOARD`; Rust-only builds node-free;
  `include_dir!("$OUT_DIR/dashboard_dist")` compiles via build.rs stub (rev1 #9, rev2 #5 fixed).
- **d3 + React 18** — isolated imperative effects w/ cleanup, StrictMode-idempotent (rev1 #11).
- **Upstream seam** — `BackendChatRequest` gains `response_id: Option<String>` + `serving:
  Option<Arc<ServingToken>>` (both `Debug`/`Clone`-safe, no `dyn Fn`); per-response allocation
  eliminates the cross-flow race (rev1 #5, rev2 #2). Leaf gets a `flow_store` handle + receives
  `response_id` as an explicit `logged_send_chat_request` param (no scope error, no Debug breakage)
  (rev2 #1, rev3 #1).
- **On-wire body correctness** — captured at the leaf post-finalize (incl. shrink-retry), keyed
  by the explicitly-passed `response_id` (rev1 #1, rev2 #1).
- **Terminal metrics** — state-machine ownership (`OpenL0→ClaimedL1→Finalized` CAS in the record):
  L0 middleware Drop finalizes only if `OpenL0` (catches extractor/conversion failures → no
  orphans; `/v1/completions` not instrumented); L1 engine guard CAS-claims then finalizes on all
  paths; private `Instant` latency; **cumulative-aware usage** — `turn_base + chunk.usage` upserted
  per usage-bearing chunk, single `add` after the loop (no double-count, no lost midstream total)
  (rev1 #2, rev2/3 #6, rev3/4 #4, rev5 #1). Sessions don't outlive the cookie (WS closed at `exp`).
- **FlowStore soundness** — records + order under ONE lock; records are `Arc<FlowRecord>` (COW);
  bodies `Arc<[u8]>` live ONLY in the live store (never shared with snapshots); inference-endpoint
  whitelist (rev2 #7). Bodies capped via a redacting streaming serializer (O(CAP) memory) with a
  live body-byte quota + a separate snapshot-summary quota (rev1 #7, rev2 #9, rev5 #2).
- **Time-travel consistency + memory** — **body-free** immutable 5 s `SnapshotFlowSummary` snapshots
  (kills the 135 GiB worst case — snapshots retain no `Arc<[u8]>`/`FlowRecord`); snapshot task holds
  FlowStore+MetricsLayer locks simultaneously in a fixed order AND captures one versioned
  `Arc<ProviderHealthSnapshot>` for topology → true atomic cut; `/snapshot?at=` returns a frozen cut
  with per-domain cursors (rev2 #3, rev3 #2/#3, rev4 #2/#3/#5, rev5 #2/#3).
- **Auth surface** — env-only secrets + HMAC-SHA256 stateless cookie (constant-time, `Path=/`),
  `/debug` + `/debug/ws` protected, WS cookie+Origin+expiry, CSRF on kill, mutation gate, validated
  `https://` `PUBLIC_ORIGIN` REQUIRED on non-loopback (hard-fail, explicit insecure override),
  login shell, per-route CSP (externalize `/debug` inline script), redaction; 1 h bearer-window
  logout caveat documented (rev1 #3, rev2 #4, rev3 #5, rev4 #6/#7/#8/#9). Replay deferred.
- **provider_health** — `Arc<ProviderMetrics>` preserves derived `Clone`; `Arc<CatalogMeta>` for
  coherent (un-torn) catalog publish; `consecutive_failures` reset on success; `Down` defined
  (rev2 #8, rev3 #8).
- **AbortHub** — keyed by `api_call_id` (= flow `:id`), registered by L1 guard, removed on finalize
  (rev3 #9).
- **seq** — per-domain `{domain,seq}` cursors; no global watermark (rev3 #7).
- **include_dir!** — `static DASHBOARD_DIST: Dir<'static> = include_dir!("$OUT_DIR/dashboard_dist")`
  + build.rs clear/rewrite stub when flag off (rev3 #6).
- **seq contention / zero-cost** — per-domain seq (not global), benchmarked; disabled path
  benchmarked allocation/clone-free (rev2 #10).
- **7-day cron** — the polling cron that deferred this run is deleted (done).
