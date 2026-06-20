/**
 * Placeholder views for the four routes. These are intentionally thin (D9 scope =
 * scaffold only; the real views are D10-D12). Each renders its route name + a small
 * live readout from the stores so `npm run dev` shows the plumbing working end-to-end.
 */
import { Panel } from '../components/ui/Panel';
import { useDashboard } from '../store/hooks';

function ViewFrame({ title, subtitle, children }: { title: string; subtitle: string; children?: React.ReactNode }) {
  return (
    <Panel className="m-4 flex-1 p-6">
      <h2 className="text-base font-semibold text-text">{title}</h2>
      <p className="mt-1 text-sm text-text-muted">{subtitle}</p>
      <div className="mt-4">{children}</div>
    </Panel>
  );
}

// FlowsView moved to ./FlowsView.tsx (D10 — the real transformation inspector).

export function TopologyView() {
  const nodes = useDashboard((s) => s.topologyNodes.length);
  return (
    <ViewFrame title="Topology" subtitle="Provider force-graph — implemented in D12.">
      <p className="text-sm text-text-muted">
        Providers in store: <span className="tabular-nums text-accent">{nodes}</span>
      </p>
    </ViewFrame>
  );
}

export function SankeyView() {
  const edges = useDashboard((s) => s.topologyEdges.length);
  return (
    <ViewFrame title="Sankey" subtitle="Cost/flow Sankey — implemented in D12.">
      <p className="text-sm text-text-muted">
        Edges in store: <span className="tabular-nums text-accent">{edges}</span>
      </p>
    </ViewFrame>
  );
}

export function TheaterView() {
  const monitor = useDashboard((s) => s.monitor.length);
  return (
    <ViewFrame title="Theater" subtitle="Live monitor theater — implemented in D12.">
      <p className="text-sm text-text-muted">
        Monitor messages buffered: <span className="tabular-nums text-accent">{monitor}</span>
      </p>
    </ViewFrame>
  );
}
