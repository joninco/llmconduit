/**
 * River (D12) — a single live stream tile in the theater. Output is bright mono, reasoning is dim
 * and collapsible, tool calls render as compact cards. A per-river tokens/sec meter sits in the
 * header; a blinking cursor trails the output while the stream is running (gone once it completes).
 *
 * Pure React + CSS — no d3, no framer-motion (the fade/linger is a CSS keyframe in index.css, and
 * `prefers-reduced-motion` cuts the cursor blink + entrance via that CSS, so motion is honored
 * without a JS animation library). The component is presentational; the TheaterView owns the data
 * (folding the monitor ring into rivers) and the grid.
 */
import { useState } from 'react';
import type { River as RiverData } from './riverModel';
import { cn } from '../../lib/cn';

const STATUS_DOT: Record<RiverData['status'], string> = {
  running: 'bg-status-healthy',
  completed: 'bg-text-muted',
  failed: 'bg-status-down',
};

export function River({ river }: { river: RiverData }) {
  // Reasoning is collapsed by default (it is the dim secondary channel); the toggle reveals it.
  const [showReasoning, setShowReasoning] = useState(false);
  const running = river.status === 'running';

  return (
    <div
      data-testid="river"
      data-river-id={river.id}
      data-status={river.status}
      // `river-tile` carries the CSS linger-then-fade entrance (reduced-motion → no animation).
      className="river-tile flex min-h-0 min-w-0 flex-col overflow-hidden rounded-md border border-line bg-panel"
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

      <div className="min-h-0 flex-1 overflow-auto px-3 py-2 font-mono text-xs leading-relaxed">
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

        {/* Reasoning — dim + collapsible. */}
        {river.reasoning && (
          <div className="mt-2">
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
      </div>
    </div>
  );
}
