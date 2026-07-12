/**
 * Terminal Overview control room. Every flow rollup comes from the immutable server-side
 * `/dashboard/api/overview` cut selected by the shared URL window/filters and, while seeking,
 * the retained `at` instant. Provider attempts are deliberately global and labelled as such.
 */
import { useEffect, useMemo, useState, type ReactNode } from 'react';
import { useQuery } from '@tanstack/react-query';
import type {
  FlowStatus,
  OverviewCost,
  OverviewDataQuality,
  OverviewDimensionRollup,
  OverviewQuery,
  OverviewResponse,
  OverviewTokens,
  ProviderLatency,
} from '../../api/types';
import { getConnection, queryKeys } from '../../api/connection';
import { useDashboard } from '../../store/hooks';
import { flowFilterStore } from '../../store/flowFilterStore';
import { navigate, readHashScope, useHashScope } from '../../router/useHashRoute';
import type { FlowFilters } from '../../components/FlowTable/filterTypes';
import { Panel } from '../../components/ui/Panel';
import { Sparkline } from '../../viz/Sparkline';
import { ProviderLatencyTile } from '../../components/viz/ProviderLatencyTile';
import { buildProviderLatency } from '../../components/viz/providerLatency';
import { fmtCost, fmtLatency, fmtPercent, fmtTokens, fmtTokensPerSec } from '../../components/FlowTable/format';
import { cn } from '../../lib/cn';
import { useTopologyQuery, topologyProviderKey } from '../../store/useTopologyQuery';
import { EngineMetricsCard } from '../../components/viz/EngineMetricsCard';
import { StaleFallbackBanner } from '../../components/StaleFallbackBanner';
import { deriveDashboardStatus } from '../../lib/dashboardStatus';

const DASH = '—';
const TOP_ROWS = 5;
const ERROR_RATE_THRESHOLD = 5;
const ROW_BUTTON =
  'flex w-full items-center justify-between gap-2 rounded-md border border-line/50 bg-panel-raised px-2 py-1 text-left transition-colors hover:border-accent/60 hover:bg-accent/5 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent';

type FlowFacet = 'model' | 'upstream' | 'client' | 'failure';
type DisplayQuality = OverviewDataQuality | 'estimated';

const QUALITY_CLASS: Record<DisplayQuality, string> = {
  measured: 'text-status-healthy',
  derived: 'text-accent',
  estimated: 'text-status-cooling',
  partial: 'text-status-cooling',
  unavailable: 'text-text-muted',
};

export function OverviewView() {
  const hashScope = useHashScope();
  const seeking = useDashboard((state) => state.connection === 'seeking');
  const seekAtMs = useDashboard((state) => state.seekAtMs);
  const seekCutId = useDashboard((state) => state.seekCutId);
  const { client } = getConnection();
  const openOnly = hashScope.status === 'open';
  const topologyNodes = useDashboard((state) => state.topologyNodes);
  const { engineMetricsById } = useTopologyQuery();

  const request = useMemo<OverviewQuery>(() => ({
    window: hashScope.window,
    ...(seeking && seekCutId !== null ? { cut_id: seekCutId } : seeking && seekAtMs !== null ? { at: seekAtMs } : {}),
    ...(hashScope.status ? { status: hashScope.status } : {}),
    ...(hashScope.model ? { model: hashScope.model } : {}),
    ...(hashScope.upstream ? { upstream: hashScope.upstream } : {}),
    ...(hashScope.client ? { client: hashScope.client } : {}),
  }), [hashScope.client, hashScope.model, hashScope.status, hashScope.upstream, hashScope.window, seekAtMs, seekCutId, seeking]);

  const overview = useQuery({
    queryKey: queryKeys.overview(request),
    queryFn: () => client.overview(request),
    enabled: !openOnly,
  });

  return (
    <div className="min-h-0 min-w-0 flex-1 overflow-auto p-3 sm:p-4" data-testid="overview-view">
      <div className="mb-3 flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <h1 className="text-base font-semibold text-text">Control room</h1>
        <span className="text-[11px] text-text-muted">
          terminal {hashScope.window} rollups · server cut
        </span>
        {seeking && (
          <span className="text-[10px] text-status-cooling" data-testid="overview-frozen">
            · frozen cut
          </span>
        )}
      </div>

      {!openOnly && overview.data && <OverviewHeadline response={overview.data} />}

      <section className="mb-3" aria-labelledby="engine-health-title" data-testid="engine-health-section">
        <div className="mb-2 flex items-baseline gap-2">
          <h2 id="engine-health-title" className="text-sm font-semibold text-text">Engine health</h2>
          <span className="text-[11px] text-text-muted">Backend m1 · scheduler, cache, and throughput</span>
        </div>
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-4">
          {topologyNodes
            .filter((node) => !hashScope.upstream || node.id === hashScope.upstream)
            .map((node) => (
              <EngineMetricsCard
                key={topologyProviderKey(node)}
                provider={node.route ? `${node.route} / ${node.name}` : node.name}
                metrics={engineMetricsById[topologyProviderKey(node)] ?? node.engine_metrics}
                nowMs={seeking && seekAtMs != null ? seekAtMs : Date.now()}
                compact
              />
            ))}
          {topologyNodes.filter((node) => !hashScope.upstream || node.id === hashScope.upstream).length === 0 && (
            <Panel className="p-3 text-xs italic text-text-muted" data-quality="unavailable">No providers in this scope · {DASH}</Panel>
          )}
        </div>
      </section>

      {openOnly ? (
        <Panel className="p-6 text-center" data-testid="overview-open-unavailable" role="status">
          <p className="text-sm text-status-cooling">Terminal analytics unavailable for open-only scope.</p>
          <p className="mt-1 text-xs text-text-muted">Active-now remains available in the Global metrics strip.</p>
        </Panel>
      ) : overview.isPending && !overview.data ? (
        <Panel className="p-6 text-center text-sm text-text-muted" data-testid="overview-loading" role="status">
          Loading terminal overview…
        </Panel>
      ) : overview.isError || !overview.data ? (
        <Panel className="flex flex-col items-center gap-2 p-6 text-center" data-testid="overview-error" role="alert">
          <p className="text-sm text-status-down">The terminal overview could not be loaded.</p>
          <p className="max-w-xl text-xs text-text-muted">{errorMessage(overview.error)}</p>
          <button type="button" className={cn(ROW_BUTTON, 'w-auto px-3')} onClick={() => void overview.refetch()}>
            Retry
          </button>
        </Panel>
      ) : (
        <OverviewContent response={overview.data} />
      )}
    </div>
  );
}

function OverviewHeadline({ response }: { response: OverviewResponse }) {
  const connection = useDashboard((state) => state.connection);
  const metrics = useDashboard((state) => state.metrics);
  const hasDashboardData = useDashboard((state) => state.metrics !== null || state.flows.size > 0 || state.topologyNodes.length > 0);
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, []);
  const instant = metrics?.instant ?? null;
  const status = deriveDashboardStatus({
    connection,
    hasDashboardData,
    generatedAtMs: metrics?.generated_at_ms ?? null,
    activeStreams: instant?.active_streams_now ?? 0,
    lastActivityAtMs: metrics?.last_activity?.at_ms ?? null,
    nowMs,
  });
  const scoped = Boolean(response.scope.status || response.scope.model || response.scope.upstream || response.scope.client);
  const failureRate = response.totals.requests > 0 ? response.totals.failures / response.totals.requests * 100 : null;
  const p95Available = Boolean(instant && instant.latency_samples >= 2 && instant.p95_ms !== null);
  const throughput = metrics?.engine_throughput?.generated_tokens_per_sec ?? instant?.reported_tokens_per_sec ?? null;
  const throughputSamples = metrics?.engine_throughput
    ? `${metrics.engine_throughput.measured_sources}/${metrics.engine_throughput.total_sources} engines`
    : `${instant?.usage_samples ?? 0} usage samples`;
  const health = connection === 'error' || connection === 'closed'
    ? { value: 'Disconnected', cls: 'text-status-down', detail: 'Dashboard transport requires attention.' }
    : status.freshness.label === 'Stale'
      ? { value: 'Metrics stale', cls: 'text-status-cooling', detail: status.freshness.detail }
      : response.totals.failures > 0
        ? { value: 'Degraded', cls: 'text-status-down', detail: `${response.totals.failures} failed terminal flow${response.totals.failures === 1 ? '' : 's'} in scope.` }
        : response.totals.requests === 0
          ? { value: 'No terminal traffic', cls: 'text-text-muted', detail: 'No completed requests are in the selected window.' }
          : { value: 'Healthy', cls: 'text-status-healthy', detail: 'No failures in the selected window.' };

  return (
    <section className="mb-4" aria-labelledby="overview-headline-title" data-testid="overview-headline">
      <div className="mb-2 flex min-w-0 flex-wrap items-center justify-between gap-2">
        <h2 id="overview-headline-title" className="text-sm font-semibold text-text">Operational picture</h2>
      </div>
      <div className="grid min-w-0 grid-cols-2 gap-px rounded-md border border-line bg-line md:grid-cols-3 xl:grid-cols-6">
        <HeadlineMetric label="Overall health" value={health.value} className={health.cls} detail={health.detail} />
        <HeadlineMetric label="Active traffic" value={`${instant?.active_streams_now ?? 0}`} className={(instant?.active_streams_now ?? 0) > 0 ? 'text-status-healthy' : 'text-text'} detail="Global · current open requests" />
        <HeadlineMetric
          label="Failure rate"
          value={failureRate === null ? 'Unavailable' : fmtPercent(failureRate)}
          className={failureRate !== null && failureRate > 0 ? 'text-status-down' : 'text-text'}
          detail={`${response.totals.requests} terminal · ${scoped ? 'Scoped' : 'Global'} · last ${response.scope.window}`}
        />
        <HeadlineMetric
          label="Gateway E2E P95"
          value={p95Available ? fmtLatency(instant!.p95_ms!) : 'Unavailable'}
          className={p95Available ? 'text-text' : 'text-text-muted'}
          detail={instant && instant.latency_samples < 2 ? `Latest publisher interval · Gateway E2E · ${instant.latency_samples} sample · insufficient` : `Latest publisher interval · Gateway E2E · ${instant?.latency_samples ?? 0} terminal requests · measured`}
        />
        <HeadlineMetric
          label="Throughput"
          value={throughput === null ? 'Unavailable' : fmtTokensPerSec(throughput)}
          className={throughput === null ? 'text-text-muted' : 'text-status-healthy'}
          detail={`${throughputSamples} · Global · latest interval`}
        />
        <HeadlineMetric
          label="Data freshness"
          value={status.freshness.label}
          className={status.freshness.label === 'Fresh' ? 'text-status-healthy' : status.freshness.label === 'Stale' ? 'text-status-cooling' : 'text-text-muted'}
          detail={status.freshness.detail}
        />
      </div>
    </section>
  );
}

function HeadlineMetric({ label, value, detail, className }: { label: string; value: string; detail: string; className: string }) {
  return (
    <div className="min-w-0 bg-panel px-3 py-2.5" title={detail}>
      <div className="text-[11px] font-medium text-text-muted">{label}</div>
      <div className={cn('mt-0.5 break-words font-mono text-lg font-semibold tabular-nums', className)}>{value}</div>
      <div className="mt-1 text-[10px] leading-tight text-text-muted">{detail}</div>
    </div>
  );
}

function OverviewContent({ response }: { response: OverviewResponse }) {
  const scoped = Boolean(
    response.scope.status || response.scope.model || response.scope.upstream || response.scope.client,
  );
  const partial = response.data_quality === 'partial' || response.overflow.overflowed;

  return (
    <>
      {response.scope.mode === 'stale_fallback' && response.scope.selected_at_ms !== null && (
        <StaleFallbackBanner asOfMs={response.scope.selected_at_ms} surface="overview" />
      )}
      <section
        className="mb-3 flex flex-wrap items-center gap-2 rounded-md border border-line bg-panel px-3 py-2 text-[10px]"
        aria-label="Overview aggregate provenance"
        data-testid="overview-provenance"
        data-quality={response.data_quality}
      >
        <QualityBadge quality={response.data_quality} />
        <span className="rounded border border-accent/40 px-1.5 py-0.5 uppercase tracking-wide text-accent">
          Flow rollups · {scoped ? 'Scoped' : 'Global'}
        </span>
        <span className="rounded border border-line px-1.5 py-0.5 uppercase tracking-wide text-text-muted">
          Provider attempts · Global
        </span>
        <span className="font-mono tabular-nums text-text-muted">{response.totals.requests} terminal flows</span>
        <span className="font-mono tabular-nums text-text-muted">
          {response.totals.successes} success · {response.totals.failures} fail · {response.totals.cancellations} cancel
        </span>
        <span className="ml-auto font-mono tabular-nums text-text-muted">
          cut {formatCut(response.scope.selected_at_ms ?? response.generated_at_ms)} · seq {response.metrics_seq}
        </span>
      </section>

      {partial && (
        <div
          className="mb-3 rounded-md border border-status-cooling/50 bg-status-cooling/10 px-3 py-2 text-xs text-status-cooling"
          role="status"
          data-testid="overview-partial"
          data-quality="partial"
        >
          Partial aggregate: bounded dimensions folded {response.overflow.slot_folded_samples} slot samples and{' '}
          {response.overflow.aggregate_folded_samples} window samples into <code>__other__</code>{' '}
          (limit {response.overflow.dimension_limit}); provider health folded {response.overflow.provider_folded_samples}{' '}
          attempt samples. Totals include folded samples; filtered attribution may not.
        </div>
      )}

      <section className="mb-4" aria-labelledby="overview-operations-title">
        <h2 id="overview-operations-title" className="mb-2 text-sm font-semibold text-text">Flow and provider health</h2>
        <div className="grid grid-cols-1 gap-3 xl:grid-cols-2 2xl:grid-cols-5">
          <FlowOutcomesTile response={response} />
          <ProviderAttemptsTile response={response} />
          <FailureTile response={response} />
          <CostSeriesTile response={response} />
        </div>
      </section>

      <section className="mb-4" aria-labelledby="overview-attribution-title">
        <h2 id="overview-attribution-title" className="mb-2 text-sm font-semibold text-text">Served traffic</h2>
        <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
          <DimensionGroup title="Served models" subtitle="Volume and cost share one scoped population">
            <LeaderboardTile testId="overview-top-models-volume" title="By volume" rows={response.served_models} mode="volume" facet="model" quality={response.data_quality} />
            <LeaderboardTile testId="overview-top-models-cost" title="By cost" rows={response.served_models} mode="cost" facet="model" quality={response.data_quality} />
          </DimensionGroup>
          <DimensionGroup title="Served providers" subtitle="Scoped terminal flows; provider attempts above remain Global">
            <LeaderboardTile testId="overview-top-providers-volume" title="By volume" rows={response.providers} mode="volume" facet="upstream" quality={response.data_quality} />
            <LeaderboardTile testId="overview-top-providers-cost" title="By cost" rows={response.providers} mode="cost" facet="upstream" quality={response.data_quality} />
          </DimensionGroup>
        </div>
      </section>

      <section aria-labelledby="overview-resources-title">
        <h2 id="overview-resources-title" className="mb-2 text-sm font-semibold text-text">Clients and resource use</h2>
        <div className="grid grid-cols-1 gap-3 xl:grid-cols-3">
          <ClientTile response={response} />
          <ContextTile response={response} />
          <TokenMixTile tokens={response.tokens} quality={response.data_quality} />
        </div>
      </section>
    </>
  );
}

function FlowOutcomesTile({ response }: { response: OverviewResponse }) {
  const available = response.totals.requests > 0;
  return (
    <Panel className="p-3" data-testid="overview-flow-outcomes" data-quality={available ? response.data_quality : 'unavailable'}>
      <div className="mb-2 flex items-baseline justify-between gap-2">
        <span className="text-xs font-semibold text-text">Flow outcomes</span>
        <span className="text-[10px] text-text-muted">{response.totals.requests} terminal · last {response.scope.window}</span>
      </div>
      {available ? (
        <dl className="grid grid-cols-3 gap-px overflow-hidden rounded border border-line/60 bg-line/60">
          <OutcomeFigure label="Succeeded" value={response.totals.successes} className="text-status-healthy" />
          <OutcomeFigure label="Failed" value={response.totals.failures} className={response.totals.failures > 0 ? 'text-status-down' : 'text-text'} />
          <OutcomeFigure label="Cancelled" value={response.totals.cancellations} className="text-status-cooling" />
        </dl>
      ) : (
        <p className="text-xs text-text-muted">No terminal flows in this scoped window.</p>
      )}
      <p className="mt-2 text-[10px] text-text-muted">Flow rollups count final client outcomes. Provider attempts count every dispatch, including failed primaries.</p>
    </Panel>
  );
}

function OutcomeFigure({ label, value, className }: { label: string; value: number; className: string }) {
  return (
    <div className="bg-panel-raised px-2 py-2">
      <dt className="text-[10px] text-text-muted">{label}</dt>
      <dd className={cn('font-mono text-xl font-semibold tabular-nums', className)}>{value}</dd>
    </div>
  );
}

function DimensionGroup({ title, subtitle, children }: { title: string; subtitle: string; children: ReactNode }) {
  return (
    <Panel className="min-w-0 p-3">
      <div className="mb-3">
        <h3 className="text-xs font-semibold text-text">{title}</h3>
        <p className="text-[10px] text-text-muted">{subtitle}</p>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">{children}</div>
    </Panel>
  );
}

function CostSeriesTile({ response }: { response: OverviewResponse }) {
  const points = response.cost_series.map((point) =>
    point.requests === 0 ? 0
      : point.cost.samples > 0 && point.cost.total_usd !== null ? point.cost.total_usd : null);
  const quality = costQuality(response.cost, response.data_quality);
  const available = response.cost.samples > 0 && response.cost.total_usd !== null;
  return (
    <Panel className="flex min-w-0 flex-col gap-2 p-3" data-testid="overview-cost-trend" data-quality={quality}>
      <div className="flex flex-wrap items-baseline justify-between gap-1">
        <span className="text-xs font-semibold text-text">Scoped cost</span>
        <QualityBadge quality={quality} />
      </div>
      <div className="flex flex-wrap items-end gap-x-5 gap-y-2">
        <div>
          <div className="text-[10px] text-text-muted">Window total · {response.cost.samples} priced samples</div>
          <div className={cn('font-mono text-2xl font-semibold tabular-nums', QUALITY_CLASS[quality])} data-testid="overview-cost-total">
            {available ? fmtCost(response.cost.total_usd) : 'Unavailable'}
          </div>
          {!available && <p className="mt-1 text-[10px] text-text-muted">No pricing rule matched a usage-bearing request in this scope.</p>}
        </div>
        <div className="font-mono text-xs tabular-nums text-text-muted">
          {response.cost.samples} priced / {response.totals.requests} requests
        </div>
        <div className="ml-auto min-w-36 flex-1">
          {points.length > 0 ? (
            <Sparkline data={points} label="Scoped terminal cost per deterministic server bin" />
          ) : (
            <p className="text-right text-xs italic text-text-muted" data-quality="unavailable">No priced series points · {DASH}</p>
          )}
        </div>
      </div>
    </Panel>
  );
}

function ProviderAttemptsTile({ response }: { response: OverviewResponse }) {
  const aggregate = response.provider_attempts_global;
  const providers = [...aggregate.providers]
    .sort((a, b) => b.error_rate - a.error_rate || (b.p99 ?? -1) - (a.p99 ?? -1))
    .slice(0, TOP_ROWS);
  const available = providers.length > 0;
  return (
    <Panel className="flex flex-col gap-2 p-3 2xl:col-span-2" data-testid="overview-providers" data-available={String(available)} data-quality={aggregate.data_quality}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-semibold text-text">Provider attempts · latency and error</span>
        <span className="rounded border border-line px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-wide text-text-muted">Global</span>
      </div>
      {!available ? (
        <p className="px-1 py-2 text-xs italic text-text-muted" data-testid="overview-providers-unavailable" data-quality="unavailable">
          No provider-attempt samples in this window · {DASH}
        </p>
      ) : (
        <ul className="grid grid-cols-1 gap-2 sm:grid-cols-2" role="list">
          {providers.map((provider) => (
            <li key={provider.provider}>
              <button
                type="button"
                className={cn(ROW_BUTTON, 'block h-full')}
                onClick={() => showFlows('upstream', provider.provider)}
                aria-label={`Show flows for provider ${provider.provider}`}
                data-testid="overview-provider"
                data-provider={provider.provider}
              >
                <span className="block truncate font-mono text-xs text-text" title={provider.provider}>{provider.provider}</span>
                <ProviderLatencyTile model={buildProviderLatency(provider as ProviderLatency, provider.provider)} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function LeaderboardTile({
  testId,
  title,
  rows,
  mode,
  facet,
  quality,
}: {
  testId: string;
  title: string;
  rows: OverviewDimensionRollup[];
  mode: 'volume' | 'cost';
  facet: 'model' | 'upstream';
  quality: OverviewDataQuality;
}) {
  const ranked = [...rows]
    .filter((row) => mode === 'volume' || row.cost.total_usd !== null)
    .sort((a, b) => mode === 'volume'
      ? b.requests - a.requests || a.key.localeCompare(b.key)
      : (b.cost.total_usd ?? 0) - (a.cost.total_usd ?? 0) || a.key.localeCompare(b.key));
  const visible = ranked.slice(0, TOP_ROWS);
  const available = visible.length > 0;
  return (
    <div className="flex min-w-0 flex-col gap-2 border-t border-line/60 pt-2" data-testid={testId} data-available={String(available)} data-quality={quality}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-medium text-text-muted">{title}</span>
        {ranked.length > visible.length && <span className="text-[9px] text-text-muted">+{ranked.length - visible.length} more</span>}
      </div>
      {!available ? (
        <p className="px-1 py-2 text-xs italic text-text-muted" data-testid={`${testId}-unavailable`} data-quality="unavailable">
          {mode === 'cost' ? 'Cost unavailable — no priced groups.' : `No terminal flows · ${DASH}`}
        </p>
      ) : (
        <ol className="flex flex-col gap-1">
          {visible.map((row, index) => (
            <li key={row.key}>
              <button
                type="button"
                className={ROW_BUTTON}
                onClick={() => showFlows(facet, row.key)}
                aria-label={`Show flows for ${facet} ${row.key}`}
                data-testid="overview-leaderboard-row"
                data-key={row.key}
              >
                <span className="flex min-w-0 items-baseline gap-1.5">
                  <span className="text-[10px] tabular-nums text-text-muted">{index + 1}.</span>
                  <span className="truncate font-mono text-xs text-text" title={row.key}>{row.key}</span>
                </span>
                <RollupFigures row={row} quality={quality} />
              </button>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}

function RollupFigures({ row, quality }: { row: OverviewDimensionRollup; quality: OverviewDataQuality }) {
  const cost = costQuality(row.cost, quality);
  return (
    <span className="flex shrink-0 items-baseline gap-2">
      <span className="font-mono text-xs tabular-nums text-text" data-testid="overview-leaderboard-volume" data-quality={quality === 'partial' ? 'partial' : 'measured'}>
        {row.requests}
      </span>
      <span className={cn('font-mono text-xs tabular-nums', QUALITY_CLASS[cost])} data-testid="overview-leaderboard-cost" data-quality={cost}>
        {fmtCost(row.cost.total_usd)}
      </span>
      {row.cost.confidence === 'estimated' && row.cost.total_usd !== null && <SmallBadge text="est" />}
      {quality === 'partial' && <SmallBadge text="partial" />}
    </span>
  );
}

function FailureTile({ response }: { response: OverviewResponse }) {
  const failures = [...response.failures].sort((a, b) => b.requests - a.requests).slice(0, TOP_ROWS);
  const failed = response.failures.reduce((total, row) => total + row.requests, 0);
  const rate = response.totals.requests > 0 ? failed / response.totals.requests * 100 : null;
  const quality: OverviewDataQuality = rate === null
    ? 'unavailable'
    : response.data_quality === 'partial' ? 'partial' : 'derived';
  return (
    <Panel className="flex flex-col gap-2 p-3" data-testid="overview-failures" data-available={String(response.totals.requests > 0)} data-quality={quality}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-semibold text-text">Failures by reason</span>
        <span className={cn('font-mono text-sm font-semibold tabular-nums', rate !== null && rate > ERROR_RATE_THRESHOLD ? 'text-status-down' : QUALITY_CLASS[quality])} data-testid="overview-failures-rate" data-quality={quality}>
          {rate === null ? DASH : fmtPercent(rate)}
        </span>
      </div>
      {response.totals.requests === 0 ? (
        <p className="px-1 py-2 text-xs italic text-text-muted" data-testid="overview-failures-unavailable" data-quality="unavailable">No terminal flows · {DASH}</p>
      ) : failures.length === 0 ? (
        <p className="px-1 py-2 text-xs italic text-text-muted" data-testid="overview-failures-none">No failures in this scoped window.</p>
      ) : (
        <ul className="flex flex-col gap-1" role="list">
          {failures.map((row) => (
            <li key={row.key}>
              <button type="button" className={ROW_BUTTON} onClick={() => showFlows('failure', row.key)} aria-label={`Show failed flows for reason ${row.key}`} data-testid="overview-failure-group" data-group-key={row.key}>
                <span className="font-mono text-xs text-text">{humanize(row.key)}</span>
                <span className="font-mono text-xs font-semibold tabular-nums text-status-down">{row.requests}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function ClientTile({ response }: { response: OverviewResponse }) {
  const rows = [...response.clients].sort((a, b) => b.requests - a.requests).slice(0, TOP_ROWS);
  return (
    <Panel className="flex flex-col gap-2 p-3" data-testid="overview-clients" data-available={String(rows.length > 0)} data-quality={response.data_quality}>
      <span className="text-xs font-semibold text-text">Clients · scoped terminal rollup</span>
      {rows.length === 0 ? (
        <p className="px-1 py-2 text-xs italic text-text-muted" data-testid="overview-clients-unavailable" data-quality="unavailable">No attributed client samples · {DASH}</p>
      ) : (
        <ul className="flex flex-col gap-1" role="list">
          {rows.map((row) => {
            const quality = costQuality(row.cost, response.data_quality);
            return (
              <li key={row.key}>
                <button type="button" className={ROW_BUTTON} onClick={() => showFlows('client', row.key)} aria-label={`Show flows for client ${row.key}`} data-testid="overview-client-row" data-client={row.key}>
                  <span className="min-w-0 truncate font-mono text-xs text-text" title={row.key}>{row.key}</span>
                  <span className="flex shrink-0 items-baseline gap-2">
                    <span className="font-mono text-xs tabular-nums text-text">{row.requests} req</span>
                    <span className={cn('font-mono text-xs tabular-nums', QUALITY_CLASS[quality])} data-testid="overview-client-cost" data-quality={quality}>{fmtCost(row.cost.total_usd)}</span>
                    {row.cost.confidence === 'estimated' && row.cost.total_usd !== null && <SmallBadge text="est" />}
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

function ContextTile({ response }: { response: OverviewResponse }) {
  const context = response.context;
  const available = context.samples > 0 && context.average_pressure_pct !== null;
  const pressure = context.average_pressure_pct;
  return (
    <Panel className="flex flex-col gap-3 p-3" data-testid="overview-context" data-available={String(available)} data-quality={context.data_quality}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-semibold text-text">Context pressure</span>
        <QualityBadge quality={context.data_quality} />
      </div>
      <div className="grid grid-cols-2 gap-3">
        <div>
          <div className="text-[10px] text-text-muted">Average utilization</div>
          <div className={cn('font-mono text-2xl font-semibold tabular-nums', pressure !== null && pressure >= 90 ? 'text-status-down' : pressure !== null && pressure >= 75 ? 'text-status-cooling' : QUALITY_CLASS[context.data_quality])} data-testid="overview-context-pressure">
            {available ? fmtPercent(pressure) : DASH}
          </div>
        </div>
        <div className="text-right">
          <div className="text-[10px] text-text-muted">Effective route limit</div>
          <div className="font-mono text-sm tabular-nums text-text" data-testid="overview-context-limit" data-quality={context.effective_route_limit_min === null ? 'unavailable' : 'derived'}>
            {fmtTokens(context.effective_route_limit_min)} tok
          </div>
        </div>
      </div>
      <div className="text-[10px] text-text-muted">
        {context.samples} measured · {context.unavailable_samples} unavailable · {fmtTokens(context.input_tokens)} input tokens
      </div>
    </Panel>
  );
}

function TokenMixTile({ tokens, quality }: { tokens: OverviewTokens; quality: OverviewDataQuality }) {
  const available = tokens.samples > 0 && tokens.prompt !== null && tokens.completion !== null;
  const cached = tokens.cached ?? 0;
  const reasoning = tokens.reasoning ?? 0;
  const exclusive = available ? [
    { key: 'prompt', value: Math.max(0, tokens.prompt! - cached), color: 'bg-accent' },
    { key: 'cached', value: cached, color: 'bg-meta' },
    { key: 'completion', value: Math.max(0, tokens.completion! - reasoning), color: 'bg-status-healthy' },
    { key: 'reasoning', value: reasoning, color: 'bg-status-cooling' },
  ] : [];
  const total = exclusive.reduce((sum, item) => sum + item.value, 0);
  const fields: { key: keyof Pick<OverviewTokens, 'prompt' | 'completion' | 'cached' | 'reasoning'>; label: string; color: string }[] = [
    { key: 'prompt', label: 'prompt', color: 'bg-accent' },
    { key: 'completion', label: 'completion', color: 'bg-status-healthy' },
    { key: 'cached', label: 'cached', color: 'bg-meta' },
    { key: 'reasoning', label: 'reasoning', color: 'bg-status-cooling' },
  ];
  return (
    <Panel className="flex flex-col gap-2 p-3" data-testid="overview-token-mix" data-available={String(available)} data-quality={available ? quality : 'unavailable'}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="text-xs font-semibold text-text">Token mix · terminal totals</span>
        <span className="font-mono text-[9px] tabular-nums text-text-muted">{tokens.samples} usage samples</span>
      </div>
      {!available ? (
        <p className="px-1 py-2 text-xs italic text-text-muted" data-testid="overview-token-mix-unavailable" data-quality="unavailable">No usage-bearing terminal flows · {DASH}</p>
      ) : (
        <>
          <div className="flex h-2 overflow-hidden rounded-full bg-line/40" aria-hidden data-testid="overview-token-mix-bar">
            {exclusive.map((item) => <span key={item.key} className={item.color} style={{ width: `${total > 0 ? item.value / total * 100 : 0}%` }} />)}
          </div>
          <dl className="grid grid-cols-2 gap-x-3 gap-y-1">
            {fields.map((field) => {
              const value = tokens[field.key];
              const fieldQuality = value === null ? 'unavailable' : quality === 'partial' ? 'partial' : 'measured';
              return (
                <div className="contents" key={field.key}>
                  <dt className="flex items-center gap-1 text-[11px] text-text-muted"><span className={cn('h-2 w-2 rounded-sm', field.color)} />{field.label}</dt>
                  <dd className={cn('text-right font-mono text-[11px] tabular-nums', QUALITY_CLASS[fieldQuality])} data-testid={`overview-token-${field.key}`} data-quality={fieldQuality}>{fmtTokens(value)}</dd>
                </div>
              );
            })}
          </dl>
        </>
      )}
    </Panel>
  );
}

function showFlows(facet: FlowFacet, value: string): void {
  const scope = readHashScope();
  const current: FlowFilters = {
    status: scope.status,
    model: scope.model,
    upstream: scope.upstream,
    client: scope.client,
  };
  const next: FlowFilters = facet === 'failure'
    ? { ...current, status: 'failed' as FlowStatus }
    : { ...current, [facet]: value };
  // Hydrate immediately for the destination view, then write one hash navigation carrying the
  // selected window plus every pre-existing facet. This also works from a pasted deep link before
  // App's hash→store synchronization effect has had a chance to run.
  flowFilterStore.getState().hydrate(next);
  navigate('flows', null, { ...scope, ...next });
}

function costQuality(cost: OverviewCost, aggregate: OverviewDataQuality): DisplayQuality {
  if (cost.samples === 0 || cost.total_usd === null || cost.confidence === 'unavailable') return 'unavailable';
  if (aggregate === 'partial') return 'partial';
  return cost.confidence === 'estimated' ? 'estimated' : 'derived';
}

function QualityBadge({ quality }: { quality: DisplayQuality }) {
  return <span className={cn('text-[9px] font-semibold uppercase tracking-wide', QUALITY_CLASS[quality])} data-quality={quality}>{quality}</span>;
}

function SmallBadge({ text }: { text: string }) {
  return <span className="rounded-sm bg-status-cooling/15 px-1 text-[8px] uppercase tracking-wide text-status-cooling">{text}</span>;
}

function humanize(value: string): string {
  return value.replaceAll('_', ' ');
}

function formatCut(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : 'Unknown transport error';
}
