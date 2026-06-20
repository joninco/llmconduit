/**
 * SankeyView (D12) — the cost-story screen. Builds the 3-column token-flow model (client →
 * gateway → served-model) from the live flow rows + the D13 price table, then renders the
 * `TokenSankey`. Band height = tokens over the rolling 30 s window; bands are cost-colored; the
 * header shows the windowed `$`/min. Clicking a model band cross-links to the FlowTable filtered to
 * that model.
 *
 * Recompute cadence: the model is a pure function of the store rows + a `nowMs` that advances on a
 * ~1 s tick (the rolling window slides). While SEEKING (D11), `nowMs` is FROZEN to the snapshot cut
 * `seekAtMs` and the rows are the frozen summaries (`applySeekCut` installed them), so the Sankey
 * shows the historical token flow as-of the seeked instant with no live data bleeding in — we read
 * the store's frozen slices directly and stop the live tick.
 */
import { useEffect, useMemo, useState } from 'react';
import { TokenSankey } from '../components/viz/TokenSankey';
import { buildSankeyModel } from '../components/viz/sankeyModel';
import { useDashboard } from '../store/hooks';
import { flowFilterStore } from '../store/flowFilterStore';
import { navigate } from '../router/useHashRoute';
import { Panel } from '../components/ui/Panel';

const RECOMPUTE_MS = 1000;
const WINDOW_MS = 30_000;

export function SankeyView() {
  const flows = useDashboard((s) => s.flows);
  const priceTable = useDashboard((s) => s.priceTable);
  const seeking = useDashboard((s) => s.connection === 'seeking');
  // While seeking the window is anchored to the frozen cut instant; live → it slides on a 1 s tick.
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const [nowTick, setNowTick] = useState(() => Date.now());

  useEffect(() => {
    if (seeking) return; // frozen: the window does not advance during a seek.
    const id = window.setInterval(() => setNowTick(Date.now()), RECOMPUTE_MS);
    return () => window.clearInterval(id);
  }, [seeking]);

  const nowMs = seeking ? (seekAtMs ?? nowTick) : nowTick;
  const rows = useMemo(() => [...flows.values()], [flows]);
  const model = useMemo(
    () => buildSankeyModel(rows, priceTable, nowMs, WINDOW_MS),
    [rows, priceTable, nowMs],
  );

  function onSelectModel(m: string): void {
    flowFilterStore.getState().setModel(m);
    navigate('flows');
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col p-4" data-testid="sankey-view">
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Token Sankey</h2>
        <p className="text-sm text-text-muted">client → gateway → model · band = tokens/30s · click a band to filter flows</p>
        <span className="ml-auto tabular-nums text-sm text-meta" data-testid="sankey-cost-per-min">
          ${model.costPerMin.toFixed(2)}/min
        </span>
        {seeking && (
          <span
            className="rounded-sm border border-status-cooling/40 bg-status-cooling/10 px-2 py-0.5 text-[11px] text-status-cooling"
            data-testid="sankey-historical"
          >
            historical snapshot
          </span>
        )}
      </header>
      <Panel className="flex min-h-0 flex-1 items-center justify-center overflow-auto p-4">
        {model.links.length === 0 ? (
          <p className="text-sm text-text-muted" data-testid="sankey-empty">No token flow in the last 30s.</p>
        ) : (
          <TokenSankey model={model} onSelectModel={onSelectModel} />
        )}
      </Panel>
    </div>
  );
}
