/**
 * LatencyBreakdown (gap 10) — the per-flow latency waterfall for the FlowDetail inspector header.
 * Decomposes a turn's wall-clock into its phases (queue/normalize → routing → upstream wait →
 * prefill → generation → finalize) as a segmented bar + a per-segment legend, answering "was it
 * slow at the provider (prefill/TTFT) or just a long generation?" at a glance.
 *
 * Design language: matches the inspector's other header surfaces (the gap-09 `ContextGauge`, the
 * gap-07/08 `dl` rows) — a labelled block, `tabular-nums`, the shared status tokens, a
 * `data-quality` provenance attribute on every figure. A segment's fill color is keyed by phase
 * (not risk); an UNAVAILABLE segment shows a dashed, muted, un-filled gap (visibly "no reading",
 * NOT a 0-width slice), exactly like the gauge's unavailable rail.
 *
 * Data quality (consumes `latencyBreakdown`, never fabricates):
 *  - a phase whose endpoint is unknown ⇒ its segment is UNAVAILABLE: `—`, no bar width, a dashed
 *    legend marker — DISTINCT from a real ~0ms phase (`0ms`, a hairline solid segment).
 *  - TTFT is `measured` (first content delta) or, until then, an `estimated` first-visible-activity
 *    fallback that is LABELLED `est` (dashboard-visible activity, never claimed as upstream TTFB).
 *  - stream `tok/s` is `derived`; unavailable ⇒ `—`, never `0`.
 *  - a clock-disordered segment is FLAGGED (a skew marker), never rendered negative.
 */
import type { LatencyBreakdown as LatencyBreakdownModel, PhaseId, PhaseSegment, Quality } from './latencyBreakdown';
import { fmtElapsed, fmtTokensPerSec } from '../FlowTable/format';
import { cn } from '../../lib/cn';

/** Phase → the solid fill color token for its segment + legend swatch (static class strings). */
const PHASE_FILL: Record<PhaseId, string> = {
  queue: 'bg-text-muted',
  routing: 'bg-meta',
  upstream: 'bg-status-cooling',
  prefill: 'bg-accent',
  generation: 'bg-status-healthy',
  finalize: 'bg-line',
};

/** Quality → the small badge styling for an estimated/derived label (mirrors the gap-07/08 chips). */
const QUALITY_BADGE: Partial<Record<Quality, { cls: string; text: string; title: string }>> = {
  estimated: {
    cls: 'bg-status-cooling/15 text-status-cooling',
    text: 'est',
    title: 'estimated — derived first-visible-activity latency (first dashboard output segment), not the upstream first byte',
  },
  derived: {
    cls: 'bg-accent/15 text-accent',
    text: 'derived',
    title: 'a derived figure (computed from measured phases), not a directly measured one',
  },
};

/** A small provenance badge for an estimated/derived figure (none for measured/unavailable). */
function QualityBadge({ quality }: { quality: Quality }) {
  const b = QUALITY_BADGE[quality];
  if (!b) return null;
  return (
    <span
      className={cn('ml-1 rounded-sm px-1 py-0.5 text-[9px] uppercase tracking-wide', b.cls)}
      data-testid="latency-quality-badge"
      data-quality={quality}
      title={b.title}
    >
      {b.text}
    </span>
  );
}

/** One headline figure (TTFT / TTFB / total / tok/s) — value + provenance, `—` when unavailable. */
function FigureCell({
  testId,
  label,
  value,
  quality,
  detail,
}: {
  testId: string;
  label: string;
  value: string;
  quality: Quality;
  detail: string;
}) {
  const unavailable = quality === 'unavailable';
  return (
    <div className="flex flex-col" data-testid={testId} data-quality={quality} title={detail}>
      <span className="text-[10px] uppercase tracking-wide text-text-muted">{label}</span>
      <span className="flex items-baseline">
        <span className={cn('tabular-nums', unavailable ? 'text-text-muted' : 'text-text')}>{value}</span>
        <QualityBadge quality={quality} />
      </span>
    </div>
  );
}

export function LatencyBreakdown({ model }: { model: LatencyBreakdownModel }) {
  const { total, ttft, ttfb, rate, segments, knownSpanMs } = model;

  return (
    <div className="flex flex-col gap-1.5" data-testid="latency-breakdown">
      {/* Headline timing figures — the "Timing" line. */}
      <div className="grid grid-cols-4 gap-2">
        <FigureCell
          testId="latency-ttft"
          label="TTFT"
          value={fmtElapsed(ttft.valueMs)}
          quality={ttft.quality}
          detail={ttft.detail}
        />
        <FigureCell
          testId="latency-ttfb"
          label="wire TTFB"
          value={fmtElapsed(ttfb.valueMs)}
          quality={ttfb.quality}
          detail={ttfb.detail}
        />
        <FigureCell
          testId="latency-total"
          label="total"
          value={fmtElapsed(total.valueMs)}
          quality={total.quality}
          detail={total.detail}
        />
        <FigureCell
          testId="latency-rate"
          label="stream"
          value={fmtTokensPerSec(rate.tokensPerSec)}
          quality={rate.quality}
          detail={rate.detail}
        />
      </div>

      {/* The segmented waterfall bar. Each KNOWN segment fills a proportional slice; UNAVAILABLE
          segments are skipped (no width) — the bar represents only what was actually measured. */}
      <div
        className="flex h-2 w-full overflow-hidden rounded-full bg-line/40"
        data-testid="latency-bar"
        role="img"
        aria-label="latency phase breakdown"
      >
        {knownSpanMs > 0 &&
          segments.map((seg) =>
            seg.quality !== 'unavailable' && seg.durationMs !== null ? (
              <div
                key={seg.id}
                className={cn('h-full', PHASE_FILL[seg.id], seg.disordered && 'opacity-60')}
                // Width is the segment's share of the KNOWN span (never a fabricated slice). A real
                // 0ms segment gets a hairline minimum so it stays visible + distinct from absent.
                style={{ width: `${barWidthPct(seg.durationMs, knownSpanMs)}%` }}
                data-testid={`latency-seg-${seg.id}`}
                data-quality={seg.quality}
                title={`${seg.label}: ${fmtElapsed(seg.durationMs)} — ${seg.detail}`}
              />
            ) : null,
          )}
      </div>

      {/* Per-segment legend — every phase listed, with its duration + provenance. An UNAVAILABLE
          phase reads `—` with a dashed swatch (NOT 0ms); a disordered one shows a skew note. */}
      <dl className="grid grid-cols-[auto_1fr_auto] items-center gap-x-2 gap-y-0.5" data-testid="latency-legend">
        {segments.map((seg) => (
          <LegendRow key={seg.id} seg={seg} />
        ))}
      </dl>
    </div>
  );
}

/** One legend row: a phase swatch, its label, and its duration (or `—` when unavailable). */
function LegendRow({ seg }: { seg: PhaseSegment }) {
  const unavailable = seg.quality === 'unavailable';
  return (
    <div className="contents" data-testid={`latency-legend-${seg.id}`} data-quality={seg.quality}>
      {/* Swatch — a solid phase color for a measured/derived segment; a dashed muted box for an
          unavailable one (mirrors the gauge's dashed unavailable rail). */}
      <span
        className={cn(
          'h-2 w-2 rounded-sm',
          unavailable ? 'border border-dashed border-line/70 bg-transparent' : PHASE_FILL[seg.id],
        )}
        aria-hidden="true"
      />
      <span className={cn('text-[11px]', unavailable ? 'text-text-muted' : 'text-text')} title={seg.detail}>
        {seg.label}
        {seg.disordered && (
          <span className="ml-1 text-[9px] uppercase tracking-wide text-status-cooling" title="clock skew — endpoints out of order, clamped to 0 (not negative)" data-testid={`latency-skew-${seg.id}`}>
            skew
          </span>
        )}
      </span>
      <span
        className={cn('justify-self-end text-[11px] tabular-nums', unavailable ? 'text-text-muted' : 'text-text')}
        data-testid={`latency-dur-${seg.id}`}
        title={unavailable ? seg.detail : `${seg.label}: ${seg.detail}`}
      >
        {/* `—` for unavailable (an endpoint was unknown), a real duration otherwise. A measured 0ms
            reads `0ms` — distinct from `—`. */}
        {unavailable ? '—' : fmtElapsed(seg.durationMs)}
      </span>
    </div>
  );
}

/**
 * A segment's width as a % of the known span. A real `0ms` (or a sub-percent) segment is floored to
 * a small minimum so it stays VISIBLE (a measured phase is never invisible), but only when there is
 * more than one known segment (a single segment fills the bar). Unavailable segments never reach
 * here (the caller skips them).
 */
function barWidthPct(durationMs: number, knownSpanMs: number): number {
  if (knownSpanMs <= 0) return 0;
  const pct = (durationMs / knownSpanMs) * 100;
  // Floor a real-but-tiny segment to a hairline so a measured ~0ms phase is still seen.
  return pct > 0 && pct < 1.5 ? 1.5 : pct;
}
