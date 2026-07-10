import { useFlowFilter } from '../store/hooks';
import { flowFilterStore } from '../store/flowFilterStore';
import { useFlowRows } from './FlowTable/useFlowRows';
import { useHashScope } from '../router/useHashRoute';
import { useEffect, useRef, useState } from 'react';

/** Shared scope disclosure shown before every dashboard summary/view. */
export function ScopeBar() {
  const filters = useFlowFilter((state) => state.filters);
  const scope = useHashScope();
  const { rows, total } = useFlowRows(filters);
  const active = [
    filters.status && ['status', filters.status],
    filters.model && ['model', filters.model],
    filters.upstream && ['upstream', filters.upstream],
    filters.client && ['client', filters.client],
  ].filter(Boolean) as [string, string][];
  const scoped = active.length > 0;
  const coverage = total > 0 ? Math.round((rows.length / total) * 100) : null;
  const filterKey = JSON.stringify(filters);
  const previousFilterKey = useRef(filterKey);
  const [announcement, setAnnouncement] = useState('');
  useEffect(() => {
    if (previousFilterKey.current === filterKey) return;
    previousFilterKey.current = filterKey;
    setAnnouncement(`Filters updated. Showing ${rows.length} of ${total} flows.`);
  }, [filterKey, rows.length, total]);

  return (
    <section
      className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b border-line bg-panel/70 px-4 py-1 text-[11px]"
      aria-label="Dashboard scope"
      tabIndex={0}
      data-testid="scope-bar"
    >
      <span className="shrink-0 font-semibold uppercase tracking-[0.14em] text-text">{scope.window}</span>
      <span className="shrink-0 rounded border border-accent/40 px-1.5 py-0.5 text-accent">
        Flow rollups · {scoped ? 'Scoped' : 'Global'}
      </span>
      <span className="shrink-0 rounded border border-line px-1.5 py-0.5 text-text-muted">
        Provider health · Global
      </span>
      {active.map(([key, value]) => (
        <button
          key={key}
          className="shrink-0 rounded-full border border-line bg-panel-raised px-2 py-0.5 text-text hover:border-accent"
          onClick={() => flowFilterStore.getState().setFilters({ ...filters, [key]: null })}
          aria-label={`Remove ${key} filter ${value}`}
        >
          {key}: {value} ×
        </button>
      ))}
      <span className="ml-auto shrink-0 text-text-muted">
        {rows.length}/{total} flows{coverage === null ? ' · coverage —' : ` · ${coverage}% coverage`}
      </span>
      {scoped && (
        <button
          className="shrink-0 rounded px-2 py-0.5 text-accent hover:bg-accent/10"
          onClick={() => flowFilterStore.getState().clear()}
        >
          Clear all
        </button>
      )}
      <span className="sr-only" aria-live="polite" aria-atomic="true" data-testid="scope-announcement">
        {announcement}
      </span>
    </section>
  );
}
