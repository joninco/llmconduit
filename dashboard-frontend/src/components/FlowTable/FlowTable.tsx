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
import { useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import type { FlowSummary, ModelPrice } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { cn } from '../../lib/cn';
import { StatusChip } from './StatusChip';
import { fmtClock, fmtCost, fmtElapsed, fmtModelPair, fmtTokens } from './format';
import { elapsedMs, flowCost, isFailover, shortId, statusClass } from './flowModel';
import { FilterBar } from './FilterBar';
import { EMPTY_FILTERS, type FlowFilters } from './filterTypes';
import { useFlowRows } from './useFlowRows';

const ROW_HEIGHT = 30;
const OVERSCAN = 12;

/** Best-effort client label from the row — the mock has none, so fall back to the method. */
function clientLabel(flow: FlowSummary): string {
  return flow.method;
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
  const [filters, setFilters] = useState<FlowFilters>(EMPTY_FILTERS);
  const { rows, total, models, upstreams } = useFlowRows(filters);
  const priceTable = useDashboard((s) => s.priceTable);

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
    </div>
  );
}

function HeaderRow() {
  return (
    <div
      className={cn(
        COLS.grid,
        'border-b border-line bg-panel-raised py-1.5 text-[10px] uppercase tracking-wide text-text-muted',
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
  selected,
  onSelect,
}: {
  flow: FlowSummary;
  priceTable: Record<string, ModelPrice>;
  selected: boolean;
  onSelect: (id: string) => void;
}) {
  const klass = statusClass(flow.status, flow.terminal_reason);
  const isError = klass === 'client-error' || klass === 'server-error';
  const failover = isFailover(flow);
  const cost = flowCost(flow, priceTable);
  const tokensIn = flow.usage?.prompt;
  const tokensOut = flow.usage?.completion;

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
        selected ? 'bg-accent/15' : 'hover:bg-panel-raised/60',
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
      <span className="text-right tabular-nums text-text-muted">
        {fmtTokens(tokensIn)}<span className="text-line"> / </span>{fmtTokens(tokensOut)}
      </span>
      <span className="text-right tabular-nums text-meta">{fmtCost(cost)}</span>
      <span className="text-right tabular-nums text-text-muted">{fmtElapsed(elapsedMs(flow, Date.now()))}</span>
    </button>
  );
}
