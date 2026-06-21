/**
 * Quick-chip filter bar for the FlowTable: status (the four FlowStatus values), model, and
 * upstream target. Each group is a row of toggle chips; the active chip filters the list. The
 * model/upstream options are derived from the rows in view (only values actually present), so
 * the bar stays relevant as flows arrive.
 *
 * ALWAYS-VISIBLE ACTIVE FILTER (D12 R5 MED): a Topology/Sankey cross-link can SET a model/upstream
 * filter to a value with NO matching flow in view (e.g. an idle provider, or a model whose flows
 * aged out). Since the chip options are derived ONLY from rows in view, that active value would
 * render no chip — an INVISIBLE filter the user cannot toggle off, leaving the table stuck empty.
 * So we fold the currently-SELECTED value into each group's option list even when no row matches it,
 * guaranteeing it always shows as an active (toggle-off-able) chip; a "clear" control also appears
 * whenever any facet is active as a single-click escape hatch.
 */
import { FLOW_STATUSES } from '../../api/types';
import { cn } from '../../lib/cn';
import { EMPTY_FILTERS, type FlowFilters } from './filterTypes';

/** Union the derived options with the active value (if any) so an unmatched selection stays visible. */
function withSelected(options: string[], selected: string | null): string[] {
  if (!selected || options.includes(selected)) return options;
  // Surface the orphaned active value FIRST so it reads as "the filter you set" before the in-view
  // options; the rest keep their sorted (caller-supplied) order.
  return [selected, ...options];
}

function Chip({ active, onClick, children }: { active: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={cn(
        'rounded-full border px-2.5 py-0.5 text-xs transition-colors',
        active
          ? 'border-accent/40 bg-accent/15 text-accent'
          : 'border-line bg-panel text-text-muted hover:text-text',
      )}
    >
      {children}
    </button>
  );
}

export function FilterBar({
  filters,
  models,
  upstreams,
  total,
  shown,
  onChange,
}: {
  filters: FlowFilters;
  models: string[];
  upstreams: string[];
  total: number;
  shown: number;
  onChange: (next: FlowFilters) => void;
}) {
  const toggle = <K extends keyof FlowFilters>(key: K, value: FlowFilters[K]) =>
    onChange({ ...filters, [key]: filters[key] === value ? null : value });

  // Fold the active value into each list so a cross-linked selection with no matching row in view
  // still renders an (active, toggle-off-able) chip instead of becoming an invisible filter.
  const modelOptions = withSelected(models, filters.model);
  const upstreamOptions = withSelected(upstreams, filters.upstream);
  const anyActive = filters.status !== null || filters.model !== null || filters.upstream !== null;

  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-line bg-panel px-3 py-2" data-testid="flow-filter-bar">
      <FilterGroup label="status">
        {FLOW_STATUSES.map((s) => (
          <Chip key={s} active={filters.status === s} onClick={() => toggle('status', s)}>
            {s}
          </Chip>
        ))}
      </FilterGroup>
      {modelOptions.length > 0 && (
        <FilterGroup label="model">
          {modelOptions.map((m) => (
            <Chip key={m} active={filters.model === m} onClick={() => toggle('model', m)}>
              {m}
            </Chip>
          ))}
        </FilterGroup>
      )}
      {upstreamOptions.length > 0 && (
        <FilterGroup label="upstream">
          {upstreamOptions.map((u) => (
            <Chip key={u} active={filters.upstream === u} onClick={() => toggle('upstream', u)}>
              {u}
            </Chip>
          ))}
        </FilterGroup>
      )}
      {/* Single-click escape hatch: always available while any facet is active, so even an
          off-screen / unmatched selection is clearable without hunting for its chip. */}
      {anyActive && (
        <button
          type="button"
          onClick={() => onChange(EMPTY_FILTERS)}
          className="rounded-full border border-line bg-panel px-2.5 py-0.5 text-xs text-text-muted transition-colors hover:text-text"
          data-testid="flow-filter-clear"
        >
          clear
        </button>
      )}
      <span className="ml-auto tabular-nums text-xs text-text-muted" data-testid="flow-count">
        {shown === total ? `${total} flows` : `${shown} / ${total}`}
      </span>
    </div>
  );
}

function FilterGroup({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-1.5">
      <span className="text-[10px] uppercase tracking-wide text-text-muted">{label}</span>
      {children}
    </div>
  );
}
