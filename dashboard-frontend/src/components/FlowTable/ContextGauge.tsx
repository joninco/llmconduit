/**
 * ContextGauge (gap 09) — the per-flow context-window utilization gauge for the FlowDetail
 * inspector header. A compact horizontal bar + `% util` + remaining-tokens readout + a near/over
 * badge, answering "are we near max context?" at a glance.
 *
 * Design language: matches the inspector's other header lines (the gap-07 cached/reasoning + gap-08
 * cache-economics rows) — a labelled `dl` row, `tabular-nums`, the shared status tokens. The fill
 * color tracks the risk band (healthy → cooling → down), mirroring `STATUS_COLOR` so the gauge reads
 * the same as every other status surface.
 *
 * Utilization is `prompt (input) tokens ÷ model context_limit` (spec 09 / FEATURES item 4) — the
 * completion the model emits is NOT counted (it does not occupy the input window).
 *
 * Data quality (consumes `contextUtilization`, never fabricates):
 *  - utilization is `derived` ONLY when both prompt tokens AND a known `context_limit` exist; the
 *    `data-quality` attribute + the rendered string reflect that.
 *  - UNKNOWN capacity (gap-06 `null`) OR unreported prompt usage ⇒ `—` and an EMPTY (un-filled)
 *    track — NOT a fabricated `0%`/`100%` and not a full/empty-as-zero gauge. The "unavailable"
 *    track is visibly distinct (a dashed, muted rail) from a real `0%` fill (a solid rail at 0
 *    width).
 *  - a real `0%` (measured 0 prompt / known limit) renders `0.0%` with an (empty-but-derived) fill,
 *    distinct from unavailable.
 */
import type { ContextUtilization, UtilRisk } from './contextUtilization';
import { fmtTokens } from './format';
import { cn } from '../../lib/cn';

/** Risk band → the fill color token + a human title fragment. */
const RISK_FILL: Record<Exclude<UtilRisk, 'none'>, string> = {
  ok: 'bg-status-healthy',
  near: 'bg-status-cooling',
  over: 'bg-status-down',
};

/** Risk band → the `% util` text color (mirrors the fill so number + bar agree). */
const RISK_TEXT: Record<Exclude<UtilRisk, 'none'>, string> = {
  ok: 'text-status-healthy',
  near: 'text-status-cooling',
  over: 'text-status-down',
};

/**
 * The gauge body. `util` is the pre-computed `contextUtilization` (the SAME object the aggregate
 * reads), so the bar can never disagree with the number.
 */
export function ContextGauge({ util }: { util: ContextUtilization }) {
  const derived = util.quality === 'derived';
  // Bar width: clamp the VISUAL fill to [0,100]% even when over budget (a bar cannot exceed its
  // track), but the NUMBER is the honest (possibly >100%) percent. Unavailable ⇒ no fill.
  const widthPct = derived && util.fraction !== null ? Math.min(100, Math.max(0, util.fraction * 100)) : 0;
  const risk = util.risk === 'none' ? 'ok' : util.risk;

  return (
    <div className="flex flex-col gap-1" data-testid="context-gauge" data-quality={util.quality} data-risk={util.risk}>
      <div className="flex items-baseline gap-1.5">
        <span
          className={cn('tabular-nums', derived ? RISK_TEXT[risk] : 'text-text-muted')}
          data-testid="context-util-pct"
          title={
            derived
              ? 'context-window utilization = prompt (input) tokens ÷ model context limit (derived)'
              : 'context utilization unavailable — unknown model context limit or unreported prompt usage (— not 0%)'
          }
        >
          {util.percentLabel}
        </span>
        {/* Near/over badge — only on a DERIVED utilization that crossed a threshold. Never inferred
            from missing data (an unavailable gauge carries no badge). */}
        {derived && (util.risk === 'near' || util.risk === 'over') && (
          <span
            className={cn(
              'rounded-sm px-1 text-[9px] uppercase tracking-wide',
              util.risk === 'over' ? 'bg-status-down/15 text-status-down' : 'bg-status-cooling/15 text-status-cooling',
            )}
            data-testid="context-risk-badge"
            title={
              util.risk === 'over'
                ? 'at/over the model context window — truncation / 400 risk'
                : 'approaching the model context window — slow prefill / overflow risk'
            }
          >
            {util.risk === 'over' ? 'over' : 'near'}
          </span>
        )}
        <span className="ml-auto text-[10px] tabular-nums text-text-muted" data-testid="context-headroom" title="remaining context-window headroom (tokens)">
          {derived ? `${util.remainingLabel} left` : '—'}
        </span>
      </div>
      {/* The track. A derived utilization fills a solid rail; an UNAVAILABLE one shows a dashed,
          muted, un-filled rail (visibly "no reading", not an empty 0%). */}
      <div
        className={cn(
          'h-1.5 w-full overflow-hidden rounded-full',
          derived ? 'bg-line/60' : 'border border-dashed border-line/70 bg-transparent',
        )}
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={100}
        // Bounded to the declared [0,100] range (an over-budget flow's HONEST % stays in the visible
        // text + the `over` badge; the ARIA value cannot exceed its max). Undefined when unavailable.
        aria-valuenow={derived && util.fraction !== null ? Math.round(widthPct) : undefined}
        aria-label="context-window utilization"
        data-testid="context-gauge-track"
      >
        {derived && (
          <div
            className={cn('h-full rounded-full transition-[width]', RISK_FILL[risk])}
            style={{ width: `${widthPct}%` }}
            data-testid="context-gauge-fill"
          />
        )}
      </div>
      {/* used / capacity caption — a measured detail under the bar; `—` capacity when unknown. */}
      <span className="text-[10px] tabular-nums text-text-muted" data-testid="context-gauge-caption">
        {fmtTokens(util.usedTokens)} / {fmtTokens(util.contextLimit)} ctx
      </span>
    </div>
  );
}
