import { useMemo, useState } from 'react';
import type { DebugTimelineEvent } from '../../api/types';
import { fmtClock, fmtElapsed } from '../FlowTable/format';
import { cn } from '../../lib/cn';

type Category = 'errors' | 'routing' | 'provider' | 'failover' | 'tools' | 'first-token' | 'completion';
const CATEGORIES: Category[] = ['errors', 'routing', 'provider', 'failover', 'tools', 'first-token', 'completion'];

function categories(event: DebugTimelineEvent): Set<Category> {
  const text = `${event.kind} ${event.summary}`.toLowerCase();
  const result = new Set<Category>();
  if (/error|failed|timeout|cancel|reject/.test(text)) result.add('errors');
  if (/rout|model.resol|catalog/.test(text)) result.add('routing');
  if (/provider|upstream|dispatch/.test(text)) result.add('provider');
  if (/failover|fallback|retry|attempt/.test(text)) result.add('failover');
  if (/tool|function|search/.test(text)) result.add('tools');
  if (/first.?token|first.?byte|content_part.added|output_text.delta/.test(text)) result.add('first-token');
  if (/completed|complete|done|terminal|incomplete/.test(text)) result.add('completion');
  return result;
}

function severity(event: DebugTimelineEvent): 'error' | 'warning' | 'info' {
  const text = `${event.kind} ${event.summary}`.toLowerCase();
  if (/error|failed|reject/.test(text)) return 'error';
  if (/timeout|cancel|failover|fallback|retry|incomplete/.test(text)) return 'warning';
  return 'info';
}

async function copyText(value: string): Promise<void> {
  await navigator.clipboard?.writeText(value);
}

export function Timeline({ events, startedAtMs }: { events: DebugTimelineEvent[]; startedAtMs?: number | null }) {
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState<Category | 'all'>('all');
  const [expandedSummaries, setExpandedSummaries] = useState(false);
  const [rawOpen, setRawOpen] = useState<Set<number>>(new Set());
  const base = startedAtMs ?? events[0]?.timestamp_ms ?? 0;
  const indexed = useMemo(() => events.map((event, index) => ({
    event,
    index,
    categories: categories(event),
    severity: severity(event),
    delta: index === 0 ? 0 : Math.max(0, event.timestamp_ms - (events[index - 1]?.timestamp_ms ?? event.timestamp_ms)),
    duration: index === events.length - 1 ? null : Math.max(0, (events[index + 1]?.timestamp_ms ?? event.timestamp_ms) - event.timestamp_ms),
  })), [events]);
  const normalized = query.trim().toLowerCase();
  const visible = indexed.filter(({ event, categories: tags }) =>
    (filter === 'all' || tags.has(filter))
    && (!normalized || `${event.kind} ${event.summary} ${event.payload_preview ?? ''}`.toLowerCase().includes(normalized)),
  );

  if (events.length === 0) {
    return <div className="px-3 py-4 text-xs italic text-text-muted" data-testid="timeline-empty">No timeline events yet.</div>;
  }

  const allVisibleRaw = visible.length > 0 && visible.every(({ index }) => rawOpen.has(index));
  const toggleAllRaw = () => setRawOpen(allVisibleRaw ? new Set() : new Set(visible.map(({ index }) => index)));

  return (
    <section className="flex min-h-0 flex-col" data-testid="timeline">
      <div className="sticky top-0 z-10 space-y-2 border-b border-line bg-panel/95 px-3 py-2 backdrop-blur">
        <div className="flex flex-wrap items-center gap-2">
          <label className="min-w-44 flex-1">
            <span className="sr-only">Search timeline</span>
            <input
              type="search"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Search kind, summary, or captured preview"
              className="h-8 w-full rounded border border-line bg-bg px-2 text-xs text-text outline-none focus:border-accent"
            />
          </label>
          <button type="button" className="rounded px-2 py-1 text-xs text-accent hover:bg-accent/10" aria-pressed={expandedSummaries} onClick={() => setExpandedSummaries((open) => !open)}>
            {expandedSummaries ? 'Compact summaries' : 'Expand all summaries'}
          </button>
          <button type="button" className="rounded px-2 py-1 text-xs text-accent hover:bg-accent/10" aria-pressed={allVisibleRaw} onClick={toggleAllRaw}>
            {allVisibleRaw ? 'Collapse raw' : 'Expand all raw'}
          </button>
        </div>
        <div className="flex gap-1 overflow-x-auto" role="group" aria-label="Timeline event filters">
          {(['all', ...CATEGORIES] as const).map((category) => {
            const count = category === 'all' ? indexed.length : indexed.filter((item) => item.categories.has(category)).length;
            return (
              <button
                key={category}
                type="button"
                aria-pressed={filter === category}
                onClick={() => setFilter(category)}
                className={cn('shrink-0 rounded-full border px-2 py-0.5 text-[10px]', filter === category ? 'border-accent bg-accent/15 text-text' : 'border-line text-text-muted')}
              >
                {category} · {count}
              </button>
            );
          })}
        </div>
      </div>

      <ol className="flex flex-col" aria-label={`${visible.length} matching timeline events`}>
        {visible.map(({ event, index, categories: tags, severity: level, delta, duration }) => {
          const open = rawOpen.has(index);
          return (
            <li
              key={`${event.timestamp_ms}-${index}`}
              className={cn('border-b border-line/50 px-3 py-2 [content-visibility:auto]', level === 'error' && 'border-l-2 border-l-status-down', level === 'warning' && 'border-l-2 border-l-status-cooling')}
              data-testid="timeline-event"
              data-severity={level}
            >
              <div className="grid min-w-0 grid-cols-[82px_64px_64px_minmax(88px,auto)_1fr_auto] items-baseline gap-2 text-[10px]">
                <time className="tabular-nums text-text-muted" dateTime={new Date(event.timestamp_ms).toISOString()} title={new Date(event.timestamp_ms).toISOString()}>{fmtClock(event.timestamp_ms)}</time>
                <span className="tabular-nums text-text-muted">+{fmtElapsed(event.timestamp_ms - base)}</span>
                <span className="tabular-nums text-text-muted">Δ {fmtElapsed(delta)}</span>
                <span className="truncate font-mono text-accent" title={event.kind}>{event.kind}</span>
                <span className="truncate text-xs text-text" title={event.summary}>{event.summary}</span>
                <span className="tabular-nums text-text-muted">{duration === null ? 'terminal' : fmtElapsed(duration)}</span>
              </div>
              {expandedSummaries && (
                <div className="mt-1 flex flex-wrap items-center gap-1 text-[10px] text-text-muted">
                  <span>severity: {level}</span>
                  {[...tags].map((tag) => <span key={tag} className="rounded bg-line/40 px-1">{tag}</span>)}
                  {event.images.map((image) => <span key={image.id} className="rounded bg-meta/15 px-1 text-meta" title={image.path}>{image.label} · {image.mime_type}</span>)}
                </div>
              )}
              <div className="mt-1 flex items-center gap-2">
                {event.payload_preview && (
                  <button
                    type="button"
                    className="text-[10px] text-accent hover:underline"
                    aria-expanded={open}
                    onClick={() => setRawOpen((current) => {
                      const next = new Set(current);
                      if (next.has(index)) next.delete(index); else next.add(index);
                      return next;
                    })}
                  >
                    {open ? 'Hide captured preview' : 'Show captured preview'}
                  </button>
                )}
                <button type="button" className="text-[10px] text-text-muted hover:text-text" onClick={() => { void copyText(JSON.stringify(event, null, 2)); }}>Copy event</button>
              </div>
              {open && event.payload_preview && (
                <div className="mt-1 rounded-sm border border-line bg-panel-raised p-2">
                  <div className="mb-1 flex items-center justify-between text-[10px] text-text-muted">
                    <span>Captured payload preview · bounded by the monitor capture limit</span>
                    <button type="button" className="text-accent hover:underline" onClick={() => { void copyText(event.payload_preview ?? ''); }}>Copy raw</button>
                  </div>
                  <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] text-text">{event.payload_preview}</pre>
                </div>
              )}
            </li>
          );
        })}
      </ol>
      {visible.length === 0 && <div className="px-3 py-6 text-center text-xs text-text-muted">No timeline events match this search and filter.</div>}
    </section>
  );
}
