# D4 — provider_health accessor + topology publication

> **Source:** DASHBOARD_PLAN.md rev 8 §4.6. Topic 13.

**Priority:** MEDIUM · **Surface:** `src/upstream.rs` (trait, leaf/failover/routing), `src/engine.rs`
(`Gateway::upstream_health`), `src/lib.rs`

## Purpose
Expose per-upstream health + counters for the topology map, lock-free and dyn-safe, without breaking
the derived `Clone` on the upstream structs or tearing the (fetched_ms,size) catalog pair. Fixes Codex:
bare `AtomicU64`/`AtomicUsize` fields break `#[derive(Debug, Clone)]` on `RoutingUpstreamClient`
(upstream.rs:340)/`FailoverUpstreamClient` (upstream.rs:212); two atomics tear the catalog metadata
pair; `failover_count` cumulative can't express "consecutive failures"; a sync method can't lock the
`AsyncMutex<catalog>` (upstream.rs:343); per-`served_count` republish allocates O(providers)/request
and an idle cooling provider would never flip→Healthy.

## Jobs to Be Done
- Add non-async default trait method `UpstreamClient::provider_health(&self) -> Vec<ProviderHealth>`
  (upstream.rs:85, mirrors `supported_model_catalog` default at :115) — dyn-safe with
  `Arc<dyn UpstreamClient>` (lib.rs:123/engine.rs:529).
- Behind `Arc<ProviderMetrics>` per provider (structs keep derived `Clone`): cumulative
  `served_count`/`failover_count` + a `consecutive_failures` reset to 0 at `mark_provider_success`
  (upstream.rs:877), bumped in `mark_failure`. Catalog metadata behind a single
  `Arc<CatalogMeta { fetched_ms, size }>` swapped atomically inside `refresh_catalog` (under the
  existing `AsyncMutex` hold) → no torn pair.
- `ProviderHealth` is an owned serializable DTO (epoch-ms, not `Instant`): `{id, name, route,
  base_url, status: Healthy|Cooling|Down, cooling_until_ms, last_error, served_count, failover_count,
  consecutive_failures, catalog_fetched_ms, catalog_size}`.
- `Down` semantics: `Cooling` = `cooling_until > now`; `Down` = `Cooling` AND
  `consecutive_failures >= DOWN_THRESHOLD` (default 3); `Healthy` = neither.
- `FailoverUpstreamClient::provider_health()` reads `self.states` (upstream.rs:212/437) + its
  `Arc<ProviderMetrics>`; `RoutingUpstreamClient` overrides to aggregate per nested provider with
  `route = Some(route_id)` + the `Arc<CatalogMeta>`. `Gateway::upstream_health()` exposes it.
- **Publication cadence:** publish an immutable versioned `Arc<ProviderHealthSnapshot>` on a coalesced
  1 s tick (NOT per `served_count`) and wake the publisher at the next `cooling_until` deadline (so an
  idle cooling→Healthy flip happens with no traffic). Atomics update continuously; the snapshot reads
  them at tick time. WS `TopologyUpdate` piggybacks the 1 s cadence (D7 envelope).

## Acceptance criteria
- [ ] `UpstreamClient::provider_health()` default `Vec::new()` added (upstream.rs:85); `Arc<dyn
      UpstreamClient>` still constructs at lib.rs:123 (dyn-safety verified by compiling).
- [ ] `RoutingUpstreamClient`/`FailoverUpstreamClient` still `#[derive(Debug, Clone)]` (no bare atomics
      on the structs — behind `Arc<ProviderMetrics>`/`Arc<CatalogMeta>`).
- [ ] `Arc<CatalogMeta {fetched_ms,size}` swapped under the existing AsyncMutex in `refresh_catalog`;
      `provider_health()` reads it lock-free (no torn `(fetched_ms,size)` pair — test).
- [ ] `served_count`/`failover_count` cumulative; `consecutive_failures` resets on
      `mark_provider_success` (upstream.rs:877), bumped on `mark_failure`.
- [ ] `Down` = `Cooling` + `consecutive_failures >= 3` (configurable `DOWN_THRESHOLD`); test нагревает
      a provider to Down then succeeds once → `Healthy` (or `Cooling` cleared).
- [ ] Coalesced 1 s publication + cooldown-deadline wake: test that an idle cooling provider flips to
      Healthy at `cooling_until` with zero traffic (advance a tokio mock clock).
- [ ] `Gateway::upstream_health()` returns the `ProviderHealth` vec; routing override sets `route`.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Depends on:** D2 (serving token `{route,provider}` written at success = source of attribution).
- **Extends:** `UpstreamClient` trait (additive default method), leaf/failover/routing, `Gateway`.
- **New APIs:** `provider_health()`, `ProviderHealth`, `ProviderMetrics`, `CatalogMeta`,
  `ProviderHealthSnapshot`, `Gateway::upstream_health()`.
- **Consumed by:** D5 (topology captured in the 5 s snapshot via ONE `Arc<ProviderHealthSnapshot>`),
  D7 (`TopologyUpdate` WS frame, cadence-aligned), D12 (topology view), D13 (`/topology` route).

## Constraints
- Non-async default method only (no existing signature change); dyn-compatible.
- Catalog metadata publish must not block the hot path (`refresh_catalog` already holds the
  `AsyncMutex`); the swap is a single `Arc` store.
- AGENTS.md hard rules preserved: failover pre-first-chunk only; routing providers not failover
  fallbacks. `mark_provider_success` semantics unchanged (clears cooldown).

## Out of scope
- The 5 s atomic snapshot capture (D5) — D4 publishes; D5 captures one `Arc`.
- REST `/topology` route (D13); frontend topology view (D12).
- Price table (D13) — `/topology` returns it but the config field is added in D13.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
