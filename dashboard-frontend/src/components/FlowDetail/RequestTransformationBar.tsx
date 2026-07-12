import type { ReactNode } from 'react';
import type { ChangeSummary } from './diff';

export type RequestViewMode = 'changes' | 'all';

export interface TransformationHop {
  label: string;
  available: boolean;
  summary: ChangeSummary;
}

interface RequestTransformationBarProps {
  query: string;
  onQueryChange: (query: string) => void;
  mode: RequestViewMode;
  onModeChange: (mode: RequestViewMode) => void;
  normalization: TransformationHop;
  lowering: TransformationHop;
}

/**
 * Shared control + orientation rail for the three request representations. The old UI left the
 * operator to infer red/green backgrounds; this names each stage and reports the actual operation
 * counts at each hop before they inspect individual rows.
 */
export function RequestTransformationBar({
  query,
  onQueryChange,
  mode,
  onModeChange,
  normalization,
  lowering,
}: RequestTransformationBarProps) {
  const totalOperations = normalization.summary.total + lowering.summary.total;
  return (
    <div className="shrink-0 border-b border-line bg-panel-raised" data-testid="request-transformation-bar">
      <div className="flex items-center gap-2 border-b border-line/70 px-3 py-1.5">
        <div className="flex min-w-0 flex-1 items-center gap-2 rounded-md border border-line bg-panel px-2 py-1 transition-colors focus-within:border-accent/60">
          <svg viewBox="0 0 16 16" className="h-3.5 w-3.5 shrink-0 text-text-muted" fill="none" aria-hidden="true">
            <circle cx="7" cy="7" r="4.5" stroke="currentColor" strokeWidth="1.5" />
            <path d="M10.5 10.5 14 14" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
          </svg>
          <input
            type="text"
            value={query}
            onChange={(event) => onQueryChange(event.target.value)}
            placeholder="search every representation…"
            spellCheck={false}
            className="min-w-0 flex-1 bg-transparent font-mono text-xs text-text placeholder:text-text-muted focus:outline-none"
            data-testid="json-search-input"
          />
          {query && (
            <button
              type="button"
              onClick={() => onQueryChange('')}
              aria-label="clear search"
              className="shrink-0 text-text-muted transition-colors hover:text-text"
            >
              ✕
            </button>
          )}
        </div>
        <div
          className="flex shrink-0 rounded-md border border-line bg-panel p-0.5 font-mono text-[10px] uppercase tracking-[0.08em]"
          role="group"
          aria-label="Request representation view"
        >
          <ViewButton active={mode === 'changes'} onClick={() => onModeChange('changes')} testId="request-view-changes">
            changes <span className="ml-1 text-[9px] opacity-75">{totalOperations}</span>
          </ViewButton>
          <ViewButton active={mode === 'all'} onClick={() => onModeChange('all')} testId="request-view-all">
            all JSON
          </ViewButton>
        </div>
      </div>

      <div
        className="overflow-x-auto focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent"
        data-testid="request-stage-rail"
        tabIndex={0}
        aria-label="Request transformation stages"
      >
        <div className="grid min-w-[760px] grid-cols-[minmax(116px,1fr)_minmax(168px,1.15fr)_minmax(116px,1fr)_minmax(168px,1.15fr)_minmax(116px,1fr)] items-center gap-2 px-3 py-2">
          <Stage step="A" title="Client payload" subtitle="captured at ingress" />
          <Hop hop={normalization} />
          <Stage step="B" title="Gateway canonical" subtitle="after adapter + policies" />
          <Hop hop={lowering} />
          <Stage step="C" title="Provider payload" subtitle="captured at dispatch" />
        </div>
      </div>
    </div>
  );
}

function ViewButton({
  active,
  onClick,
  testId,
  children,
}: {
  active: boolean;
  onClick: () => void;
  testId: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-pressed={active}
      onClick={onClick}
      data-testid={testId}
      className={`rounded px-2 py-1 transition-colors ${active ? 'bg-accent/25 text-text' : 'text-text-muted hover:text-text'}`}
    >
      {children}
    </button>
  );
}

function Stage({ step, title, subtitle }: { step: 'A' | 'B' | 'C'; title: string; subtitle: string }) {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-accent/50 font-mono text-[10px] font-semibold text-accent">
        {step}
      </span>
      <span className="min-w-0 leading-tight">
        <span className="block truncate text-[10px] font-semibold uppercase tracking-[0.1em] text-text">{title}</span>
        <span className="block truncate text-[9px] text-text-muted">{subtitle}</span>
      </span>
    </div>
  );
}

function Hop({ hop }: { hop: TransformationHop }) {
  const accessible = hop.available
    ? hop.summary.total === 0
      ? `${hop.label}: no structural changes`
      : `${hop.label}: ${hop.summary.added} introduced, ${hop.summary.changed} rewritten, ${hop.summary.removed} omitted`
    : `${hop.label}: capture unavailable`;
  return (
    <div className="flex min-w-0 items-center gap-1.5" aria-label={accessible} data-testid={`request-hop-${hop.label}`}>
      <span className="h-px min-w-2 flex-1 bg-line" aria-hidden />
      <span className="min-w-0 rounded-md border border-line bg-panel px-2 py-1 text-center leading-tight">
        <span className="block text-[8px] uppercase tracking-[0.12em] text-text-muted">{hop.label}</span>
        <span className="mt-0.5 flex items-center justify-center gap-1.5 whitespace-nowrap font-mono text-[9px]">
          {!hop.available ? (
            <span className="text-text-muted">capture unavailable</span>
          ) : hop.summary.total === 0 ? (
            <span className="text-text-muted">no structural change</span>
          ) : (
            <>
              {hop.summary.added > 0 && <Operation glyph="+" count={hop.summary.added} label="new" tone="text-accent" />}
              {hop.summary.changed > 0 && <Operation glyph="~" count={hop.summary.changed} label="rewrite" tone="text-status-cooling" />}
              {hop.summary.removed > 0 && <Operation glyph="−" count={hop.summary.removed} label="drop" tone="text-meta" />}
            </>
          )}
        </span>
      </span>
      <span className="text-[11px] text-text-muted" aria-hidden>→</span>
    </div>
  );
}

function Operation({ glyph, count, label, tone }: { glyph: string; count: number; label: string; tone: string }) {
  return (
    <span className={tone} title={`${count} ${label}`}>
      <span className="font-semibold" aria-hidden>{glyph}</span>{count} {label}
    </span>
  );
}
