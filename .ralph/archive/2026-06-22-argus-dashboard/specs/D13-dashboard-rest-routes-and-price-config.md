# D13 — Dashboard REST routes + price-table config

> **Source:** DASHBOARD_PLAN.md rev 8 §5. Topic 13. The contract surface tying the stores to the SPA.

**Priority:** HIGH · **Surface:** `src/http.rs` (route registration in the `--with-debug-ui` block),
new `src/dashboard_api.rs` handlers, `src/config.rs` (price table), `src/lib.rs`

## Purpose
Wire the axum REST endpoints the SPA consumes, returning per-domain seq cursors (no singular `seq`) and
shapes the frontend (D9-D12) + auth (D7) expect. Plus the per-model price table (Sankey cost coloring
+ cost_per_min). This is the integration point that makes Phase 0's data reachable by the UI.

## Jobs to Be Done
Register (inside the existing `if options.with_debug_ui` block, http.rs:75), all behind D7 auth:
- `GET /dashboard` → `dashboard_index` → SPA shell (D8 embedded `dist/index.html`).
- `GET /dashboard/api/flows?status=&model=&upstream=&page=&limit=` → `dashboard_flows` →
  `{flows:[FlowSummary], total, flow_seq}` from `DashboardFlowStore::list(filter)` (D1).
- `GET /dashboard/api/flows/:id` → `dashboard_flow_detail` → `{flow_seq, inbound_body,
  inbound_headers, normalized, upstream_body, model_requested, model_served, upstream_target, usage,
  deltas:[…], terminal_reason, started_ms, elapsed_ms}` (D1/D2/D3; deltas replayed from
  `MonitorHub::snapshot()` filtered by `response_id`).
- `GET /dashboard/api/metrics` → `dashboard_metrics` → `{metrics_seq, reqs_per_sec, active_streams,
  error_pct, p50,p95,p99, tokens_per_sec, cost_per_min, windows:{m1,m5,h1}}` (D5).
- `GET /dashboard/api/topology` → `dashboard_topology` → `{topology_seq, nodes:[ProviderHealth…],
  edges:[{from,to,throughput,tokens_per_sec,cost_per_sec}]}` + the price table (D4 + price config).
- `GET /dashboard/api/catalog` → `dashboard_catalog` → `[{id,context_limit}]` via
  `Gateway::upstream_model_catalog`.
- `GET /dashboard/api/snapshot?at=<unix_ms>` → `dashboard_snapshot` →
  `{cursors:{flow_seq,metrics_seq,topology_seq,monitor_seq}, at_ms, summaries:[SnapshotFlowSummary],
  metrics, topology}` body-free frozen cut (D5).
- `POST /dashboard/api/flows/:id/kill` → `dashboard_flow_kill` (D6, behind D7 mutation+CSRF gate).
- **Replay is NOT registered** (deferred, plan §6).
- Handlers get `State(Arc<Gateway>)`; `:id` = `api_call_id`.

**Price table (config, additive):** `pub price_table: HashMap<String, ModelPrice{input_per_1k:
f64, output_per_1k: f64, cached_per_1k: f64}>` on `Config` (src/config.rs), from YAML `price_table:`
map + env `LLMCONDUIT_PRICE_TABLE_JSON` (mirror the existing `upstream_chat_kwargs` env pattern).
`Gateway::price_for(model)` accessor; flow detail computes `cost = usage × price`; `/topology`
returns the table; `cost_per_min`/`cost_per_sec` roll up across flows (D5).

## Acceptance criteria
- [ ] All routes registered in the `if with_debug_ui` block (http.rs:75); off → none registered (test).
- [ ] `dashboard_flows` honors `status`/`model`/`upstream`/`page`/`limit`; returns `flow_seq`.
- [ ] `dashboard_flow_detail` returns all 3 bodies (D1 inbound/normalized, D2 on-wire upstream) +
      deltas (replayed from MonitorHub snapshot filtered by response_id) + usage + terminals; missing
      body fields absent (not error) when evicted.
- [ ] `dashboard_metrics` returns per-window tiles + `cost_per_min` + `metrics_seq`.
- [ ] `dashboard_topology` returns nodes/edges + price table + `topology_seq`.
- [ ] `dashboard_snapshot` returns the body-free cut with per-domain `cursors`; `snapshot_at(ts)`
      nearest ≤5 s.
- [ ] `dashboard_flow_kill` honors D7 mutation + CSRF gate (403 otherwise); D6 cancel.
- [ ] Price table loaded from YAML + `LLMCONDUIT_PRICE_TABLE_JSON` env; `Gateway::price_for` used for
      flow `cost` + `cost_per_min`.
- [ ] Cursor-bearing responses (`/flows`, `/flows/:id`, `/metrics`, `/topology`, `/snapshot`) carry
      their per-domain `seq` (flow_seq / metrics_seq / topology_seq). `/catalog` returns the BARE
      array `[{id,context_limit}]` (no cursor — it's a static-ish catalog read, not a mutating domain).
- [ ] `no-store` + auth (D7) enforced on all `/dashboard/api/*`.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D1 (FlowStore), D2 (upstream body), D3 (usage/deltas), D4 (topology), D5
  (metrics/snapshots), D6 (kill), D7 (auth/CSRP gate), D8 (shell serving). This task is the capstone
  that makes Phase 0 data reachable — schedule AFTER D1-D8.
- **Extends:** `src/http.rs` route block, new `src/dashboard_api.rs`, `src/config.rs` price table,
  `src/lib.rs` (price_table threaded to Gateway).
- **New APIs:** the handlers above + `Gateway::price_for`.

## Constraints
- `:id` = `api_call_id` (matches D6 AbortHub key + D1 store key).
- Per-domain seq cursors only (rev5/rev6 fix — no singular `seq`).
- Replay deferred; kill is the only mutation.
- Reuse `Gateway` state; no new shared state beyond D1-D7.

## Out of scope
- The store/metric implementations (D1/D5) — this wires handlers to them.
- Auth enforcement mechanics (D7) — this trusts D7's gate.
- Frontend (D9-D12).

## Definition of done
- [ ] Acceptance criteria green; end-to-end (a streamed request → flow in table → 3-pane inspector →
      usage → topology → stats → time-travel → kill) works against the real backend; Codex-xhigh APPROVED.
