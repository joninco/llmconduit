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
import { useEffect, useRef, useState } from 'react';
import { RadialTopology, type TopoHover } from '../components/viz/RadialTopology';
import { useDashboard } from '../store/hooks';
import { flowFilterStore } from '../store/flowFilterStore';
import { navigate } from '../router/useHashRoute';
import { Panel } from '../components/ui/Panel';
import { CooldownTooltip } from '../components/viz/CooldownTooltip';

export function TopologyView() {
  const nodes = useDashboard((s) => s.topologyNodes);
  const edges = useDashboard((s) => s.topologyEdges);
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const [hover, setHover] = useState<TopoHover | null>(null);
  // Pin the live tick so a cooldown countdown in the tooltip refreshes once a second.
  const [, force] = useState(0);
  const tickRef = useRef(force);
  tickRef.current = force;
  useEffect(() => {
    if (!hover) return;
    const id = window.setInterval(() => tickRef.current((n) => n + 1), 1000);
    return () => window.clearInterval(id);
  }, [hover]);

  function onSelectUpstream(id: string): void {
    // Filter the FlowTable to this upstream target, then jump to the flows view so the cross-link
    // lands on the already-filtered table. The store toggle clears it on a repeat click.
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
      <Panel className="flex min-h-0 flex-1 items-center justify-center overflow-auto p-4">
        {nodes.length === 0 ? (
          <p className="text-sm text-text-muted" data-testid="topology-empty">No providers reporting yet.</p>
        ) : (
          <RadialTopology nodes={nodes} edges={edges} onSelectUpstream={onSelectUpstream} onHover={setHover} />
        )}
      </Panel>
      {hover && <CooldownTooltip hover={hover} />}
    </div>
  );
}
