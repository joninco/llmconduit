/**
 * TheaterView (D12) — the "wow": a fullscreen-capable dark grid of live "rivers", one per active
 * stream. Each river streams its output/reasoning/tool deltas (folded from the monitor ring's
 * `segment_append` messages), with a per-river tokens/sec meter + a blinking cursor. The grid
 * auto-sizes: 1 river → big, 2 → split, 3-6 → a 3-wide multi-grid. A fullscreen toggle expands the
 * theater over the whole viewport.
 *
 * SEEK (D11) — the approved body-free-snapshot tradeoff, surfaced honestly: a historical snapshot
 * carries NO delta stream (the bodies are evicted — see D5/D10), so the theater CANNOT replay a
 * live river for a past moment. While seeking we therefore show an explicit "historical — deltas
 * not replayed" banner and render only the frozen snapshot's TERMINAL SUMMARY per flow (model,
 * status, token totals), NOT a fake live river. Leaving seek returns to the live rivers.
 */
import { useMemo, useState } from 'react';
import { River } from '../components/viz/River';
import { gridColumns } from '../components/viz/riverModel';
import { useLingeringRivers } from '../components/viz/useLingeringRivers';
import { useDashboard } from '../store/hooks';
import type { FlowSummary } from '../api/types';
import { cn } from '../lib/cn';

export function TheaterView() {
  const seeking = useDashboard((s) => s.connection === 'seeking');
  return seeking ? <HistoricalTheater /> : <LiveTheater />;
}

/** Live rivers from the monitor ring (one per `response_id`), auto-gridded, fullscreen-toggleable. */
function LiveTheater() {
  const monitor = useDashboard((s) => s.monitor);
  const [fullscreen, setFullscreen] = useState(false);
  // Terminated tiles linger-then-fade-then-remove (finding 4) rather than persisting until the
  // monitor evicts them; the hook owns the StrictMode-safe timers.
  const rivers = useLingeringRivers(monitor);
  const cols = gridColumns(rivers.length);

  return (
    <div
      className={cn(
        'flex min-h-0 min-w-0 flex-1 flex-col bg-bg p-4',
        fullscreen && 'fixed inset-0 z-40',
      )}
      data-testid="theater-view"
      data-fullscreen={fullscreen || undefined}
    >
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Theater</h2>
        <p className="text-sm text-text-muted">live streams · {rivers.length} active</p>
        <button
          type="button"
          onClick={() => setFullscreen((v) => !v)}
          aria-pressed={fullscreen}
          className="ml-auto rounded-md border border-line px-2.5 py-1 text-xs text-text-muted transition-colors hover:text-text"
          data-testid="theater-fullscreen-toggle"
        >
          {fullscreen ? 'exit fullscreen' : 'fullscreen'}
        </button>
      </header>
      {rivers.length === 0 ? (
        <div className="flex flex-1 items-center justify-center text-sm text-text-muted" data-testid="theater-empty">
          No active streams. Live output appears here as requests stream.
        </div>
      ) : (
        <div
          className="grid min-h-0 flex-1 gap-3"
          style={{ gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))` }}
          data-testid="theater-grid"
          data-cols={cols}
        >
          {rivers.map((river) => (
            <River key={river.id} river={river} exiting={river.exiting} />
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * The frozen-seek theater: NO live river (body-free snapshots have no deltas). Renders the
 * snapshot's terminal summary per flow + the explicit "deltas not replayed" affordance.
 */
function HistoricalTheater() {
  const flows = useDashboard((s) => s.flows);
  const summaries = useMemo(() => [...flows.values()], [flows]);

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg p-4" data-testid="theater-view-historical">
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Theater</h2>
        <span
          className="rounded-sm border border-status-cooling/40 bg-status-cooling/10 px-2 py-0.5 text-[11px] text-status-cooling"
          data-testid="theater-historical-banner"
        >
          historical — deltas not replayed
        </span>
      </header>
      {summaries.length === 0 ? (
        <div className="flex flex-1 items-center justify-center text-sm text-text-muted" data-testid="theater-historical-empty">
          No flows in this snapshot.
        </div>
      ) : (
        <div className="grid min-h-0 flex-1 auto-rows-min grid-cols-1 gap-2 overflow-auto sm:grid-cols-2 lg:grid-cols-3" data-testid="theater-historical-grid">
          {summaries.map((flow) => (
            <TerminalSummaryCard key={flow.api_call_id} flow={flow} />
          ))}
        </div>
      )}
    </div>
  );
}

/** A single frozen-flow card: model, status, token totals — the snapshot's terminal summary. */
function TerminalSummaryCard({ flow }: { flow: FlowSummary }) {
  const tokens = flow.usage?.total ?? 0;
  return (
    <div className="rounded-md border border-line bg-panel p-3" data-testid="theater-summary-card" data-flow-id={flow.api_call_id}>
      <div className="flex items-center gap-2">
        <span className="truncate font-mono text-xs text-text" title={flow.api_call_id}>
          {flow.model_served ?? flow.model_requested ?? flow.api_call_id}
        </span>
        <span className="ml-auto text-[11px] uppercase tracking-wide text-text-muted">{flow.status}</span>
      </div>
      <p className="mt-1 tabular-nums text-[11px] text-text-muted">
        {tokens} tokens{flow.terminal_reason ? ` · ${flow.terminal_reason}` : ''}
      </p>
    </div>
  );
}
