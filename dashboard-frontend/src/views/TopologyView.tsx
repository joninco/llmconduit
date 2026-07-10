/**
 * TopologyView (D12) — the routing-story screen. Renders the `RadialTopology` (d3-force radial
 * hub-and-spoke) from the live store topology, health-colored from D4 `ProviderHealth`. Clicking a
 * provider node cross-links to the FlowTable filtered to that upstream (shared filter store +
 * navigation). Hovering a node shows a tooltip with its cooldown countdown / last error / failover
 * count / p-stats-adjacent counters.
 *
 * Data source: `topologyNodes`/`topologyEdges` come straight from `dashboardStore`. While SEEKING
 * (D11), those slices ARE the frozen snapshot cut (`applySeekCut` installed them), so the topology
 * renders the historical state with NO extra wiring here — we just consume the store. We surface a
 * small "historical" affordance while seeking so the frozen state is not mistaken for live.
 */
import { useEffect, useMemo, useState } from 'react';
import { RadialTopology, type TopoHover } from '../components/viz/RadialTopology';
import type { ProviderLatency } from '../api/types';
import { useDashboard } from '../store/hooks';
import { useTopologyQuery } from '../store/useTopologyQuery';
import { flowFilterStore } from '../store/flowFilterStore';
import { navigate } from '../router/useHashRoute';
import { Panel } from '../components/ui/Panel';
import { CooldownTooltip } from '../components/viz/CooldownTooltip';

export function TopologyView() {
  // Seed nodes/edges/prices from `/topology` (LIVE-only; never overwrites a seek cut) — finding 5.
  // Gap 13: it ALSO returns the per-provider latency/error map off the LIVE REST data (the
  // authoritative per-provider source live; the WS topology frame carries `per_provider` ABSENT).
  const { perProviderById, loadState, retry } = useTopologyQuery();
  const nodes = useDashboard((s) => s.topologyNodes);
  const edges = useDashboard((s) => s.topologyEdges);
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const [hover, setHover] = useState<TopoHover | null>(null);
  // The cooldown countdown clock: live it ticks once a second (refreshing the tooltip); while
  // SEEKING it is FROZEN to `seekAtMs` so the historical view does not advance into the future
  // (finding 1) — and the live timer is disabled so a frozen tooltip never re-renders forward.
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    if (!hover || seeking) return; // no live tick while seeking — the clock is frozen below.
    setNowMs(Date.now());
    const id = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [hover, seeking]);
  // While seeking the countdown is measured against the frozen cut instant, never the wall clock.
  const clock = seeking ? (seekAtMs ?? nowMs) : nowMs;
  // Re-resolve the hovered provider's CURRENT health by id on each render (finding 7): a streaming
  // health update reflects in an open tooltip, and if the provider was removed the tooltip closes.
  const hoverHealth = hover ? nodes.find((n) => n.id === hover.id) ?? null : null;

  // Gap 13: resolve a node's per-provider metrics from the REST/snapshot path (NOT the live WS
  // topology frame, which carries `per_provider` ABSENT). LIVE: the REST query map
  // (`perProviderById`, the stable source unclobbered by WS frames). SEEKING / snapshot: the store
  // node's OWN `per_provider`, which the `/snapshot` reshape (and an initial-snapshot seed)
  // populates — `perProviderById` is empty while seeking. Absent in both ⇒ undefined ⇒ the tile
  // renders `—` (no in-window samples; don't-lie-with-zeros). The per-node map is memoized so the
  // RadialTopology emphasis doesn't churn on unrelated renders.
  const perProviderFor = (id: string): ProviderLatency | null | undefined =>
    perProviderById[id] ?? nodes.find((n) => n.id === id)?.per_provider ?? undefined;
  const perProviderByNode = useMemo<Record<string, ProviderLatency>>(() => {
    const map: Record<string, ProviderLatency> = {};
    for (const n of nodes) {
      const per = perProviderById[n.id] ?? n.per_provider;
      if (per) map[n.id] = per;
    }
    return map;
  }, [nodes, perProviderById]);

  function onSelectUpstream(id: string): void {
    // Filter the FlowTable to this upstream target, then jump to the flows view so the cross-link
    // lands on the already-filtered table. The setter SETS the facet deterministically (finding 10).
    flowFilterStore.getState().setUpstream(id);
    navigate('flows');
  }

  return (
    <div className="relative flex min-h-0 min-w-0 flex-1 flex-col p-4" data-testid="topology-view">
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Topology</h2>
        <p className="text-sm text-text-muted">client → gateway → upstream providers · click a node to filter flows</p>
        {seeking && (
          <span
            className="ml-auto rounded-sm border border-status-cooling/40 bg-status-cooling/10 px-2 py-0.5 text-[11px] text-status-cooling"
            data-testid="topology-historical"
          >
            historical snapshot
          </span>
        )}
      </header>
      {loadState === 'error' && nodes.length > 0 && !seeking && (
        <div className="mb-3 flex items-center gap-3 rounded border border-status-cooling/40 bg-status-cooling/10 px-3 py-2 text-xs" role="alert" data-testid="topology-stale">
          <span>Provider topology could not refresh. Showing the last available state.</span>
          <button type="button" className="ml-auto text-accent underline" onClick={retry}>Retry</button>
        </div>
      )}
      <Panel className="flex min-h-0 flex-1 items-center justify-center overflow-auto p-4">
        {nodes.length === 0 ? (
          loadState === 'loading' && !seeking ? (
            <p className="text-sm text-text-muted" role="status" data-testid="topology-loading">Loading provider topology…</p>
          ) : loadState === 'error' && !seeking ? (
            <div className="text-center text-sm text-status-down" role="alert" data-testid="topology-error">
              <p>Provider topology could not be loaded.</p>
              <button type="button" className="mt-2 text-accent underline" onClick={retry}>Retry</button>
            </div>
          ) : (
            <p className="text-sm text-text-muted" data-testid="topology-empty">
              {seeking ? 'No providers in this historical snapshot.' : 'No providers configured or reporting.'}
            </p>
          )
        ) : (
          <RadialTopology
            nodes={nodes}
            edges={edges}
            perProvider={perProviderByNode}
            onSelectUpstream={onSelectUpstream}
            onHover={setHover}
          />
        )}
      </Panel>
      {nodes.length > 0 && (
        <div className="mt-3 max-h-40 shrink-0 overflow-auto rounded border border-line" data-testid="topology-companion-table">
          <table className="w-full text-left text-xs">
            <caption className="sr-only">Provider topology and global attempt health</caption>
            <thead className="sticky top-0 bg-panel text-text-muted">
              <tr>
                <th className="px-2 py-1.5" scope="col">Provider</th>
                <th className="px-2 py-1.5" scope="col">Status</th>
                <th className="px-2 py-1.5 text-right" scope="col">Attempts</th>
                <th className="px-2 py-1.5 text-right" scope="col">p95</th>
                <th className="px-2 py-1.5 text-right" scope="col">Errors</th>
              </tr>
            </thead>
            <tbody>
              {nodes.map((node) => {
                const health = perProviderFor(node.id);
                return (
                  <tr key={node.id} className="border-t border-line">
                    <th className="px-2 py-1" scope="row">
                      <button type="button" className="rounded text-accent underline-offset-2 hover:underline" onClick={() => onSelectUpstream(node.id)}>
                        {node.name}
                      </button>
                    </th>
                    <td className="px-2 py-1">{node.status}</td>
                    <td className="px-2 py-1 text-right tabular-nums">{health?.samples ?? '—'}</td>
                    <td className="px-2 py-1 text-right tabular-nums">{health ? `${Math.round(health.p95)} ms` : '—'}</td>
                    <td className="px-2 py-1 text-right tabular-nums">{health ? `${health.error_rate.toFixed(1)}%` : '—'}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {hover && hoverHealth && (
        <CooldownTooltip health={hoverHealth} x={hover.x} y={hover.y} nowMs={clock} perProvider={perProviderFor(hover.id)} />
      )}
    </div>
  );
}
