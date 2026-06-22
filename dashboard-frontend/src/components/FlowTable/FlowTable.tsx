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
import { useRef } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import type { FlowSummary, ModelPrice } from '../../api/types';
import { useDashboard, useFlowFilter } from '../../store/hooks';
import { flowFilterStore } from '../../store/flowFilterStore';
import { cn } from '../../lib/cn';
import { StatusChip } from './StatusChip';
import { TokensCell } from './TokensCell';
import { CacheEconomics } from './CacheEconomics';
import { fmtClock, fmtElapsed, fmtModelPair } from './format';
import { costDisplay, elapsedMs, flowCost, isFailover, shortId, statusClass } from './flowModel';
import { FilterBar } from './FilterBar';
import { useFlowRows } from './useFlowRows';

const ROW_HEIGHT = 30;
const OVERSCAN = 12;

/**
 * HONEST client (user-agent) label for the row. The flow summary does NOT yet carry a
 * user-agent — surfacing one is a separate D13 backend task — so this renders a real
 * user-agent/client field IF the summary ever gains one, else the unavailable marker. It must
 * NOT mislabel the HTTP method (`POST`) as the client, which it previously did (finding 6).
 */
function clientLabel(flow: FlowSummary): string {
  // Read defensively: the field is not on `FlowSummary` today, but if the backend adds one of
  // these we surface it without a code change. Until then this honestly renders "—".
  const f = flow as FlowSummary & { client?: string | null; user_agent?: string | null };
  return f.client ?? f.user_agent ?? '—';
}

interface ColumnWidths {
  grid: string;
}
// 10-column dense grid. tabular-nums on numeric cells keeps columns aligned.
const COLS: ColumnWidths = {
  grid: 'grid grid-cols-[88px_92px_56px_minmax(120px,1fr)_minmax(150px,1.4fr)_96px_84px_120px_72px_72px] gap-2 px-3',
};

export function FlowTable({
  selectedId,
  onSelect,
}: {
  selectedId: string | null;
  onSelect: (apiCallId: string) => void;
}) {
  // The filter lives in the SHARED store (D12) so Topology/Sankey clicks can drive it; the
  // FilterBar below remains the in-table editor (its onChange writes the same store).
  const filters = useFlowFilter((s) => s.filters);
  const setFilters = flowFilterStore.getState().setFilters;
  const { rows, total, models, upstreams } = useFlowRows(filters);
  const priceTable = useDashboard((s) => s.priceTable);
  // SEEK coherence (finding 6): while seeking, an OPEN row's elapsed must derive from the FROZEN
  // cut `at_ms` (the snapshot instant) rather than wall-clock `Date.now()`, which would tick the
  // frozen view forward. `seekAtMs` is null while LIVE → rows fall back to `Date.now()` per render.
  const seekAtMs = useDashboard((s) => s.seekAtMs);

  const scrollRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: OVERSCAN,
  });

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <FilterBar
        filters={filters}
        models={models}
        upstreams={upstreams}
        total={total}
        shown={rows.length}
        onChange={setFilters}
      />
      <HeaderRow />
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto" data-testid="flow-table-scroll">
        <div style={{ height: `${virtualizer.getTotalSize()}px`, position: 'relative', width: '100%' }}>
          {virtualizer.getVirtualItems().map((vi) => {
            const flow = rows[vi.index];
            if (!flow) return null;
            return (
              <div
                key={flow.api_call_id}
                data-index={vi.index}
                data-testid="flow-row"
                style={{
                  position: 'absolute',
                  top: 0,
                  left: 0,
                  width: '100%',
                  height: `${ROW_HEIGHT}px`,
                  transform: `translateY(${vi.start}px)`,
                }}
              >
                <FlowRow
                  flow={flow}
                  priceTable={priceTable}
                  nowMs={seekAtMs ?? Date.now()}
                  selected={flow.api_call_id === selectedId}
                  onSelect={onSelect}
                />
              </div>
            );
          })}
        </div>
        {rows.length === 0 && (
          <div className="px-3 py-6 text-center text-xs text-text-muted" data-testid="flow-table-empty">
            No flows match the current filters.
          </div>
        )}
      </div>
      {/* Gap 08: the AGGREGATE cache-hit rate / "$ saved" by model, rolled up over the SAME filtered
          rows the table shows. A collapsed secondary surface under the table (never inside the
          virtualized scroll container, so it does not affect row layout). */}
      <CacheEconomics rows={rows} priceTable={priceTable} />
    </div>
  );
}

function HeaderRow() {
  return (
    <div
      className={cn(
        COLS.grid,
        'border-b border-line bg-panel-raised py-1.5 text-[10px] uppercase tracking-[0.14em] text-text-muted',
      )}
    >
      <span>time</span>
      <span>id</span>
      <span>client</span>
      <span>endpoint</span>
      <span>model</span>
      <span>upstream</span>
      <span>status</span>
      <span className="text-right">tokens</span>
      <span className="text-right">cost</span>
      <span className="text-right">elapsed</span>
    </div>
  );
}

function FlowRow({
  flow,
  priceTable,
  nowMs,
  selected,
  onSelect,
}: {
  flow: FlowSummary;
  priceTable: Record<string, ModelPrice>;
  /** Reference instant for an OPEN row's elapsed: the frozen cut `at_ms` while seeking, else now. */
  nowMs: number;
  selected: boolean;
  onSelect: (id: string) => void;
}) {
  const klass = statusClass(flow.status, flow.terminal_reason);
  const isError = klass === 'client-error' || klass === 'server-error';
  const failover = isFailover(flow);
  // Gap 07: derive the dollar STRING and the `estimated` flag TOGETHER from the cost + the per-flow
  // `cost_confidence`, so an `estimated` row is visibly labelled and an `unavailable` one renders
  // `—` (never a fabricated `$0.00`) — the same contract the StatsStrip $/min chip + FlowDetail use.
  const cost = costDisplay(flowCost(flow, priceTable), flow.cost_confidence);

  return (
    <button
      type="button"
      onClick={() => onSelect(flow.api_call_id)}
      data-selected={selected || undefined}
      title={flow.api_call_id}
      className={cn(
        COLS.grid,
        'h-full w-full items-center border-b border-line/50 text-left text-xs',
        // No transition on layout properties — only background color, so virtualized rows
        // recycling positions never trigger a FLIP.
        'transition-colors',
        isError ? 'text-status-down' : 'text-text',
        selected ? 'bg-accent/12' : 'hover:bg-accent/[0.06]',
      )}
    >
      <span className="tabular-nums text-text-muted">{fmtClock(flow.started_ms)}</span>
      <span className="truncate font-mono text-text-muted">{shortId(flow.api_call_id)}</span>
      <span className="truncate text-text-muted">{clientLabel(flow)}</span>
      <span className="truncate font-mono">{flow.uri || '—'}</span>
      <span className="flex min-w-0 items-center gap-1.5">
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
      <span className="truncate text-text-muted">{flow.upstream_target ?? '—'}</span>
      <span>
        <StatusChip status={flow.status} terminalReason={flow.terminal_reason} />
      </span>
      <TokensCell flow={flow} priceTable={priceTable} />
      <span className="flex items-center justify-end gap-1 text-right tabular-nums text-meta">
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
      <span className="text-right tabular-nums text-text-muted">{fmtElapsed(elapsedMs(flow, nowMs))}</span>
    </button>
  );
}
