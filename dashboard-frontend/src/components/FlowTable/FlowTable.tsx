/**
 * Virtualized flow table (mitmweb-style), newest-on-top. Columns: timestamp, short api_call_id,
 * client (user-agent), endpoint, requested→served model, upstream target, status chip, tokens
 * in/out, cost, elapsed. Error rows are red; running rows pulse (dot only); failover rows are
 * tagged. Driven by `useFlowRows` (live WS store ∪ `/flows` query). Row click selects the flow.
 *
 * Virtualization (`@tanstack/react-virtual`, fixed row height) keeps 10k rows smooth: only the
 * visible window + overscan is in the DOM. CRITICAL (D10): rows carry NO layout/FLIP transition —
 * only the status dot animates — so scrolling never thrashes. The header is a sibling of the
 * scroll container (not virtualized) so it stays put.
 */
import { useCallback, useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import type { FlowSummary } from '../../api/types';
import { useDashboard, useFlowFilter } from '../../store/hooks';
import { flowFilterStore } from '../../store/flowFilterStore';
import { cn } from '../../lib/cn';
import { StatusChip } from './StatusChip';
import { TokensCell } from './TokensCell';
import { CacheEconomics } from './CacheEconomics';
import { ContextPressure } from './ContextPressure';
import { fmtClock, fmtElapsed, fmtModelPair } from './format';
import { costDisplay, elapsedMs, flowCost, isFailover, shortId, statusClass } from './flowModel';
import { clientCell } from './clientAttribution';
import { ClientRollup } from './ClientRollup';
import { FilterBar } from './FilterBar';
import { useFlowRows } from './useFlowRows';
import { useCatalog } from './useCatalog';
import { useMediaQuery } from '../../lib/useMediaQuery';

const ROW_HEIGHT = 30;
const CARD_HEIGHT = 144;
const OVERSCAN = 12;
const COLUMN_COUNT = 10;
const NARROW_QUERY = '(max-width: 1023px)';

/**
 * The CLIENT cell (gap 15): renders the flow's NON-SECRET `client_label` (a key-hash `key-<hex>`, a
 * configured caller-id, or a WEAK User-Agent fallback) with a source-strength marker. The weak UA
 * fallback is rendered VISIBLY weaker (dimmed/italic + a `ua` badge) so it never reads as a confirmed
 * identity; a strong key-hash / configured-id carries its source badge. An UNATTRIBUTED flow renders
 * `—` (don't-lie-with-zeros — never a fabricated id). A raw key never reaches here (gap 04 hashes it).
 */
function ClientCellView({ flow }: { flow: FlowSummary }) {
  const cell = clientCell(flow);
  return (
    <span
      className="flex min-w-0 items-center gap-1"
      data-testid="flow-client"
      data-quality={cell.quality}
      data-strength={cell.strength}
      data-attributed={cell.attributed ? 'true' : 'false'}
      title={cell.detail}
    >
      <span className={cn('truncate text-text-muted', cell.weak && 'italic border-l border-status-cooling pl-1')}>
        {cell.label}
      </span>
      {cell.badge && (
        <span
          className={cn(
            'shrink-0 rounded-sm px-1 text-[9px] uppercase tracking-wide',
            // The WEAK UA fallback is visually distinct (cooling/amber) from a strong identity (neutral).
            cell.weak
              ? 'bg-status-cooling/15 text-status-cooling'
              : 'bg-line/40 text-text-muted',
          )}
          data-testid="flow-client-source"
          data-source={cell.source ?? undefined}
          title={cell.weak ? `weak ${cell.sourceLabel} fallback — not a confirmed identity` : `${cell.sourceLabel} (strong identity)`}
        >
          {cell.badge}
        </span>
      )}
    </span>
  );
}

interface ColumnWidths {
  grid: string;
}
// 10-column dense grid. tabular-nums on numeric cells keeps columns aligned. The CLIENT column (3rd)
// is a responsive `minmax(120px,0.9fr)` — NOT a fixed 56px (which truncated `key-9f3a1c0b2d4e` /
// `python-httpx/0.27` to a non-distinguishing prefix, defeating gap 15's purpose) — so seeded clients
// are visually distinguishable; the endpoint flex is trimmed to keep the grid balanced.
const COLS: ColumnWidths = {
  grid: 'grid grid-cols-[88px_92px_minmax(120px,0.9fr)_minmax(110px,0.9fr)_minmax(150px,1.4fr)_96px_84px_120px_72px_72px] gap-2 px-3',
};

export function FlowTable({
  selectedId,
  onSelect,
  searchQuery = '',
  onSearchQueryChange = () => {},
}: {
  selectedId: string | null;
  onSelect: (apiCallId: string) => void;
  /** Flows-only lookup; kept out of global hash scope because server rollups cannot apply it. */
  searchQuery?: string;
  onSearchQueryChange?: (query: string) => void;
}) {
  // The filter lives in the SHARED store (D12) so Topology/Sankey clicks can drive it; the
  // FilterBar below remains the in-table editor (its onChange writes the same store).
  const filters = useFlowFilter((s) => s.filters);
  const setFilters = flowFilterStore.getState().setFilters;
  const {
    rows, total, models, upstreams, clients, loadState, retry, hasMore, loadingMore, loadMore, summary,
    populationKnown,
  } = useFlowRows(filters, searchQuery);
  // Gap 09: the per-model context-window capacities (gap-06 nullable `context_limit`), for the
  // aggregate context-pressure stat. A `null`/absent window is UNKNOWN ⇒ that flow is excluded from
  // the pressure figures (never a fabricated 0%/100%).
  const contextLimits = useCatalog();
  // SEEK coherence (finding 6): while seeking, an OPEN row's elapsed must derive from the FROZEN
  // cut `at_ms` (the snapshot instant) rather than wall-clock `Date.now()`, which would tick the
  // frozen view forward. `seekAtMs` is null while LIVE → rows fall back to `Date.now()` per render.
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const narrow = useMediaQuery(NARROW_QUERY);
  const filtered = filters.status !== null
    || filters.model !== null
    || filters.upstream !== null
    || filters.client !== null
    || searchQuery.trim().length > 0;
  const seeking = useDashboard((s) => s.connection === 'seeking');

  const scrollRef = useRef<HTMLDivElement>(null);
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());
  const [activeRowId, setActiveRowId] = useState<string | null>(null);
  const activeIndex = Math.max(
    0,
    rows.findIndex((flow) => flow.api_call_id === activeRowId),
  );
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => (narrow ? CARD_HEIGHT : ROW_HEIGHT),
    overscan: OVERSCAN,
  });
  useEffect(() => virtualizer.measure(), [narrow, virtualizer]);

  const registerRow = useCallback((apiCallId: string, node: HTMLButtonElement | null) => {
    if (node) rowRefs.current.set(apiCallId, node);
    else rowRefs.current.delete(apiCallId);
  }, []);

  const focusRow = useCallback(
    (index: number) => {
      if (rows.length === 0) return;
      const nextIndex = Math.min(rows.length - 1, Math.max(0, index));
      const apiCallId = rows[nextIndex]?.api_call_id;
      if (!apiCallId) return;
      setActiveRowId(apiCallId);
      virtualizer.scrollToIndex(nextIndex, { align: 'auto' });

      const focus = () => rowRefs.current.get(apiCallId)?.focus();
      if (rowRefs.current.has(apiCallId)) focus();
      else requestAnimationFrame(focus);
    },
    [rows, virtualizer],
  );

  const onRowKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLButtonElement>, index: number, apiCallId: string) => {
      let nextIndex: number | null = null;
      switch (event.key) {
        case 'ArrowDown':
        case 'ArrowRight':
          nextIndex = index + 1;
          break;
        case 'ArrowUp':
        case 'ArrowLeft':
          nextIndex = index - 1;
          break;
        case 'Home':
          nextIndex = 0;
          break;
        case 'End':
          nextIndex = rows.length - 1;
          break;
        case 'Enter':
        case ' ':
          event.preventDefault();
          onSelect(apiCallId);
          return;
        default:
          return;
      }
      event.preventDefault();
      focusRow(nextIndex);
    },
    [focusRow, onSelect, rows.length],
  );

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <FilterBar
        filters={filters}
        models={models}
        upstreams={upstreams}
        clients={clients}
        total={total}
        shown={rows.length}
        onChange={setFilters}
        searchQuery={searchQuery}
        onSearchQueryChange={onSearchQueryChange}
      />
      {loadState === 'error' && total > 0 && !seeking && (
        <div
          className="flex items-center gap-3 border-b border-status-cooling/40 bg-status-cooling/10 px-3 py-2 text-xs text-text"
          role="alert"
          data-testid="flow-table-stale"
        >
          <span>Flow history could not refresh. Showing available live data, which may be incomplete.</span>
          <button type="button" className="ml-auto rounded text-accent underline" onClick={retry}>Retry</button>
        </div>
      )}
      <div
        className="flex min-h-0 flex-1 flex-col"
        role={narrow ? undefined : 'grid'}
        aria-label={narrow ? undefined : 'Flows'}
        aria-colcount={narrow ? undefined : COLUMN_COUNT}
        aria-rowcount={narrow ? undefined : rows.length + 1}
        data-testid={narrow ? 'flow-card-list' : 'flow-grid'}
      >
        {!narrow && <HeaderRow />}
        <div
          ref={scrollRef}
          className={cn('flex-1 overflow-auto', narrow ? 'min-h-64' : 'min-h-0')}
          data-testid="flow-table-scroll"
          role={narrow ? 'list' : 'rowgroup'}
          aria-label={narrow ? 'Flows' : undefined}
        >
          <div style={{ height: `${virtualizer.getTotalSize()}px`, position: 'relative', width: '100%' }}>
            {virtualizer.getVirtualItems().map((vi) => {
              const flow = rows[vi.index];
              if (!flow) return null;
              return (
                <div
                  key={flow.api_call_id}
                  data-index={vi.index}
                  data-testid="flow-row"
                  role={narrow ? 'listitem' : 'presentation'}
                  style={{
                    position: 'absolute',
                    top: 0,
                    left: 0,
                    width: '100%',
                    height: `${narrow ? CARD_HEIGHT : ROW_HEIGHT}px`,
                    transform: `translateY(${vi.start}px)`,
                  }}
                >
                  {narrow ? (
                    <FlowCard
                      flow={flow}
                      nowMs={seekAtMs ?? Date.now()}
                      selected={flow.api_call_id === selectedId}
                      onSelect={onSelect}
                    />
                  ) : (
                    <FlowRow
                      buttonRef={(node) => registerRow(flow.api_call_id, node)}
                      flow={flow}
                      index={vi.index}
                      nowMs={seekAtMs ?? Date.now()}
                      selected={flow.api_call_id === selectedId}
                      active={vi.index === activeIndex}
                      onFocus={() => setActiveRowId(flow.api_call_id)}
                      onKeyDown={onRowKeyDown}
                      onSelect={onSelect}
                    />
                  )}
                </div>
              );
            })}
          </div>
          {rows.length === 0 && (
            <FlowTableEmptyState
              loadState={loadState}
              filtered={filtered}
              searchQuery={searchQuery}
              populationKnown={populationKnown}
              seeking={seeking}
              onRetry={retry}
            />
          )}
          {rows.length > 0 && hasMore && (
            <div className="sticky bottom-0 flex justify-center border-t border-line bg-panel/95 px-3 py-2 backdrop-blur">
              <button
                type="button"
                className="rounded border border-line bg-panel-raised px-3 py-1 text-xs text-text-muted hover:border-accent/60 hover:text-text disabled:opacity-60"
                onClick={loadMore}
                disabled={loadingMore}
                data-testid="flow-load-more"
              >
                {loadingMore ? 'Loading…' : `Load older requests · ${rows.length} / ${total}`}
              </button>
            </div>
          )}
        </div>
      </div>
      {/* Gap 09: the AGGREGATE context-pressure stat — peak context-window utilization + near/over
          counts across the SAME filtered rows. An always-visible stat under the table (outside the
          virtualized scroll container, so it does not affect row layout). */}
      <ContextPressure rows={rows} limits={contextLimits} archive={summary?.context} archiveTotal={summary?.total} />
      {/* Gap 08: the AGGREGATE cache-hit rate / "$ saved" by model, rolled up over the SAME filtered
          rows the table shows. A collapsed secondary surface under the table (never inside the
          virtualized scroll container, so it does not affect row layout). */}
      <CacheEconomics rows={rows} />
      {/* Gap 15: the AGGREGATE "by client" roll-up — cost / errors / latency per non-secret client
          (key-hash / configured-id / weak-UA), over the SAME filtered rows. A collapsed secondary
          surface under the table; its rows cross-link into the per-client filter. */}
      <ClientRollup rows={rows} archive={summary ?? undefined} />
    </div>
  );
}

function FlowTableEmptyState({
  loadState,
  filtered,
  searchQuery,
  populationKnown,
  seeking,
  onRetry,
}: {
  loadState: 'loading' | 'ready' | 'error';
  filtered: boolean;
  searchQuery: string;
  populationKnown: boolean;
  seeking: boolean;
  onRetry: () => void;
}) {
  // A known in-memory population plus zero matches is already an honest search/filter result even
  // if the background REST reconciliation is loading or stale. Do not replace that useful answer
  // with a generic transport state; the parent renders the stale-data warning when appropriate.
  const populationResolved = populationKnown || loadState === 'ready';
  if (populationResolved && searchQuery.trim()) {
    return (
      <div className="px-3 py-6 text-center text-xs text-text-muted" data-testid="flow-table-search-empty">
        No flows match “{searchQuery.trim()}”. Try an API call ID, response ID, endpoint, model, provider, or client.
      </div>
    );
  }
  if (populationResolved && filtered) {
    return (
      <div className="px-3 py-6 text-center text-xs text-text-muted" data-testid="flow-table-filtered-empty">
        No flows match the current filters.
      </div>
    );
  }
  if (loadState === 'loading') {
    return (
      <div className="px-3 py-6 text-center text-xs text-text-muted" role="status" data-testid="flow-table-loading">
        Loading flows…
      </div>
    );
  }
  if (loadState === 'error') {
    return (
      <div className="px-3 py-6 text-center text-xs text-status-down" role="alert" data-testid="flow-table-error">
        <p>Flows could not be loaded.</p>
        <button type="button" className="mt-2 rounded text-accent underline" onClick={onRetry}>Retry</button>
      </div>
    );
  }
  const message = seeking ? 'No flows in this historical snapshot.' : 'No flows yet. Waiting for requests.';
  return (
    <div
      className="px-3 py-6 text-center text-xs text-text-muted"
      data-testid="flow-table-empty"
    >
      {message}
    </div>
  );
}

function HeaderRow() {
  return (
    <div
      role="row"
      aria-rowindex={1}
      className={cn(
        COLS.grid,
        'border-b border-line bg-panel-raised py-1.5 text-[10px] uppercase tracking-[0.14em] text-text-muted',
      )}
    >
      <span role="columnheader">time</span>
      <span role="columnheader">id</span>
      <span role="columnheader">client</span>
      <span role="columnheader">endpoint</span>
      <span role="columnheader">model</span>
      <span role="columnheader">upstream</span>
      <span role="columnheader">status</span>
      <span role="columnheader" className="text-right">tokens</span>
      <span role="columnheader" className="text-right">cost</span>
      <span role="columnheader" className="text-right">elapsed</span>
    </div>
  );
}

function FlowRow({
  buttonRef,
  flow,
  index,
  nowMs,
  selected,
  active,
  onFocus,
  onKeyDown,
  onSelect,
}: {
  buttonRef: (node: HTMLButtonElement | null) => void;
  flow: FlowSummary;
  index: number;
  /** Reference instant for an OPEN row's elapsed: the frozen cut `at_ms` while seeking, else now. */
  nowMs: number;
  selected: boolean;
  active: boolean;
  onFocus: () => void;
  onKeyDown: (event: ReactKeyboardEvent<HTMLButtonElement>, index: number, apiCallId: string) => void;
  onSelect: (id: string) => void;
}) {
  const klass = statusClass(flow.status, flow.terminal_reason);
  const isError = klass === 'client-error' || klass === 'server-error';
  const failover = isFailover(flow);
  // Gap 07: derive the dollar STRING and the `estimated` flag TOGETHER from the cost + the per-flow
  // `cost_confidence`, so an `estimated` row is visibly labelled and an `unavailable` one renders
  // `—` (never a fabricated `$0.00`) — the same contract the StatsStrip $/min chip + FlowDetail use.
  const cost = costDisplay(flowCost(flow), flow.cost_confidence);

  return (
    <button
      ref={buttonRef}
      type="button"
      role="row"
      aria-rowindex={index + 2}
      aria-selected={selected}
      tabIndex={active ? 0 : -1}
      onClick={() => onSelect(flow.api_call_id)}
      onFocus={onFocus}
      onKeyDown={(event) => onKeyDown(event, index, flow.api_call_id)}
      data-selected={selected || undefined}
      title={flow.api_call_id}
      className={cn(
        COLS.grid,
        'h-full w-full items-center border-b border-line/50 text-left text-xs',
        // No transition on layout properties — only background color, so virtualized rows
        // recycling positions never trigger a FLIP.
        'transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent',
        isError ? 'text-status-down' : 'text-text',
        selected ? 'bg-accent/12' : 'hover:bg-accent/[0.06]',
      )}
    >
      <span role="gridcell" className="tabular-nums text-text-muted">{fmtClock(flow.started_ms)}</span>
      <span role="gridcell" className="truncate font-mono text-text-muted">{shortId(flow.api_call_id)}</span>
      <span role="gridcell" className="min-w-0"><ClientCellView flow={flow} /></span>
      <span role="gridcell" className="truncate font-mono">{flow.uri || '—'}</span>
      <span role="gridcell" className="flex min-w-0 items-center gap-1.5">
        <span className="truncate">{fmtModelPair(flow.model_requested, flow.model_served)}</span>
        {failover && (
          <span
            className="shrink-0 rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase text-status-cooling"
            data-testid="failover-tag"
            title="failover / re-routed"
          >
            FO
          </span>
        )}
      </span>
      <span role="gridcell" className="truncate text-text-muted">{flow.upstream_target ?? '—'}</span>
      <span role="gridcell">
        <StatusChip status={flow.status} terminalReason={flow.terminal_reason} />
      </span>
      <span role="gridcell"><TokensCell flow={flow} /></span>
      <span role="gridcell" className="flex items-center justify-end gap-1 text-right tabular-nums text-meta">
        <span data-testid="flow-cost" data-confidence={cost.confidence}>{cost.value}</span>
        {/* Gap 07: an `estimated` per-flow cost MUST be labelled (the cross-cutting rule) — a
            compact marker so an operator never mistakes a best-effort row for a confident one on
            the main flow surface. `unavailable` already reads as `—`; `confident` needs no badge. */}
        {cost.estimated && (
          <span
            className="shrink-0 rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase tracking-wide text-status-cooling"
            data-testid="flow-cost-est"
            title="cost is an estimate — a billed token class has no configured rate"
          >
            est
          </span>
        )}
      </span>
      <span role="gridcell" className="text-right tabular-nums text-text-muted">{fmtElapsed(elapsedMs(flow, nowMs))}</span>
    </button>
  );
}

/**
 * Narrow-screen composition of the same authoritative row. Cards retain a fixed height so the
 * virtualizer never measures content, while the two-column middle section lets long endpoint,
 * model, and client values truncate instead of forcing horizontal page scroll.
 */
function FlowCard({
  flow,
  nowMs,
  selected,
  onSelect,
}: {
  flow: FlowSummary;
  nowMs: number;
  selected: boolean;
  onSelect: (id: string) => void;
}) {
  const klass = statusClass(flow.status, flow.terminal_reason);
  const isError = klass === 'client-error' || klass === 'server-error';
  const failover = isFailover(flow);
  const cost = costDisplay(flowCost(flow), flow.cost_confidence);

  return (
    <button
      type="button"
      data-testid="flow-card"
      data-selected={selected || undefined}
      aria-current={selected ? 'true' : undefined}
      title={flow.api_call_id}
      onClick={() => onSelect(flow.api_call_id)}
      className={cn(
        'mx-2 mt-2 grid w-[calc(100%_-_1rem)] grid-rows-[auto_auto_auto_1fr] gap-1 rounded-md border border-line bg-panel-raised px-3 py-2 text-left text-xs shadow-sm',
        'transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent',
        isError ? 'text-status-down' : 'text-text',
        selected ? 'border-accent/60 bg-accent/12' : 'hover:border-accent/40 hover:bg-accent/[0.06]',
      )}
      style={{ height: `${CARD_HEIGHT - 12}px` }}
    >
      <span className="flex min-w-0 items-center gap-2">
        <span className="shrink-0 tabular-nums text-text-muted">{fmtClock(flow.started_ms)}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-text-muted">{shortId(flow.api_call_id)}</span>
        <StatusChip status={flow.status} terminalReason={flow.terminal_reason} />
      </span>

      <span className="flex min-w-0 items-center gap-1.5">
        <span className="truncate font-medium">{fmtModelPair(flow.model_requested, flow.model_served)}</span>
        {failover && (
          <span
            className="shrink-0 rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase text-status-cooling"
            data-testid="failover-tag"
            title="failover / re-routed"
          >
            FO
          </span>
        )}
        <span className="ml-auto max-w-[35%] truncate text-text-muted">{flow.upstream_target ?? '—'}</span>
      </span>

      <span className="grid min-w-0 grid-cols-2 gap-3 border-t border-line/60 pt-1">
        <span className="min-w-0 truncate font-mono" title={flow.uri || undefined}>{flow.uri || '—'}</span>
        <ClientCellView flow={flow} />
      </span>

      <span className="grid grid-cols-3 items-end gap-2 text-[10px] uppercase tracking-wide text-text-muted">
        <span className="min-w-0">
          <span className="block">tokens</span>
          <TokensCell flow={flow} />
        </span>
        <span className="text-right">
          <span className="block">cost</span>
          <span className="tabular-nums normal-case text-meta" data-testid="flow-cost" data-confidence={cost.confidence}>
            {cost.value}{cost.estimated ? ' est' : ''}
          </span>
        </span>
        <span className="text-right">
          <span className="block">elapsed</span>
          <span className="tabular-nums normal-case">{fmtElapsed(elapsedMs(flow, nowMs))}</span>
        </span>
      </span>
    </button>
  );
}
