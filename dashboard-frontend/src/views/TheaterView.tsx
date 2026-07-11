/**
 * TheaterView (D12) — the "wow": a fullscreen-capable dark grid of live "rivers", one per active
 * stream. Each river streams its output/reasoning/tool deltas (from the store's incremental
 * `riverFold`, fed per `segment_append` at arrival — ring-eviction-proof), with a per-river
 * tokens/sec meter + a blinking cursor. The grid
 * auto-sizes: 1 river → big, 2 → split, 3-6 → a 3-wide multi-grid. A fullscreen toggle expands the
 * theater over the whole viewport.
 *
 * SEEK (D11) — durable SQLite cuts carry a monitor cursor and persisted transcript. Theater folds
 * those messages into frozen rivers when available and falls back to terminal summaries for legacy
 * or incomplete history. Leaving seek returns to the live rivers.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { River } from '../components/viz/River';
import { gridColumns } from '../components/viz/riverModel';
import { useLingeringRivers } from '../components/viz/useLingeringRivers';
import { useLiveRivers } from '../components/viz/useLiveRivers';
import { useDashboard } from '../store/hooks';
import type { FlowSummary } from '../api/types';

export function TheaterView() {
  const seeking = useDashboard((s) => s.connection === 'seeking');
  return seeking ? <HistoricalTheater /> : <LiveTheater />;
}

/**
 * Live rivers (one per `response_id`), auto-gridded, fullscreen-toggleable. Rivers come from the
 * store's INCREMENTAL fold (`useLiveRivers`), not a rebuild off the capped monitor ring — so a long
 * stream keeps its full text (the ring's eviction used to visibly delete tokens from the top and
 * drop the reasoning channel, which streams first).
 */
function LiveTheater() {
  const [fullscreen, setFullscreen] = useState(false);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const fullscreenToggleRef = useRef<HTMLButtonElement>(null);
  const wasFullscreenRef = useRef(false);
  // Terminated tiles linger-then-fade-then-remove (finding 4) rather than persisting until the
  // monitor evicts them; the hook owns the StrictMode-safe timers.
  const rivers = useLingeringRivers(useLiveRivers());
  const cols = gridColumns(rivers.length);

  useEffect(() => {
    if (!fullscreen) {
      if (wasFullscreenRef.current) {
        requestAnimationFrame(() => fullscreenToggleRef.current?.focus({ preventScroll: true }));
      }
      wasFullscreenRef.current = false;
      return;
    }

    wasFullscreenRef.current = true;
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (typeof dialog.showModal === 'function') {
      if (!dialog.open) dialog.showModal();
    } else {
      // jsdom and a few older embedded engines expose <dialog> without showModal(). Keeping the
      // open attribute makes the fallback usable; supported browsers still get native modality.
      dialog.setAttribute('open', '');
    }
    requestAnimationFrame(() => fullscreenToggleRef.current?.focus({ preventScroll: true }));

    return () => {
      if (typeof dialog.close === 'function' && dialog.open) dialog.close();
      else dialog.removeAttribute('open');
    };
  }, [fullscreen]);

  const content = (
    <>
      <header className="mb-3 flex items-center gap-3">
        <h2 id="theater-title" className="text-base font-semibold text-text">Theater</h2>
        <p className="text-sm text-text-muted">live streams · {rivers.length} active</p>
        <button
          ref={fullscreenToggleRef}
          type="button"
          onClick={() => setFullscreen((value) => !value)}
          aria-pressed={fullscreen}
          className="ml-auto rounded-md border border-line px-2.5 py-1 text-xs text-text-muted transition-colors hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
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
    </>
  );

  if (fullscreen) {
    return (
      <dialog
        ref={dialogRef}
        aria-labelledby="theater-title"
        className="fixed inset-0 z-40 m-0 hidden h-[100dvh] max-h-none w-screen max-w-none flex-col border-0 bg-bg p-4 text-text backdrop:bg-black/70 open:flex"
        data-testid="theater-view"
        data-fullscreen="true"
        onCancel={(event) => {
          event.preventDefault();
          setFullscreen(false);
        }}
      >
        {content}
      </dialog>
    );
  }

  return (
    <div
      className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg p-4"
      data-testid="theater-view"
    >
      {content}
    </div>
  );
}

/**
 * The frozen-seek theater replays persisted monitor messages when present and otherwise renders
 * the snapshot's terminal summaries with an explicit unavailable affordance.
 */
function HistoricalTheater() {
  const flows = useDashboard((s) => s.flows);
  const summaries = useMemo(() => [...flows.values()], [flows]);
  const rivers = useLiveRivers();

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg p-4" data-testid="theater-view-historical">
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Theater</h2>
        <span
          className="rounded-sm border border-status-cooling/40 bg-status-cooling/10 px-2 py-0.5 text-[11px] text-status-cooling"
          data-testid="theater-historical-banner"
        >
          historical — persisted transcript; deltas not replayed when unavailable
        </span>
      </header>
      {rivers.length > 0 ? (
        <div
          className="grid min-h-0 flex-1 gap-3"
          style={{ gridTemplateColumns: `repeat(${gridColumns(rivers.length)}, minmax(0, 1fr))` }}
          data-testid="theater-historical-rivers"
        >
          {rivers.map((river) => <River key={river.id} river={river} exiting={false} />)}
        </div>
      ) : summaries.length === 0 ? (
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
