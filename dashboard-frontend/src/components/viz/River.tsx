/**
 * River (D12) — a single live stream tile in the theater. Reasoning renders FIRST (dim, expanded by
 * default — it is what the model streams first), then the bright mono output, then tool cards. A
 * text-first status + request telemetry sit in the header; a blinking cursor trails the output
 * while the stream is running (gone once it completes). Failed streams surface their terminal
 * error explicitly instead of relying on a red dot or a buried tool segment.
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
import { fmtElapsed, fmtTokens, fmtTokensPerSec } from '../FlowTable/format';

const STATUS_DOT: Record<RiverData['status'], string> = {
  running: 'bg-status-healthy',
  completed: 'bg-text-muted',
  failed: 'bg-status-down',
};

const STATUS_LABEL: Record<RiverData['status'], string> = {
  running: 'streaming',
  completed: 'complete',
  failed: 'failed',
};

const STATUS_BADGE: Record<RiverData['status'], string> = {
  running: 'border-status-healthy/40 bg-status-healthy/10 text-status-healthy',
  completed: 'border-line bg-panel-raised text-text-muted',
  failed: 'border-status-down/50 bg-status-down/10 text-status-down',
};

/** How close (px) to the bottom counts as "at the bottom" for re-engaging the follow pin. */
const STICK_THRESHOLD_PX = 48;

export function River({
  river,
  exiting = false,
  retained = false,
}: {
  river: RiverData;
  exiting?: boolean;
  /** Latest completed response kept visible while Theater is idle. */
  retained?: boolean;
}) {
  // Reasoning is EXPANDED by default — it streams before the output, so hiding it made the tile
  // look empty during the thinking phase. The toggle collapses it for output-only reading.
  const [showReasoning, setShowReasoning] = useState(true);
  const running = river.status === 'running';
  const [nowMs, setNowMs] = useState(() => Date.now());

  // A live duration is more actionable than a color pulse. Terminal rivers freeze on their
  // measured completion timestamp; only running tiles own a once-per-second clock.
  useEffect(() => {
    if (!running || (river.startedAtMs == null && river.firstMs == null)) return;
    setNowMs(Date.now());
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, [river.firstMs, river.startedAtMs, running]);

  const startMs = river.startedAtMs ?? river.firstMs;
  const endMs = running ? nowMs : (river.terminalAtMs ?? river.lastMs);
  const elapsedMs = startMs != null && endMs != null ? Math.max(0, endMs - startMs) : null;
  const hasRateWindow = river.firstMs != null && river.lastMs != null && river.lastMs > river.firstMs;
  const visibleTools = river.status === 'failed' && river.error
    ? river.tools.filter((tool) => tool.trim() !== `failed: ${river.error}`)
    : river.tools;

  const bodyRef = useRef<HTMLDivElement | null>(null);
  // Follow pin: true while the view should track the stream's tail. A ref (not state) — toggling
  // it must not re-render, and the scroll handler + append effect both read the latest value.
  const stickRef = useRef(true);

  // Re-pin to the bottom whenever streamed content grows (any channel) while the pin is engaged.
  // Keyed on total streamed chars, not array/string identity, so one effect covers all channels.
  const contentLen =
    river.reasoning.length +
    river.output.length +
    visibleTools.reduce((sum, t) => sum + t.length, 0);
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
      data-retained={retained || undefined}
      data-stale={retained || undefined}
      // `river-tile` carries the CSS entrance; `river-tile-exiting` swaps it for the linger-then-fade
      // exit while a terminated tile is being removed (finding 4; reduced-motion → ~instant).
      className={cn(
        'river-tile flex min-h-0 min-w-0 flex-col overflow-hidden rounded-md border border-line bg-panel',
        exiting && 'river-tile-exiting',
        retained && 'border-status-cooling/60',
      )}
    >
      <div className="shrink-0 border-b border-line bg-panel-raised/30">
        <div className="flex min-w-0 flex-wrap items-center gap-2 px-3 py-1.5">
          <span
            className={cn(
              'inline-flex shrink-0 items-center gap-1.5 rounded-sm border px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-wide',
              STATUS_BADGE[river.status],
            )}
            data-testid="river-status"
            data-status={river.status}
          >
            <span className={cn('h-1.5 w-1.5 rounded-full', STATUS_DOT[river.status])} aria-hidden />
            {STATUS_LABEL[river.status]}
          </span>
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-text" title={`${river.model ?? river.id} · ${river.id}`}>
            {river.model ?? river.id}
          </span>
          {retained && (
            <span
              className="shrink-0 rounded-sm border border-status-cooling/50 bg-status-cooling/10 px-1.5 py-0.5 text-[9px] font-semibold uppercase tracking-wide text-status-cooling"
              data-testid="river-retained-badge"
            >
              last response
            </span>
          )}
        </div>
        <div
          className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-line/50 px-3 py-1 font-mono text-[10px] text-text-muted"
          data-testid="river-telemetry"
        >
          <span data-testid="river-elapsed" data-quality={elapsedMs == null ? 'unavailable' : 'measured'}>
            elapsed <strong className="font-medium tabular-nums text-text">{fmtElapsed(elapsedMs)}</strong>
          </span>
          <span
            data-testid="river-tokens"
            data-quality="estimated"
            title="Approximate output + reasoning tokens, estimated at four characters per token"
          >
            <strong className="font-medium tabular-nums text-text">≈{fmtTokens(Math.round(river.approxTokens))}</strong> tok
          </span>
          <span
            data-testid="river-tps"
            data-quality={hasRateWindow ? 'estimated' : 'unavailable'}
            title={hasRateWindow ? 'Approximate stream rate from emitted text' : 'Stream rate unavailable until two timestamped segments arrive'}
          >
            rate <strong className="font-medium tabular-nums text-accent">
              {fmtTokensPerSec(hasRateWindow ? river.tokensPerSec : null)}
            </strong>
          </span>
        </div>
      </div>

      <div
        ref={bodyRef}
        onScroll={onBodyScroll}
        className="min-h-0 flex-1 overflow-auto px-3 py-2 font-mono text-xs leading-relaxed"
        data-testid="river-body"
      >
        {river.status === 'failed' && (
          <div
            className="mb-2 rounded-sm border border-status-down/50 bg-status-down/10 px-2.5 py-2 text-status-down"
            role="alert"
            data-testid="river-error"
            data-quality={river.error ? 'measured' : 'unavailable'}
          >
            <p className="text-[10px] font-semibold uppercase tracking-wide">request failed</p>
            <p className="mt-0.5 whitespace-pre-wrap break-words text-[11px] text-text">
              {river.error ?? 'No failure detail was reported.'}
            </p>
          </div>
        )}

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
              <p className="mt-1 border-l border-line pl-2 italic whitespace-pre-wrap break-words text-text-muted" data-testid="river-reasoning">
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
        {visibleTools.length > 0 && (
          <div className="mt-2 flex flex-col gap-1" data-testid="river-tools">
            {visibleTools.map((tool, i) => (
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
