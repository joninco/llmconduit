/**
 * Timeline tab — renders the flow's monitor `event_append` stream (D10): one row per
 * `DebugTimelineEvent` (timestamp, kind, summary, optional payload preview + image chips).
 * Chronological (ring order). Empty state when the flow has no events yet.
 */
import type { DebugTimelineEvent } from '../../api/types';
import { fmtClock } from '../FlowTable/format';

export function Timeline({ events }: { events: DebugTimelineEvent[] }) {
  if (events.length === 0) {
    return (
      <div className="px-3 py-4 text-xs italic text-text-muted" data-testid="timeline-empty">
        No timeline events yet.
      </div>
    );
  }
  return (
    <ol className="flex flex-col gap-2 px-3 py-2" data-testid="timeline">
      {events.map((ev, i) => (
        <li key={`${ev.timestamp_ms}-${i}`} className="border-l-2 border-line pl-3" data-testid="timeline-event">
          <div className="flex items-baseline gap-2">
            <span className="tabular-nums text-[10px] text-text-muted">{fmtClock(ev.timestamp_ms)}</span>
            <span className="font-mono text-xs text-accent">{ev.kind}</span>
          </div>
          <div className="text-xs text-text">{ev.summary}</div>
          {ev.payload_preview && (
            <pre className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-words rounded-sm bg-panel-raised px-2 py-1 font-mono text-[11px] text-text-muted">
              {ev.payload_preview}
            </pre>
          )}
          {ev.images.length > 0 && (
            <div className="mt-1 flex flex-wrap gap-1">
              {ev.images.map((img) => (
                <span key={img.id} className="rounded-sm bg-meta/15 px-1.5 py-0.5 text-[10px] text-meta" title={img.path}>
                  {img.label} ({img.mime_type})
                </span>
              ))}
            </div>
          )}
        </li>
      ))}
    </ol>
  );
}
