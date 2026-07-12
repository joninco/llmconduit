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
import { useDashboard, useFlowFilter } from '../store/hooks';
import { topologyProviderKey, useTopologyQuery } from '../store/useTopologyQuery';
import { flowFilterStore } from '../store/flowFilterStore';
import { useFlowRows } from '../components/FlowTable/useFlowRows';
import { navigate } from '../router/useHashRoute';
import { Panel } from '../components/ui/Panel';
import { CooldownTooltip } from '../components/viz/CooldownTooltip';
import { StaleFallbackBanner } from '../components/StaleFallbackBanner';
import { fmtLatency, fmtPercent, fmtSamples } from '../components/FlowTable/format';
import { clientRollup } from '../components/FlowTable/clientAttribution';

/** U9 — client nodes rendered in the compact topology; the rest fold into a counted note. */
const MAX_CLIENT_NODES = 5;

export function TopologyView() {
  // Seed nodes/edges/prices from `/topology` (LIVE-only; never overwrites a seek cut) — finding 5.
  // Gap 13: it ALSO returns the per-provider latency/error map off the LIVE REST data (the
  // authoritative per-provider source live; the WS topology frame carries `per_provider` ABSENT).
  const { perProviderById, engineMetricsById, loadState, retry } = useTopologyQuery();
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
  const engineMetricsFor = (node: (typeof nodes)[number]) =>
    engineMetricsById[topologyProviderKey(node)] ?? node.engine_metrics;
  const perProviderByNode = useMemo<Record<string, ProviderLatency>>(() => {
    const map: Record<string, ProviderLatency> = {};
    for (const n of nodes) {
      const per = perProviderById[n.id] ?? n.per_provider;
      if (per) map[n.id] = per;
    }
    return map;
  }, [nodes, perProviderById]);
  const staleAsOfMs = Object.values(perProviderByNode)
    .filter((metrics) => metrics.stale && metrics.as_of_ms != null)
    .reduce<number | null>((latest, metrics) => Math.max(latest ?? 0, metrics.as_of_ms!), null);

  function onSelectUpstream(id: string): void {
    // Filter the FlowTable to this upstream target, then jump to the flows view so the cross-link
    // lands on the already-filtered table. The setter SETS the facet deterministically (finding 10).
    flowFilterStore.getState().setUpstream(id);
    navigate('flows');
  }

  // U9 — the caption promises "client → gateway → upstream providers"; render the client side.
  // Population (R2): the SCOPED loaded rows — the same filter-aware `useFlowRows` merge the
  // ScopeBar counts — so a model/upstream-filtered topology shows that scope's clients, not the
  // whole retained map. (BucketKey has no client dimension, so there is no server-windowed
  // client rate to lie with; the column labels its population explicitly.)
  const filters = useFlowFilter((s) => s.filters);
  const { rows: scopedRows } = useFlowRows(filters);
  const clientNodes = useMemo(() => {
    const rollup = clientRollup(scopedRows);
    return { rows: rollup.rows.slice(0, MAX_CLIENT_NODES), hidden: Math.max(0, rollup.rows.length - MAX_CLIENT_NODES), totalFlows: rollup.totalFlows };
  }, [scopedRows]);

  function onSelectClient(label: string): void {
    flowFilterStore.getState().setClient(label);
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
      {!seeking && staleAsOfMs !== null && <StaleFallbackBanner asOfMs={staleAsOfMs} surface="topology" />}
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
        ) : nodes.length <= 3 ? (
          <div className="flex w-full flex-wrap items-stretch justify-center gap-3" data-testid="compact-topology">
            {clientNodes.rows.length > 0 && (
              <>
                <ClientNodesColumn nodes={clientNodes} onSelect={onSelectClient} />
                <div className="flex items-center text-xl text-text-muted" aria-hidden>→</div>
              </>
            )}
            <div className="flex min-w-40 items-center justify-center rounded-md border border-accent/40 bg-accent/10 px-4 py-3 text-center">
              <div><div className="text-[10px] uppercase tracking-[0.14em] text-text-muted">gateway</div><div className="mt-1 font-mono text-sm text-text">llmconduit</div></div>
            </div>
            <div className="flex items-center text-xl text-text-muted" aria-hidden>→</div>
            {nodes.map((node) => {
              const health = perProviderFor(node.id);
              const freshAt = health?.as_of_ms ?? node.catalog_fetched_ms;
              return (
                <button
                  key={node.id}
                  type="button"
                  className="min-w-56 rounded-md border border-line bg-panel-raised p-3 text-left hover:border-accent focus-visible:ring-2 focus-visible:ring-accent"
                  onClick={() => onSelectUpstream(node.id)}
                  data-testid="compact-provider"
                  data-node-id={node.id}
                >
                  <div className="flex items-center justify-between gap-2"><strong className="truncate text-sm text-text">{node.name}</strong><span className="text-[10px] uppercase text-text-muted">{node.status}</span></div>
                  <dl className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-[11px]">
                    <dt className="text-text-muted">Attempt population</dt><dd className="text-right tabular-nums">{health ? fmtSamples(health.samples, 'attempt') : '—'}</dd>
                    <dt className="text-text-muted">Provider-attempt P95</dt><dd className="text-right tabular-nums">{fmtLatency(health?.p95 ?? null)}</dd>
                    <dt className="text-text-muted">Attempt errors</dt><dd className="text-right tabular-nums">{health ? fmtPercent(health.error_rate) : '—'}</dd>
                    <dt className="text-text-muted">Freshness</dt><dd className="text-right tabular-nums">{freshAt ? fmtElapsedAge(clock - freshAt) : '—'}</dd>
                  </dl>
                </button>
              );
            })}
          </div>
        ) : (
          /* R2/U9: the client side of the story renders in the radial (4+ providers) branch
             too — same column, beside the graph. */
          <div className="flex min-h-0 w-full items-stretch gap-3">
            {clientNodes.rows.length > 0 && <ClientNodesColumn nodes={clientNodes} onSelect={onSelectClient} />}
            <div className="min-h-0 min-w-0 flex-1">
              <RadialTopology
                nodes={nodes}
                edges={edges}
                perProvider={perProviderByNode}
                onSelectUpstream={onSelectUpstream}
                onHover={setHover}
              />
            </div>
          </div>
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
                <th className="px-2 py-1.5 text-right" scope="col">Engine</th>
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
                    <td className="px-2 py-1 text-right tabular-nums">{health?.p95 != null ? `${Math.round(health.p95)} ms` : '—'}</td>
                    <td className="px-2 py-1 text-right tabular-nums">{health ? `${health.error_rate.toFixed(1)}%` : '—'}</td>
                    <td className="px-2 py-1 text-right tabular-nums">{engineMetricsFor(node)?.status ?? '—'}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {hover && hoverHealth && (
        <CooldownTooltip health={hoverHealth} x={hover.x} y={hover.y} nowMs={clock} perProvider={perProviderFor(hover.id)} engineMetrics={engineMetricsFor(hoverHealth)} />
      )}
    </div>
  );
}

/**
 * U9 — the client side of "client → gateway → providers". One button per (capped) client from
 * the SCOPED loaded rows, heaviest first, with share, weak-UA marker, and a client-filter
 * cross-link; the remainder folds into a counted note.
 */
function ClientNodesColumn({
  nodes,
  onSelect,
}: {
  nodes: { rows: ReturnType<typeof clientRollup>['rows']; hidden: number; totalFlows: number };
  onSelect: (label: string) => void;
}) {
  return (
    <div className="flex shrink-0 flex-col justify-center gap-2" data-testid="topology-clients">
      {nodes.rows.map((client) => (
        <button
          key={client.key}
          type="button"
          className="w-56 min-w-0 rounded-md border border-line bg-panel-raised px-3 py-2 text-left hover:border-accent focus-visible:ring-2 focus-visible:ring-accent"
          onClick={() => onSelect(client.key)}
          data-testid="topology-client"
          data-client={client.key}
          title={`Show flows for client ${client.label} (${client.total} scoped loaded flows)`}
        >
          <div className="flex min-w-0 items-center justify-between gap-2">
            {/* min-w-0 + fixed card width (R2): a 4 KiB UA label must ellipsize, not widen
                the whole topology. */}
            <span className={`min-w-0 flex-1 truncate font-mono text-xs text-text${client.weak ? ' italic' : ''}`}>{client.label}</span>
            {/* Weak UA attribution carries the same explicit marker as the flow table and the
                by-client roll-up — italics alone read as styling, not as "spoofable". */}
            {client.weak && (
              <span
                className="shrink-0 rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase tracking-wide text-status-cooling"
                data-testid="topology-client-weak"
                title="weak User-Agent fallback — spoofable, NOT a confirmed identity"
              >
                ua
              </span>
            )}
            <span className="shrink-0 text-[10px] tabular-nums text-text-muted">
              {client.total}{nodes.totalFlows > 0 ? ` · ${Math.round((client.total / nodes.totalFlows) * 100)}%` : ''}
            </span>
          </div>
        </button>
      ))}
      {nodes.hidden > 0 && (
        <span className="text-center text-[10px] text-text-muted" data-testid="topology-clients-hidden">
          +{nodes.hidden} more client{nodes.hidden === 1 ? '' : 's'} · see BY CLIENT on Flows
        </span>
      )}
      <span className="text-center text-[9px] uppercase tracking-[0.14em] text-text-muted">clients · scoped loaded flows</span>
    </div>
  );
}

function fmtElapsedAge(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '—';
  if (ms < 60_000) return `${Math.max(0, Math.round(ms / 1000))} s ago`;
  return `${Math.round(ms / 60_000)} min ago`;
}
