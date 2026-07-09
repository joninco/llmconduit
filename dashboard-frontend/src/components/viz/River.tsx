/**
 * River (D12) — a single live stream tile in the theater. Reasoning renders FIRST (dim, expanded by
 * default — it is what the model streams first), then the bright mono output, then tool cards. A
 * per-river tokens/sec meter sits in the header; a blinking cursor trails the output while the
 * stream is running (gone once it completes).
 *
 * FOLLOW-THE-STREAM: the body is a scroll container pinned to the bottom while new text arrives
 * ("stick to bottom"), so a stream longer than the tile scrolls naturally instead of overflowing —
 * nothing is ever deleted from the top (the full text lives in the store's river fold; only a
 * pathological stream past the memory cap head-trims, and that shows an explicit marker). Scrolling
 * up releases the pin so history is readable mid-stream; scrolling back to the bottom re-engages it.
 *
 * Pure React + CSS — no d3, no framer-motion (the fade/linger is a CSS keyframe in index.css, and
 * `prefers-reduced-motion` cuts the cursor blink + entrance via that CSS, so motion is honored
 * without a JS animation library). The component is presentational; the TheaterView owns the data
 * (the store's incremental river fold) and the grid.
 */
import { useEffect, useRef, useState } from 'react';
import type { River as RiverData } from './riverModel';
import { cn } from '../../lib/cn';

const STATUS_DOT: Record<RiverData['status'], string> = {
  running: 'bg-status-healthy',
  completed: 'bg-text-muted',
  failed: 'bg-status-down',
};

/** How close (px) to the bottom counts as "at the bottom" for re-engaging the follow pin. */
const STICK_THRESHOLD_PX = 48;

export function River({ river, exiting = false }: { river: RiverData; exiting?: boolean }) {
  // Reasoning is EXPANDED by default — it streams before the output, so hiding it made the tile
  // look empty during the thinking phase. The toggle collapses it for output-only reading.
  const [showReasoning, setShowReasoning] = useState(true);
  const running = river.status === 'running';

  const bodyRef = useRef<HTMLDivElement | null>(null);
  // Follow pin: true while the view should track the stream's tail. A ref (not state) — toggling
  // it must not re-render, and the scroll handler + append effect both read the latest value.
  const stickRef = useRef(true);

  // Re-pin to the bottom whenever streamed content grows (any channel) while the pin is engaged.
  // Keyed on total streamed chars, not array/string identity, so one effect covers all channels.
  const contentLen =
    river.reasoning.length +
    river.output.length +
    river.tools.reduce((sum, t) => sum + t.length, 0);
  useEffect(() => {
    const el = bodyRef.current;
    if (el && stickRef.current) el.scrollTop = el.scrollHeight;
  }, [contentLen, showReasoning]);

  // User scroll position decides the pin: near-bottom re-engages, scrolled-up releases. The
  // programmatic re-pin above also fires this handler, landing at the bottom → stays engaged.
  const onBodyScroll = (): void => {
    const el = bodyRef.current;
    if (el) stickRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < STICK_THRESHOLD_PX;
  };

  return (
    <div
      data-testid="river"
      data-river-id={river.id}
      data-status={river.status}
      data-exiting={exiting || undefined}
      // `river-tile` carries the CSS entrance; `river-tile-exiting` swaps it for the linger-then-fade
      // exit while a terminated tile is being removed (finding 4; reduced-motion → ~instant).
      className={cn(
        'river-tile flex min-h-0 min-w-0 flex-col overflow-hidden rounded-md border border-line bg-panel',
        exiting && 'river-tile-exiting',
      )}
    >
      <div className="flex items-center gap-2 border-b border-line px-3 py-1.5">
        <span className={cn('h-2 w-2 shrink-0 rounded-full', STATUS_DOT[river.status])} aria-hidden />
        <span className="truncate font-mono text-xs text-text" title={river.id}>
          {river.model ?? river.id}
        </span>
        <span className="ml-auto shrink-0 tabular-nums text-[11px] text-accent" data-testid="river-tps">
          {river.tokensPerSec.toFixed(1)} tok/s
        </span>
      </div>

      <div
        ref={bodyRef}
        onScroll={onBodyScroll}
        className="min-h-0 flex-1 overflow-auto px-3 py-2 font-mono text-xs leading-relaxed"
        data-testid="river-body"
      >
        {/* Honest memory-cap marker: text only ever disappears from the top past the riverModel
            caps, and then it says so — never a silent ring truncation. */}
        {river.truncated && (
          <p className="mb-1 text-[10px] uppercase tracking-wide text-status-cooling" data-testid="river-truncated">
            … earlier text trimmed (memory cap) …
          </p>
        )}

        {/* Reasoning — dim, FIRST (it streams before output), collapsible but open by default. */}
        {river.reasoning && (
          <div className="mb-2" data-testid="river-reasoning-section">
            <button
              type="button"
              onClick={() => setShowReasoning((v) => !v)}
              aria-expanded={showReasoning}
              className="text-[10px] uppercase tracking-wide text-text-muted hover:text-text"
              data-testid="river-reasoning-toggle"
            >
              {showReasoning ? '▾ reasoning' : '▸ reasoning'}
            </button>
            {showReasoning && (
              <p className="mt-1 whitespace-pre-wrap break-words text-text-muted/80" data-testid="river-reasoning">
                {river.reasoning}
              </p>
            )}
          </div>
        )}

        {/* Output — bright mono, the primary channel. Cursor only while running. */}
        <p className="whitespace-pre-wrap break-words text-text" data-testid="river-output">
          {river.output}
          {running && (
            <span className="river-cursor ml-px inline-block" data-testid="river-cursor" aria-hidden>
              ▋
            </span>
          )}
        </p>

        {/* Tool calls — compact cards. */}
        {river.tools.length > 0 && (
          <div className="mt-2 flex flex-col gap-1" data-testid="river-tools">
            {river.tools.map((tool, i) => (
              <div key={i} className="rounded-sm border border-line bg-panel-raised px-2 py-1 text-[11px] text-meta">
                {tool}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
