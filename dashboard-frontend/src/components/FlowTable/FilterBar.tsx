/**
 * Quick-chip filter bar for the FlowTable: status (the four FlowStatus values), model, and
 * upstream target. Each group is a row of toggle chips; the active chip filters the list. The
 * model/upstream options are derived from the rows in view (only values actually present), so
 * the bar stays relevant as flows arrive.
 */
import { FLOW_STATUSES } from '../../api/types';
import { cn } from '../../lib/cn';
import type { FlowFilters } from './filterTypes';

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

  return (
    <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-line bg-panel px-3 py-2" data-testid="flow-filter-bar">
      <FilterGroup label="status">
        {FLOW_STATUSES.map((s) => (
          <Chip key={s} active={filters.status === s} onClick={() => toggle('status', s)}>
            {s}
          </Chip>
        ))}
      </FilterGroup>
      {models.length > 0 && (
        <FilterGroup label="model">
          {models.map((m) => (
            <Chip key={m} active={filters.model === m} onClick={() => toggle('model', m)}>
              {m}
            </Chip>
          ))}
        </FilterGroup>
      )}
      {upstreams.length > 0 && (
        <FilterGroup label="upstream">
          {upstreams.map((u) => (
            <Chip key={u} active={filters.upstream === u} onClick={() => toggle('upstream', u)}>
              {u}
            </Chip>
          ))}
        </FilterGroup>
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
