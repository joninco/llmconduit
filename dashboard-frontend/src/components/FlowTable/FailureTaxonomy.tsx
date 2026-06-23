/**
 * FailureTaxonomy (gap 14) — the AGGREGATE failure deep-dive panel that answers, at a glance,
 * "what is failing and why, in aggregate — not one red row at a time?" It groups the observed flows
 * by (provider/model) × failure reason, with a DERIVED error rate per group, headed by an overall
 * error-rate CHIP.
 *
 * Pure render over `failureTaxonomy(rows)` (the DOM-free model) — every figure carries its
 * `data-quality` provenance, and the don't-lie-with-zeros rules live in the model (a zero-sample
 * window renders `—`, never `0%`; an observed-but-no-failure window reads a measured-base `derived
 * 0%`). Design language matches the inspector's other instruments (`AttemptTrace`, `LatencyBreakdown`,
 * the stats strip): `tabular-nums`, the Night Watch status tokens, an instrument feel.
 *
 * SOURCE: the same observed flow population the FlowTable shows — `useFlowRows(filters)` (live WS
 * store ∪ the `/flows` query, scoped by the active filter bar). So filtering to a provider re-scopes
 * the taxonomy to that provider's failures too.
 */
import { useMemo } from 'react';
import { useDashboard, useFlowFilter } from '../../store/hooks';
import { useFlowRows } from './useFlowRows';
import { failureTaxonomy, UNAVAILABLE, type FailureGroup, type FailureTaxonomy as FailureTaxonomyModel } from './failureTaxonomy';
import { Panel } from '../ui/Panel';
import { cn } from '../../lib/cn';

/** Error-rate threshold (%) above which the chip + a group turns red (mirrors the stats strip's err%). */
export const FAILURE_RATE_THRESHOLD = 5;

/**
 * The aggregate failure panel. Self-contained: reads the filtered flow rows + builds the model. ALWAYS
 * renders (don't-lie-with-zeros — review MEDIUM): a ZERO-sample window shows an EXPLICIT `unavailable`
 * `—` state (the chip reads `—`, tagged `data-quality="unavailable"`), DISTINCT from an observed
 * all-success window which shows a MEASURED-base derived `0%`. A blank/hidden panel would conflate the
 * two — a zero-sample window must read "unmeasured (—)", not "0% / no failures".
 */
export function FailureTaxonomy() {
  const filters = useFlowFilter((s) => s.filters);
  const { rows } = useFlowRows(filters);
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const model = useMemo(() => failureTaxonomy(rows), [rows]);
  const observed = model.available;

  return (
    <Panel className="m-2 flex flex-col gap-2 p-3" data-testid="failure-taxonomy" data-available={observed ? 'true' : 'false'}>
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-baseline gap-2">
          <span className="text-xs font-semibold uppercase tracking-[0.14em] text-text-muted">failure taxonomy</span>
          {/* Grouping is `measured` only when there is an observed population to count; a zero-sample
              window has nothing to group ⇒ `unavailable` (don't fabricate a measured grouping). */}
          <span
            className="text-[10px] text-text-muted"
            data-testid="failure-grouping-quality"
            data-quality={observed ? 'measured' : 'unavailable'}
            title={observed ? 'grouping is measured (counted directly off the observed flows)' : 'no flows observed — nothing to group (unavailable, not 0)'}
          >
            {observed ? 'grouping measured' : 'grouping unavailable'}
          </span>
          {seeking && <span className="text-[10px] text-status-cooling" title="as-of the seeked moment (frozen cut)">· frozen cut</span>}
        </div>
        <ErrorRateChip model={model} />
      </div>

      {!observed ? (
        // ZERO observed flows ⇒ EXPLICIT unavailable `—` (NOT a blank, NOT `0%`/"no failures"). The
        // chip above already reads `—` (data-quality unavailable); this is its honest companion line.
        <div className="px-1 py-2 text-xs italic text-text-muted" data-testid="failure-unavailable" data-quality="unavailable">
          No flows observed in this window — error rate unavailable ({UNAVAILABLE}), not 0%.
        </div>
      ) : model.groups.length === 0 ? (
        // Observed flows, but ZERO failures: an explicit honest "no failures in this window" — the
        // overall chip reads a MEASURED-base `0%` (NOT —), distinct from the unmeasured window above.
        <div className="px-1 py-2 text-xs italic text-text-muted" data-testid="failure-none">
          No failures in this window. ({model.totalFlows} observed)
        </div>
      ) : (
        <ul className="flex flex-col gap-1.5" data-testid="failure-group-list" role="list">
          {model.groups.map((g) => (
            <FailureGroupRow key={g.key} group={g} />
          ))}
        </ul>
      )}
    </Panel>
  );
}

/**
 * The overall error-rate chip. A zero-sample window reads `—` (`unavailable`) — NOT `0%` (the model
 * already collapses to that); an observed window reads a `derived` rate, turning red above the
 * threshold. Carries a `data-quality` tag like every other dashboard metric.
 */
function ErrorRateChip({ model }: { model: FailureTaxonomyModel }) {
  const over = model.overallQuality === 'derived' && model.overallErrorRatePct > FAILURE_RATE_THRESHOLD;
  return (
    <div
      className="flex flex-col items-end gap-0.5"
      data-testid="failure-error-rate"
      data-quality={model.overallQuality}
      title={
        model.overallQuality === 'unavailable'
          ? 'error rate unavailable — no flows observed in this window (— , not 0%)'
          : `error rate (derived): ${model.totalFailed}/${model.totalFlows} flows failed`
      }
    >
      <span className="text-[10px] uppercase tracking-[0.14em] text-text-muted">err rate</span>
      <span
        className={cn('font-mono text-lg font-semibold tabular-nums', over ? 'text-status-down' : 'text-text')}
        data-testid="failure-error-rate-value"
        aria-label={`overall error rate ${model.overallErrorRateText}, ${model.overallQuality}`}
      >
        {model.overallErrorRateText}
      </span>
    </div>
  );
}

/** One model/provider group: provider · model, the derived rate (failed/total), and its reason chips. */
function FailureGroupRow({ group }: { group: FailureGroup }) {
  const over = group.errorRatePct > FAILURE_RATE_THRESHOLD;
  return (
    <li
      className="flex flex-col gap-1 rounded-md border border-line/60 bg-panel-raised px-2 py-1.5"
      data-testid="failure-group"
      data-group-key={group.key}
      role="listitem"
    >
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-baseline gap-1.5">
          <span className="truncate font-mono text-xs text-text" title={`provider: ${group.provider}`} data-testid="failure-group-provider">
            {group.provider}
          </span>
          <span className="text-line">·</span>
          <span className="truncate font-mono text-xs text-text-muted" title={`model: ${group.model}`} data-testid="failure-group-model">
            {group.model}
          </span>
        </div>
        <div className="flex shrink-0 items-baseline gap-1.5">
          {/* The derived error rate for this group (failed/total). A measured-base `0%` is impossible
              here — a group is listed only when it HAS failures — so this is always a real >0% derived. */}
          <span
            className={cn('font-mono text-sm font-semibold tabular-nums', over ? 'text-status-down' : 'text-status-cooling')}
            data-testid="failure-group-rate"
            data-quality="derived"
            title="error rate for this group (derived: failed / observed)"
          >
            {group.errorRateText}
          </span>
          <span className="text-[10px] tabular-nums text-text-muted" data-testid="failure-group-count" title="failed / observed flows in this group (measured)">
            {group.failed}/{group.total}
          </span>
        </div>
      </div>
      {/* The per-reason breakdown — each a distinct terminal_reason / bounded gap-03 error class. */}
      <ul className="flex flex-wrap gap-1" data-testid="failure-reason-list" role="list">
        {group.reasons.map((r) => (
          <li
            key={r.key}
            className="flex items-center gap-1 rounded-sm bg-status-down/10 px-1.5 py-0.5 text-[10px]"
            data-testid="failure-reason"
            data-reason-key={r.key}
            data-source={r.source}
            role="listitem"
            title={r.source === 'error_class' ? `gap-03 error class (taxonomic): ${r.label}` : `terminal reason: ${r.label}`}
          >
            <span className="font-mono text-status-down">{r.label}</span>
            <span className="tabular-nums text-text-muted">×{r.count}</span>
          </li>
        ))}
      </ul>
    </li>
  );
}
