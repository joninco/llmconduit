/**
 * ContextPressure (gap 09) — the AGGREGATE context-pressure stat for the flows screen.
 *
 * The spec-09 operator question is "are we near max context — risking slow prefill, truncation, or
 * 400s?" — answered per-flow by the inspector gauge, and in AGGREGATE here: the PEAK context-window
 * utilization across the current (filtered) flow set, plus how many flows are near/over the window.
 * A compact always-visible stat (the "aggregate context-pressure stat exists" acceptance criterion),
 * sitting beside the gap-08 cache-economics strip under the table.
 *
 * Data quality (consumes the gap-06 nullable catalog + gap-07 usage, never fabricates):
 *  - the peak is `derived` over flows whose utilization is measurable (known limit + reported
 *    usage); a flow set with NONE measurable renders `—` (unavailable), NOT a fabricated `0%` peak
 *    (see `aggregateContextPressure`).
 *  - the near/over counts only ever count MEASURED flows — an unknown-limit or unreported-usage flow
 *    is excluded entirely, never silently treated as 0% or 100%.
 *  - the `measured / total` coverage is shown so an operator sees at a glance how much of the set is
 *    actually measurable.
 */
import { useMemo } from 'react';
import type { FlowSummary } from '../../api/types';
import { aggregateContextPressure, type ContextLimitMap, type UtilRisk } from './contextUtilization';
import { cn } from '../../lib/cn';

/** Risk band → the peak-percent accent (mirrors the gauge fill/text tokens). */
const RISK_TEXT: Record<Exclude<UtilRisk, 'none'>, string> = {
  ok: 'text-status-healthy',
  near: 'text-status-cooling',
  over: 'text-status-down',
};

export function ContextPressure({
  rows,
  limits,
}: {
  rows: FlowSummary[];
  limits: ContextLimitMap;
}) {
  const agg = useMemo(() => aggregateContextPressure(rows, limits), [rows, limits]);
  const measurable = agg.measuredFlows > 0;
  const risk = agg.peakRisk === 'none' ? 'ok' : agg.peakRisk;

  return (
    <section
      className="flex shrink-0 items-center gap-2 border-t border-line bg-panel px-3 py-1.5 text-[11px]"
      data-testid="context-pressure-panel"
      aria-label="aggregate context-window pressure"
    >
      <span className="uppercase tracking-[0.14em] text-text-muted">context pressure</span>

      {/* Peak utilization across the visible flows — the headline pressure signal. */}
      <span className="ml-1 flex items-baseline gap-1" title="peak context-window utilization across the current flow set (derived; — when nothing is measurable)">
        <span className="text-text-muted">peak</span>
        <span
          className={cn('font-mono tabular-nums', measurable ? RISK_TEXT[risk] : 'text-text-muted')}
          data-testid="context-pressure-peak"
          data-quality={measurable ? 'derived' : 'unavailable'}
          data-risk={agg.peakRisk}
        >
          {agg.peakLabel}
        </span>
      </span>

      {/* Near/over-limit counts — only ever count MEASURED flows. */}
      <span className="flex items-baseline gap-1" title="flows at/over the near-limit threshold (≥85% / ≥100% of the model window)">
        <span className="text-text-muted">near/over</span>
        <span className="font-mono tabular-nums text-status-cooling" data-testid="context-pressure-near">{agg.nearCount}</span>
        <span className="text-line">/</span>
        <span className="font-mono tabular-nums text-status-down" data-testid="context-pressure-over">{agg.overCount}</span>
      </span>

      {/* Coverage: how much of the set is measurable (the don't-lie-with-zeros honesty). */}
      <span className="ml-auto font-mono tabular-nums text-text-muted" data-testid="context-pressure-coverage">
        {agg.measuredFlows}/{agg.totalFlows} measured
      </span>
    </section>
  );
}
