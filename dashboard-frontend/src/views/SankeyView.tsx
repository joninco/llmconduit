/**
 * SankeyView (D12) — the cost-story screen. Builds the 3-column token-flow model (client →
 * gateway → (upstream, served-model)) and renders the `TokenSankey`. Band height = tokens over the
 * rolling 30 s window; bands are cost-colored. Clicking a model band cross-links to the FlowTable
 * filtered ATOMICALLY to that (upstream, model) pair (finding 9).
 *
 * Band source (finding 2): LIVE bands come from `useSankeyWindow`, which subscribes to the store
 * directly and folds each flow's usage GROWTH into TIMESTAMPED deltas pruned to the window — so a
 * long-running flow's lifetime total never inflates the 30 s rate. The `$`/min readout is the
 * authoritative `MetricTick.cost_per_min` from the store (finding 3), NOT a local projection of the
 * Sankey lane costs; that value is also the correctly FROZEN one during seek.
 *
 * SEEK (D11): a frozen snapshot is a point-in-time cut with NO delta history, so we cannot rebuild a
 * rolling rate from it. We synthesize one delta per frozen flow (its cumulative usage at the cut
 * instant) and build the Sankey with `nowMs = seekAtMs`, so the historical token attribution shows
 * as-of the seeked moment with no live data bleeding in (the frozen rows replace the store slices;
 * the live accumulator skips while seeking). `cost_per_min` reads the frozen store metrics.
 */
import { useEffect, useMemo, useState } from 'react';
import { TokenSankey } from '../components/viz/TokenSankey';
import { buildSankeyModel, type SankeyUsageDelta } from '../components/viz/sankeyModel';
import { useDashboard } from '../store/hooks';
import { useSankeyWindow } from '../store/useSankeyWindow';
import { useTopologyQuery } from '../store/useTopologyQuery';
import { flowFilterStore } from '../store/flowFilterStore';
import { navigate } from '../router/useHashRoute';
import { Panel } from '../components/ui/Panel';

const WINDOW_MS = 30_000;
const RECOMPUTE_MS = 1_000;

export function SankeyView() {
  // Seed the price table (+ topology) from `/topology` so the cost colors / lane costs have prices
  // even when the topology view was never opened first — finding 5 (LIVE-only; never overwrites seek).
  useTopologyQuery();
  const seeking = useDashboard((s) => s.connection === 'seeking');
  return seeking ? <SeekSankey /> : <LiveSankey />;
}

/** LIVE: bands from the rolling delta window; `$`/min from the live `MetricTick.cost_per_min`. */
function LiveSankey() {
  const priceTable = useDashboard((s) => s.priceTable);
  const costPerMin = useDashboard((s) => s.metrics?.cost_per_min ?? 0);
  const { version, deltasRef } = useSankeyWindow(WINDOW_MS);
  // A ~1 s recompute tick (spec: "Recompute ~1 s") so the rolling window SLIDES even when the store
  // is momentarily idle: `buildSankeyModel` filters deltas by `nowMs`, so advancing the clock drains
  // aged-out tokens from the bands without waiting for the next usage frame.
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNowMs(Date.now()), RECOMPUTE_MS);
    return () => window.clearInterval(id);
  }, []);

  // `version` bumps on each folded increment / prune so this body re-runs to read the ref ring; the
  // `nowMs` tick slides the window between folds.
  void version;
  const model = useMemo(
    () => buildSankeyModel(deltasRef.current, priceTable, nowMs, WINDOW_MS),
    // Recompute when the ring changed (version), the clock ticked (nowMs), or prices moved.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [version, nowMs, priceTable],
  );

  return <SankeyChrome model={model} costPerMin={costPerMin} seeking={false} />;
}

/**
 * SEEK: synthesize one delta per FROZEN flow (its cumulative usage at the cut instant) so the model
 * shows the historical token attribution; `$`/min from the frozen store metrics. No live data leaks
 * in — the store holds the frozen cut and the live accumulator is skipped while seeking.
 */
function SeekSankey() {
  const flows = useDashboard((s) => s.flows);
  const priceTable = useDashboard((s) => s.priceTable);
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const costPerMin = useDashboard((s) => s.metrics?.cost_per_min ?? 0);
  const atMs = seekAtMs ?? Date.now();

  const deltas = useMemo<SankeyUsageDelta[]>(() => {
    const out: SankeyUsageDelta[] = [];
    for (const f of flows.values()) {
      const u = f.usage;
      const model = f.model_served ?? f.model_requested;
      if (!u || !model || u.total <= 0) continue;
      // One synthetic delta at the cut instant: the frozen cumulative IS the windowed attribution
      // for a body-free snapshot (no per-tick history survives the cut).
      out.push({ ts: atMs, upstream: f.upstream_target ?? null, model, prompt: u.prompt, cached: u.cached, completion: u.completion, total: u.total });
    }
    return out;
  }, [flows, atMs]);

  const model = useMemo(() => buildSankeyModel(deltas, priceTable, atMs, WINDOW_MS), [deltas, priceTable, atMs]);
  return <SankeyChrome model={model} costPerMin={costPerMin} seeking />;
}

/** Shared chrome: header (with `$`/min + the seek affordance), empty state, and the chart. */
function SankeyChrome({
  model,
  costPerMin,
  seeking,
}: {
  model: ReturnType<typeof buildSankeyModel>;
  costPerMin: number;
  seeking: boolean;
}) {
  function onSelect(m: string, upstream: string | null): void {
    // Filter BOTH facets atomically (finding 9): the lane is a (upstream, model) pair, so the
    // cross-link lands the table on exactly that lane. A null upstream filters model alone.
    const filters = flowFilterStore.getState().filters;
    flowFilterStore.getState().setFilters({ ...filters, model: m, upstream: upstream ?? null });
    navigate('flows');
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col p-4" data-testid="sankey-view">
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Token Sankey</h2>
        <p className="text-sm text-text-muted">client → gateway → model · band = tokens/30s · click a band to filter flows</p>
        <span className="ml-auto tabular-nums text-sm text-meta" data-testid="sankey-cost-per-min">
          ${costPerMin.toFixed(2)}/min
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
          <TokenSankey model={model} onSelectModel={onSelect} />
        )}
      </Panel>
    </div>
  );
}
