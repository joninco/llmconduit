/**
 * TokensCell (gap 08) — the flow-table tokens cell with a token-economics breakdown popover.
 *
 * The dense cell reads `in / out` (prompt/completion) like before; on hover/focus it reveals a
 * popover with the cached/reasoning SPLIT, the cache-hit rate, and the "$ saved by cache" figure —
 * the spec-08 surface that makes the cost/token story legible. Every line carries its own
 * data-quality tier (`data-quality`): a `measured` count, a `derived` rate/saving, or `—`
 * (unavailable) — an UNREPORTED token class renders `—`, never a fabricated `0`; a reported `0`
 * stays a distinct measured zero.
 *
 * Positioning: the popover is rendered as a FIXED overlay at the cell's screen rect (the same
 * convention as `CooldownTooltip`), so the `overflow-auto` virtualized scroll container never clips
 * it and it adds no layout to the row (no FLIP, D10 constraint). It is toggled by hover. The row is
 * itself a `<button>`, so the cell does NOT add a nested focusable element (invalid HTML); the SAME
 * split + cache-hit + "$ saved" surface is mirrored in the keyboard-reachable FlowDetail inspector
 * line, so the breakdown is not mouse-only overall.
 */
import { useId, useRef, useState } from 'react';
import type { FlowSummary, ModelPrice } from '../../api/types';
import { fmtTokens } from './format';
import { tokenEconomics, type EconValue, type TokenEconomics } from './tokenEconomics';
import { cn } from '../../lib/cn';

export function TokensCell({
  flow,
  priceTable,
}: {
  flow: FlowSummary;
  priceTable: Record<string, ModelPrice>;
}) {
  const econ = tokenEconomics(flow, priceTable);
  const tokensIn = flow.usage?.prompt;
  const tokensOut = flow.usage?.completion;
  // A popover is worth offering only when there is usage to break down.
  const hasUsage = !!flow.usage;

  const anchorRef = useRef<HTMLSpanElement>(null);
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null);
  const popoverId = useId();

  const reveal = () => {
    const rect = anchorRef.current?.getBoundingClientRect();
    if (rect) setPos({ x: rect.right, y: rect.bottom });
    setOpen(true);
  };
  const hide = () => setOpen(false);

  if (!hasUsage) {
    // No usage → no breakdown; render the plain dual-count (both `—`) without an interactive layer.
    return (
      <span className="flex items-center justify-end text-right tabular-nums text-text-muted" data-testid="tokens-cell">
        {fmtTokens(tokensIn)}
        <span className="text-line"> / </span>
        {fmtTokens(tokensOut)}
      </span>
    );
  }

  return (
    <span
      ref={anchorRef}
      className="relative flex items-center justify-end text-right tabular-nums text-text-muted"
      data-testid="tokens-cell"
      aria-describedby={open ? popoverId : undefined}
      onMouseEnter={reveal}
      onMouseLeave={hide}
    >
      <span className="underline decoration-line decoration-dotted underline-offset-2" data-testid="tokens-trigger">
        {fmtTokens(tokensIn)}
        <span className="text-line no-underline"> / </span>
        {fmtTokens(tokensOut)}
      </span>
      {open && pos && <TokenBreakdownPopover id={popoverId} econ={econ} x={pos.x} y={pos.y} />}
    </span>
  );
}

/** One labelled line in the breakdown popover, tagged with its data-quality tier. */
function EconLine({
  label,
  econ,
  title,
}: {
  label: string;
  econ: EconValue;
  title?: string;
}) {
  return (
    <div className="contents" data-testid={`econ-line-${label}`} data-quality={econ.quality}>
      <dt className="text-text-muted" title={title}>{label}</dt>
      <dd className={cn('text-right tabular-nums', econ.quality === 'unavailable' ? 'text-text-muted' : 'text-text')}>
        {econ.value}
      </dd>
    </div>
  );
}

/**
 * The breakdown card, a FIXED overlay anchored at the cell's bottom-right screen coordinate. The
 * reasoning/cached `—` lines read as honest gaps; the "$ saved" line ALWAYS renders (so the absence
 * of a saving is itself legible) but shows `—` when no configured cached price licenses a figure
 * (presence gate) or the class was unreported — never a fabricated saving.
 */
function TokenBreakdownPopover({
  id,
  econ,
  x,
  y,
}: {
  id: string;
  econ: TokenEconomics;
  x: number;
  y: number;
}) {
  return (
    <div
      id={id}
      role="tooltip"
      data-testid="tokens-popover"
      className="pointer-events-none fixed z-50 w-52 -translate-x-full translate-y-1 rounded-md border border-line bg-panel-raised p-2.5 text-left text-xs shadow-lg"
      style={{ left: x, top: y }}
    >
      <div className="mb-1.5 flex items-center justify-between border-b border-line pb-1">
        <span className="text-[10px] uppercase tracking-[0.14em] text-text-muted">token economics</span>
        {/* The split is MEASURED (upstream-reported); the rate + saving are DERIVED. The badge
            makes the strongest provenance present visible at a glance. */}
        <span className="rounded-sm bg-accent/15 px-1 text-[9px] uppercase tracking-wide text-accent">
          measured · derived
        </span>
      </div>
      <dl className="grid grid-cols-[1fr_auto] gap-x-3 gap-y-0.5">
        <EconLine label="prompt" econ={econ.prompt} title="prompt (input) tokens" />
        <EconLine label="completion" econ={econ.completion} title="completion (output) tokens" />
        <EconLine label="cached" econ={econ.cached} title="cache-read prompt tokens (— = upstream did not report; 0 = a real miss)" />
        <EconLine label="reasoning" econ={econ.reasoning} title="reasoning tokens (— = upstream did not report)" />
        <div className="col-span-2 my-0.5 border-t border-line/60" />
        <EconLine label="cache hit" econ={econ.cacheHit} title="cached / prompt (derived; — = cached unreported)" />
        <EconLine
          label="$ saved"
          econ={econ.saved}
          title={
            econ.cachedPriceConfigured
              ? '$ saved by serving cached tokens at the cached rate (derived)'
              : '$ saved unavailable — no configured cached price for this model'
          }
        />
      </dl>
    </div>
  );
}
