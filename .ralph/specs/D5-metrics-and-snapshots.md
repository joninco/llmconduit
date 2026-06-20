# D5 — MetricsLayer (ring buffers + histograms) + coordinated body-free snapshots

> **Source:** DASHBOARD_PLAN.md rev 8 §4.1. Topic 13.

**Priority:** HIGH (stats strip + time-travel depend on it) · **Surface:** new `src/metrics.rs`,
`src/engine.rs`/`src/http.rs` (record calls), `src/lib.rs` (construction), `src/dashboard_flow.rs`
(D1 `snapshot_summaries`)

## Purpose
Authoritative aggregated stats + a memory-safe, internally-consistent time-travel store. Fixes Codex
blockers: independent live-store reads are not an atomic cut; a `SnapshotMutex` writers skip is
ineffective; snapshots retaining body `Arc<[u8]>` recreate the 135 GiB worst case
(720×512×3×128 KiB); a singular `seq` can't dedup a multi-store cut. Reuses MonitorHub ONLY for
transcript/streaming-delta broadcast (not as a store).

## Jobs to Be Done
- `MetricsLayer`: per-window ring buffers 1m/5m/1h (60/300/3600 slots @1 s). Buckets keyed
  `{status_class, model, endpoint, upstream}`. Latency histogram 30 log-spaced buckets 1 ms..120 s;
  p50/p95/p99 via linear interpolation over cumulative in-window counts. Tokens in/out/cached/reasoning
  summed per slot. `record_response(api_call_id, status, served_model, elapsed_ms, upstream)` from the
  engine terminal path (D3 guard finalize) — NOT middleware headers.
- `record_usage(api_call_id, cumulative_usage)` from D3's incremental upsert.
- **5 s coordinated snapshots:** a background task takes ONE critical section holding FlowStore mutex
  THEN MetricsLayer mutex (fixed lock ORDER) in a single block, reads both, captures ONE
  `Arc<ProviderHealthSnapshot>` (D4) for topology, assembles an immutable body-free
  `DashboardSnapshot`, releases. True atomic cut across all three stores; brief (pointer + scalar
  copies, no body copy); runs every 5 s. Lock order is documented; only the snapshot task holds >1 lock
  → no deadlock.
- Each snapshot holds a distinct body-free `SnapshotFlowSummary { api_call_id, response_id, method,
  uri, model_requested, model_served, upstream_target, usage, status, started_ms, elapsed_ms,
  terminal_reason }` — NO `Arc<[u8]>`, NO reference into the live store. Worst case ≈
  720 × 512 × <1 KiB ≈ 360 MiB of SUMMARIES, bounded by a **snapshot-summary quota** (e.g. 400 MiB).
- `SnapshotRing` retains 720 body-free snapshots (1 h). `DashboardSnapshot` carries **per-domain
  cursors** `{flow_seq, metrics_seq, topology_seq, monitor_seq}`.
- `snapshot_at(ts)` returns nearest ≤5 s slot. The live store body-byte quota (D1) is independent; the
  inspector body panel for a rewind-ed flow reads LIVE (shows "body evicted" if gone).
- Per-store seq: MetricsLayer + FlowStore each keep an internal `seq`; WS frames + REST responses use
  per-domain cursors (no global watermark — D7/D13).

## Acceptance criteria
- [ ] `src/metrics.rs`: `MetricsLayer` with ring buffers + histograms; p-quantile interpolation unit
      tests (known samples → expected p50/p95/p99 within tolerance).
- [ ] `record_response`/`record_usage` called from D3's terminal guard finalize (NOT middleware; test
      asserts a streamed request populates metrics only at finalize).
- [ ] 5 s snapshot task holds FlowStore+MetricsLayer locks simultaneously in fixed order + captures ONE
      `Arc<ProviderHealthSnapshot>` (D4); a test asserts `/snapshot?at=` is internally consistent
      (summaries/metrics/topology match at the cut instant; no torn reads).
- [ ] Snapshots are body-free: `DashboardSnapshot` contains `SnapshotFlowSummary` (no `Arc<[u8]>`); a
      test asserts peak snapshot-ring memory is ≤ ~400 MiB under churn, NOT 135 GiB (simulate 720
      snapshots × 512 flows × 128 KiB bodies live-evicted; assert snapshot ring holds no body refs).
- [ ] `DashboardSnapshot` carries per-domain cursors `{flow_seq,metrics_seq,topology_seq,monitor_seq}`;
      `snapshot_at(ts)` returns nearest ≤5 s slot.
- [ ] Lock-order test: no deadlock under concurrent FlowStore mutation + snapshot task (stress test).
- [ ] Zero-cost disabled path: `MetricsLayer::disabled()` early-returns; a criterion bench asserts a
      streaming request with dashboard off vs D1 disabled handles adds no allocation and no added clone
      cost; also bench per-domain seq updates under load (not a hot-spot).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` + `cargo bench` (criterion
      dev-dep) · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D1 (`DashboardFlowStore.snapshot_summaries`, body-free summaries), D3
  (`record_response`/`record_usage` call sites), D4 (`Arc<ProviderHealthSnapshot>`).
- **Extends:** new `src/metrics.rs`; `src/lib.rs` construction (built in the `with_debug_ui` branch);
  `Gateway::new` gains `MetricsLayer` (additive to current 8 params).
- **New APIs:** `MetricsLayer`, `DashboardSnapshot`, `SnapshotFlowSummary`, `SnapshotRing`,
  `snapshot_at`. `SnapshotRing`/`MetricsLayer` have `Disabled` stubs (D8/lib.rs).
- **Consumed by:** D7 (`/snapshot` route + `metric_tick` frame), D11 (stats strip + scrubber), D13
  (`/metrics` + `/snapshot` routes).

## Constraints
- In-memory ring buffers only (no `metrics` crate) — match repo idiom.
- Lock order FlowStore→MetricsLayer is mandatory for any code holding both (only the snapshot task).
- Bodies NEVER on snapshots (the 135 GiB fix). The live body-byte quota (D1) + snapshot-summary quota
  bound ALL dynamic allocations.
- `MonitorHub` stays transcript-only (D3 usage rides it for the monitor domain but bodies/metrics are
  NOT derived from it).

## Out of scope
- WS `metric_tick` frame shape (D7); REST `/metrics` + `/snapshot` (D13).
- Frontend stats strip + scrubber (D11).

## Definition of done
- [ ] Acceptance criteria green; zero-cost bench passes; Codex-xhigh APPROVED.
