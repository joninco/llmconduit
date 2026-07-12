/** Server-authored terminal-flow Sankey for the selected URL scope and historical cut. */
import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import type { CostConfidence, OverviewQuery, OverviewResponse } from '../api/types';
import { getConnection, queryKeys } from '../api/connection';
import { TokenSankey } from '../components/viz/TokenSankey';
import { buildServerSankeyModel, type SankeyModel } from '../components/viz/sankeyModel';
import type { CostDisplay } from '../components/FlowTable/flowModel';
import { useDashboard } from '../store/hooks';
import { useHashScope } from '../router/useHashRoute';
import { flowFilterStore } from '../store/flowFilterStore';
import { navigate } from '../router/useHashRoute';
import { Panel } from '../components/ui/Panel';
import { StaleFallbackBanner } from '../components/StaleFallbackBanner';

const WINDOW_SECONDS = { m1: 60, m5: 300, h1: 3_600 } as const;

export function SankeyView() {
  const scope = useHashScope();
  const seeking = useDashboard((state) => state.connection === 'seeking');
  const seekAtMs = useDashboard((state) => state.seekAtMs);
  const seekCutId = useDashboard((state) => state.seekCutId);
  const { client } = getConnection();
  const openOnly = scope.status === 'open';
  const request = useMemo<OverviewQuery>(() => ({
    window: scope.window,
    ...(seeking && seekCutId !== null ? { cut_id: seekCutId } : seeking && seekAtMs !== null ? { at: seekAtMs } : {}),
    ...(scope.status ? { status: scope.status } : {}),
    ...(scope.model ? { model: scope.model } : {}),
    ...(scope.upstream ? { upstream: scope.upstream } : {}),
    ...(scope.client ? { client: scope.client } : {}),
  }), [scope.client, scope.model, scope.status, scope.upstream, scope.window, seekAtMs, seekCutId, seeking]);

  const overview = useQuery({
    queryKey: queryKeys.overview(request),
    queryFn: () => client.overview(request),
    enabled: !openOnly,
  });

  if (openOnly) {
    return <SankeyUnavailable message="Terminal token lanes are unavailable for open-only scope." />;
  }
  if (overview.isPending && !overview.data) {
    return <SankeyUnavailable message="Loading terminal token lanes…" pending />;
  }
  if (overview.isError || !overview.data) {
    return <SankeyUnavailable message={`Terminal token lanes could not be loaded. ${errorMessage(overview.error)}`} />;
  }

  return <AuthoritativeSankey response={overview.data} seeking={seeking} />;
}

function AuthoritativeSankey({ response, seeking }: { response: OverviewResponse; seeking: boolean }) {
  const seconds = WINDOW_SECONDS[response.scope.window];
  const model = useMemo(
    () => buildServerSankeyModel(response.lanes, seconds),
    [response.lanes, seconds],
  );
  const cost = costPerMinute(response, seconds);
  return (
    <SankeyChrome
      model={model}
      cost={cost}
      seeking={seeking}
      window={response.scope.window}
      staleAsOfMs={response.scope.mode === 'stale_fallback' ? response.scope.selected_at_ms : null}
    />
  );
}

function costPerMinute(response: OverviewResponse, seconds: number): CostDisplay {
  const aggregate = response.cost;
  if (aggregate.samples === 0 || aggregate.total_usd === null || aggregate.confidence === 'unavailable') {
    return { value: '—', estimated: false, confidence: 'unavailable' };
  }
  return {
    value: `$${(aggregate.total_usd * 60 / seconds).toFixed(2)}`,
    estimated: aggregate.confidence === 'estimated',
    confidence: aggregate.confidence as CostConfidence,
  };
}

function SankeyUnavailable({ message, pending = false }: { message: string; pending?: boolean }) {
  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col p-4" data-testid="sankey-view">
      <Panel className="m-auto p-6 text-center text-sm text-text-muted" role={pending ? 'status' : 'alert'} data-testid="sankey-unavailable">
        {message}
      </Panel>
    </div>
  );
}

function SankeyChrome({
  model,
  cost,
  seeking,
  window,
  staleAsOfMs,
}: {
  model: SankeyModel;
  cost: CostDisplay;
  seeking: boolean;
  window: 'm1' | 'm5' | 'h1';
  staleAsOfMs: number | null;
}) {
  function onSelect(modelName: string, upstream: string | null): void {
    const filters = flowFilterStore.getState().filters;
    flowFilterStore.getState().setFilters({ ...filters, model: modelName, upstream });
    navigate('flows');
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col p-4" data-testid="sankey-view">
      <header className="mb-2 flex flex-wrap items-center gap-3">
        <h2 className="text-base font-semibold text-text">Token Sankey</h2>
        <p className="text-sm text-text-muted">
          client → gateway → model · band = tokens reported by terminal flows / {window} · click a band to filter flows
        </p>
        <span className="ml-auto flex items-center gap-1 tabular-nums text-sm text-meta">
          <span data-testid="sankey-cost-per-min" data-confidence={cost.confidence}>{cost.value}/min</span>
          {cost.estimated && (
            <span
              className="shrink-0 rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase tracking-wide text-status-cooling"
              data-testid="sankey-cost-est"
              title="cost is estimated because terminal pricing coverage is incomplete"
            >
              est
            </span>
          )}
        </span>
        {seeking && (
          <span className="rounded-sm border border-status-cooling/40 bg-status-cooling/10 px-2 py-0.5 text-[11px] text-status-cooling" data-testid="sankey-historical">
            historical snapshot
          </span>
        )}
      </header>
      {staleAsOfMs !== null && <StaleFallbackBanner asOfMs={staleAsOfMs} surface="sankey" />}
      <div className="mb-2 flex items-center gap-2 text-[10px] text-text-muted" aria-label="Band terminal cost legend" data-testid="sankey-cost-legend">
        <span>terminal cost</span>
        <span className="h-2.5 w-7 rounded-sm border border-line bg-accent" aria-hidden />
        <span>lower</span>
        <span className="h-2.5 w-7 rounded-sm border-2 border-dashed border-meta bg-accent/40" aria-hidden />
        <span>unpriced / mixed</span>
        <span className="h-2.5 w-7 rounded-sm border border-line bg-status-down" aria-hidden />
        <span>higher</span>
      </div>
      <Panel className="flex min-h-0 flex-1 items-center justify-center overflow-auto p-4">
        {model.links.length === 0 ? (
          <p className="text-sm text-text-muted" data-testid="sankey-empty">No terminal flow reported tokens in this window.</p>
        ) : (
          <TokenSankey model={model} onSelectModel={onSelect} />
        )}
      </Panel>
      {model.links.length > 0 && (
        <div className="mt-3 max-h-40 shrink-0 overflow-auto rounded border border-line" data-testid="sankey-companion-table">
          <table className="w-full text-left text-xs">
            <caption className="sr-only">Server-authored terminal token lanes with terminal-time cost</caption>
            <thead className="sticky top-0 bg-panel text-text-muted">
              <tr>
                <th className="px-2 py-1.5" scope="col">Lane</th>
                <th className="px-2 py-1.5 text-right" scope="col">Reported tokens / {window}</th>
                <th className="px-2 py-1.5 text-right" scope="col">Terminal cost</th>
              </tr>
            </thead>
            <tbody>
              {model.nodes.filter((node) => node.model).map((node) => {
                const lane = model.links.find((link) => link.target === node.id);
                return (
                  <tr key={node.id} className="border-t border-line">
                    <th className="px-2 py-1" scope="row">
                      <button type="button" className="rounded text-accent underline-offset-2 hover:underline" onClick={() => onSelect(node.model!, node.upstream ?? null)}>
                        {node.label}
                      </button>
                    </th>
                    <td className="px-2 py-1 text-right tabular-nums">{Math.round(lane?.value ?? 0).toLocaleString()}</td>
                    <td className="px-2 py-1 text-right tabular-nums">
                      {lane?.costAvailable === false ? '—' : `$${(lane?.cost ?? 0).toFixed(4)}`}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : 'Unknown error.';
}
