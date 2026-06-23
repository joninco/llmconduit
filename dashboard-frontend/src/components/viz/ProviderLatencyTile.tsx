/**
 * ProviderLatencyTile (gap 13) — the per-provider latency + error-distribution block inside the
 * topology `CooldownTooltip`. Replaces the tooltip's GLOBAL p99 (which could not answer "which
 * upstream is degrading?") with THIS provider's p50/p95/p99 + error rate + per-class failure
 * distribution, sourced from the spec-12 `ProviderLatency` on the REST/snapshot topology node.
 *
 * Consumes the pure `providerLatency` model (never fabricates):
 *  - a provider with NO in-window samples (absent `ProviderLatency`) ⇒ every figure reads `—`
 *    (`unavailable`), NEVER a `0ms`/`0%`. A small "no samples in window" hint replaces the figures.
 *  - a present entry ⇒ `derived` percentiles (each carries a `derived` provenance badge) + a
 *    `measured` error rate (an all-served provider reads a real `0%`, distinct from `—`).
 *  - the `__other__` overflow bucket / `unknown` sentinel are LABELLED honestly (a hint line),
 *    never hidden.
 *
 * Design language matches the inspector surfaces (gap-10 `LatencyBreakdown`): `tabular-nums`, the
 * shared status tokens, a `data-quality` attribute on every figure, a small uppercase provenance
 * badge for derived/measured-zero.
 */
import type { ProviderLatencyModel, ProviderFigure, Quality } from './providerLatency';
import { cn } from '../../lib/cn';

/** Quality → the small provenance badge (mirrors the gap-10 latency badges). */
const QUALITY_BADGE: Partial<Record<Quality, { cls: string; text: string; title: string }>> = {
  derived: {
    cls: 'bg-accent/15 text-accent',
    text: 'derived',
    title: 'derived — a percentile computed from this provider\'s own attempt-latency histogram (incl. failed primaries)',
  },
  measured: {
    cls: 'bg-status-healthy/15 text-status-healthy',
    text: 'measured',
    title: 'measured — a directly-counted failed/total attempt ratio for this provider',
  },
};

/** A small provenance badge for a derived/measured figure (none for unavailable). */
function QualityBadge({ quality }: { quality: Quality }) {
  const b = QUALITY_BADGE[quality];
  if (!b) return null;
  return (
    <span
      className={cn('ml-1 rounded-sm px-1 py-px text-[8px] uppercase tracking-wide', b.cls)}
      data-testid="provider-latency-quality-badge"
      data-quality={quality}
      title={b.title}
    >
      {b.text}
    </span>
  );
}

/** One labelled per-provider figure cell (`p50` / `err` …) — value + provenance, `—` when absent. */
function FigureRow({
  testId,
  label,
  figure,
  badge = false,
}: {
  testId: string;
  label: string;
  figure: ProviderFigure;
  badge?: boolean;
}) {
  const unavailable = figure.quality === 'unavailable';
  return (
    <>
      <dt className="text-text-muted">{label}</dt>
      <dd
        className={cn('flex items-center justify-end text-right tabular-nums', unavailable ? 'text-text-muted' : 'text-text')}
        data-testid={testId}
        data-quality={figure.quality}
      >
        <span>{figure.text}</span>
        {badge && !unavailable && <QualityBadge quality={figure.quality} />}
      </dd>
    </>
  );
}

export function ProviderLatencyTile({ model }: { model: ProviderLatencyModel }) {
  return (
    <div
      className="mt-1.5 border-t border-line pt-1.5"
      data-testid="provider-latency-tile"
      data-available={model.available ? 'true' : 'false'}
    >
      <div className="mb-0.5 flex items-center gap-1">
        <span className="text-[10px] uppercase tracking-wide text-text-muted">per-provider</span>
        {(model.isOverflow || model.isUnknown) && (
          <span
            className="rounded-sm bg-meta/15 px-1 py-px text-[8px] uppercase tracking-wide text-meta"
            data-testid="provider-latency-overflow-hint"
            title={
              model.isOverflow
                ? 'aggregated overflow: providers beyond the per-window tracking cap fold into this bucket'
                : 'attempts with no recorded provider id'
            }
          >
            {model.isOverflow ? 'overflow' : 'unknown'}
          </span>
        )}
      </div>

      {model.available ? (
        <dl className="grid grid-cols-2 gap-x-2 gap-y-0.5">
          <FigureRow testId="provider-p50" label="p50" figure={model.p50} badge />
          <FigureRow testId="provider-p95" label="p95" figure={model.p95} />
          <FigureRow testId="provider-p99" label="p99" figure={model.p99} />
          <FigureRow testId="provider-error-rate" label="err rate" figure={model.errorRate} badge />
          <dt className="text-text-muted">served</dt>
          <dd className="text-right tabular-nums text-text" data-testid="provider-samples">
            {model.samplesText}
          </dd>
        </dl>
      ) : (
        // Absent ⇒ the no-samples state: an explicit `—` line, NEVER fabricated 0ms/0%.
        <p className="tabular-nums text-text-muted" data-testid="provider-latency-unavailable" data-quality="unavailable">
          — no samples in window
        </p>
      )}

      {model.errors.length > 0 && (
        <dl
          className="mt-1 grid grid-cols-2 gap-x-2 gap-y-0.5 border-t border-line/60 pt-1"
          data-testid="provider-error-distribution"
        >
          {model.errors.map((row) => (
            <div className="contents" key={row.class} data-testid={`provider-error-${row.class}`}>
              <dt className="truncate text-status-down" title={`${row.label} failures`}>
                {row.label}
              </dt>
              <dd className="text-right tabular-nums text-status-down">{row.count}</dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}
