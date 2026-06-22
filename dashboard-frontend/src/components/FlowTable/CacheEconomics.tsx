/**
 * CacheEconomics (gap 08) — the AGGREGATE cache-hit-rate strip for the flows screen.
 *
 * The spec-08 operator question is "is prefix caching saving money?" — answered per-flow by the
 * tokens-cell popover, and in AGGREGATE here: cache-hit rate (and "$ saved") rolled up BY MODEL
 * across the current (filtered) flow set. Model is the honestly-available grouping dimension on the
 * frozen `FlowSummary` contract today; per-CLIENT attribution arrives on the frontend with gap 15
 * (`client_label`), at which point this strip can add that dimension without changing the math.
 *
 * Data-quality (consumes the gap-07 contract, never fabricates):
 *  - the hit rate is `derived` over flows that REPORTED `cached`; a model whose flows never reported
 *    cached renders `—` (unavailable), NOT a fabricated 0% (see `aggregateCacheByKey`).
 *  - "$ saved" sums only flows with a CONFIGURED cached price (presence) → `—` when none.
 *  - a model group is badged `est` when ANY grouped flow's `cost_confidence !== 'confident'` (the
 *    cross-cutting "label estimates" rule), independent of whether a hit rate is shown — an estimated
 *    row with an unavailable rate but a derived `$ saved` still carries the badge; a fully-confident
 *    group carries no badge.
 *
 * Collapsed by default (a dense secondary surface under the table); the header summarises the
 * overall reported-sample coverage so an operator sees at a glance whether caching is measurable.
 */
import { useMemo, useState } from 'react';
import type { FlowSummary, ModelPrice } from '../../api/types';
import { aggregateCacheByKey, type CacheAggregateRow } from './tokenEconomics';
import { cn } from '../../lib/cn';

export function CacheEconomics({
  rows,
  priceTable,
}: {
  rows: FlowSummary[];
  priceTable: Record<string, ModelPrice>;
}) {
  const [open, setOpen] = useState(false);
  // Group by the SERVED model (the model actually billed); fall back to requested when the served
  // identity is not yet known. Flows with no model at all are dropped by `aggregateCacheByKey`.
  const aggregates = useMemo(
    () => aggregateCacheByKey(rows, (f) => f.model_served ?? f.model_requested, priceTable),
    [rows, priceTable],
  );

  // Overall coverage: how many model groups have ANY measured cache-hit rate (reported cached).
  const measuredGroups = aggregates.filter((a) => a.hitRate.quality !== 'unavailable').length;

  return (
    <section
      className="shrink-0 border-t border-line bg-panel"
      data-testid="cache-economics-panel"
      aria-label="aggregate cache economics by model"
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] uppercase tracking-[0.14em] text-text-muted transition-colors hover:text-text"
        data-testid="cache-economics-toggle"
      >
        <span className={cn('inline-block transition-transform', open ? 'rotate-90' : '')} aria-hidden>
          ▸
        </span>
        <span>cache economics</span>
        <span className="ml-auto font-mono tabular-nums text-text-muted" data-testid="cache-economics-summary">
          {aggregates.length === 0
            ? 'no models'
            : `${measuredGroups}/${aggregates.length} models with measured hit rate`}
        </span>
      </button>
      {open && (
        <div className="max-h-44 overflow-auto px-3 pb-2">
          {aggregates.length === 0 ? (
            <div className="py-2 text-center text-xs text-text-muted" data-testid="cache-economics-empty">
              No model usage in the current flow set.
            </div>
          ) : (
            <table className="w-full text-xs" data-testid="cache-economics-table">
              <thead>
                <tr className="text-[10px] uppercase tracking-[0.12em] text-text-muted">
                  <th className="py-1 text-left font-normal">model</th>
                  <th className="py-1 text-right font-normal">cache hit</th>
                  <th className="py-1 text-right font-normal">$ saved</th>
                  <th className="py-1 text-right font-normal">reported</th>
                </tr>
              </thead>
              <tbody>
                {aggregates.map((agg) => (
                  <AggregateRow key={agg.key} agg={agg} />
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}
    </section>
  );
}

function AggregateRow({ agg }: { agg: CacheAggregateRow }) {
  return (
    <tr className="border-t border-line/40" data-testid="cache-economics-row" data-model={agg.key}>
      <td className="truncate py-1 pr-2 font-mono text-text" title={agg.key}>
        {agg.key}
      </td>
      <td className="py-1 text-right tabular-nums" data-testid="agg-hit-rate" data-quality={agg.hitRate.quality}>
        <span className={agg.hitRate.quality === 'unavailable' ? 'text-text-muted' : 'text-text'}>
          {agg.hitRate.value}
        </span>
        {/* The cross-cutting rule: an aggregate that includes a non-confident member is an
            ESTIMATE — labelled, so a confident roll-up is never confused with a best-effort one.
            Rendered whenever the group is estimated, INDEPENDENT of `hitRate.quality`: a derived
            `$ saved` (or a zero-denominator / unavailable-rate row) must still carry the `est` label
            so an estimate is never shown unlabelled. */}
        {agg.estimated && (
          <span
            className="ml-1.5 rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase tracking-wide text-status-cooling"
            data-testid="agg-est"
            title="estimate — at least one flow in this group has an estimated cost confidence"
          >
            est
          </span>
        )}
      </td>
      <td className="py-1 text-right tabular-nums" data-testid="agg-saved" data-quality={agg.saved.quality}>
        <span className={agg.saved.quality === 'unavailable' ? 'text-text-muted' : 'text-meta'}>
          {agg.saved.value}
        </span>
      </td>
      <td className="py-1 text-right tabular-nums text-text-muted" data-testid="agg-reported">
        {agg.reportedSamples}/{agg.totalSamples}
      </td>
    </tr>
  );
}
