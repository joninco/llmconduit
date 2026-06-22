/**
 * latencyBreakdown (gap 10) — decompose a turn's wall-clock into its phases for the FlowDetail
 * inspector waterfall. Pure + DOM-free (sibling of `tokenEconomics.ts`/`contextUtilization.ts`)
 * so it is unit-testable and the bar can never disagree with the numbers it derives.
 *
 * "Was it slow at the provider (prefill/TTFT) or just a long generation?" — the answer is the
 * SEGMENTED decomposition of the turn:
 *
 *   ingress → normalize/queue → routing → upstream wait (wire TTFB) → prefill→first content
 *           → generation/streaming → finalize
 *
 * SOURCES (spec 10 — the oracle):
 *  - the gap-02 phase epochs (`ingress_ms`, `normalization_done_ms`, `routing_decision_ms`,
 *    `first_content_delta_ms`, `stream_end_ms`, `finalize_ms`) flattened on the flow — MEASURED.
 *  - the gap-03 served attempt's `first_upstream_byte_ms` (flow-level `first_upstream_byte_ms`
 *    fallback) — MEASURED wire TTFB that ENRICHES the upstream-wait segment.
 *  - the monitor `output` segments' `timestamp_ms` — the honest DERIVED fallback for TTFT until
 *    `first_content_delta_ms` is populated for a flow ("first-visible-activity latency", labelled
 *    as dashboard-visible activity, NOT upstream first byte).
 *
 * DATA-QUALITY INVARIANTS (the heart of an honest breakdown — every one is asserted in tests):
 *  - A segment is derived from a PAIR of KNOWN epoch timestamps. If EITHER endpoint is unknown the
 *    sub-duration is UNAVAILABLE — rendered as a gap/`—`, NEVER a `0ms`/zero-width segment.
 *  - "phase didn't happen / unmeasured" (`unavailable`) is DISTINCT from "phase took ~0ms"
 *    (`measured`/`derived`, `durationMs === 0`): the former carries no bar width + reads `—`; the
 *    latter is a real (possibly hairline) segment that reads `0ms`.
 *  - NO negative durations: clocks that disorder (end < start) are CLAMPED to 0 and FLAGGED
 *    (`disordered`) rather than rendered negative.
 *  - NO fabricated total: the wall-clock total is itself derived from a known pair (ingress→the
 *    latest known right edge); when no usable span exists it is `unavailable` (`—`).
 *  - Every segment is TAGGED `measured | derived | estimated | unavailable`; an `estimated`
 *    segment (the derived first-visible-activity TTFT fallback) is LABELLED as such.
 *  - Stream `tok/s` is `derived` (completion ÷ stream duration); unavailable ⇒ `—`, never `0`.
 */
import type { Attempt, DebugSegment, FlowDetail, FlowSummary, Usage } from '../../api/types';

/** Provenance of a derived figure — mirrors the dashboard's measured/derived/unavailable tags. */
export type Quality = 'measured' | 'derived' | 'estimated' | 'unavailable';

/** A stable id per phase segment (drives keys, test selectors, and the color map). */
export type PhaseId =
  | 'queue' // ingress → normalization (inbound normalize / queue)
  | 'routing' // normalization → routing decision
  | 'upstream' // routing → first upstream byte (wire TTFB) | routing → first content (no TTFB)
  | 'prefill' // first upstream byte → first content delta (provider prefill→first token)
  | 'generation' // first content delta → stream end (token streaming)
  | 'finalize'; // stream end → finalize (server-side wrap-up)

/** One segment of the waterfall. `durationMs` is meaningful ONLY when `quality !== 'unavailable'`. */
export interface PhaseSegment {
  id: PhaseId;
  /** Human label for the row/segment. */
  label: string;
  /**
   * The sub-duration in ms. `null` ⇒ UNAVAILABLE (an endpoint was unknown) — render `—`, no bar.
   * A finite `0` is a REAL measured ~0ms phase (distinct from unavailable). Never negative.
   */
  durationMs: number | null;
  /** `measured`/`derived` when both endpoints were known; `unavailable` when one was missing. */
  quality: Quality;
  /**
   * True when the segment's endpoints were observed out of order (end < start) and the duration
   * was CLAMPED to 0 rather than rendered negative. The UI flags it (a clock-skew note).
   */
  disordered: boolean;
  /** A short, human "why" for the title attribute (which two phases bound it / why unavailable). */
  detail: string;
}

/** A render-ready figure: the formatted-elsewhere value lives in the component; here we keep the raw. */
export interface Figure {
  /** The raw ms (TTFT/total) — `null` ⇒ unavailable. Never a fabricated `0` for "unmeasured". */
  valueMs: number | null;
  quality: Quality;
  /** A short provenance note for the title attribute. */
  detail: string;
}

/** Stream throughput figure (tokens/sec) — always `derived` when computable, else unavailable. */
export interface RateFigure {
  /** Tokens per second — `null` ⇒ unavailable (no completion count or no stream duration). */
  tokensPerSec: number | null;
  quality: Quality;
  detail: string;
}

/** The full latency breakdown the inspector renders. */
export interface LatencyBreakdown {
  /** Total wall-clock (ingress → latest known right edge). `unavailable` ⇒ `—`, never fabricated. */
  total: Figure;
  /**
   * True TTFT — the first CONTENT delta the client saw, relative to ingress.
   *  - `measured` from `first_content_delta_ms − ingress_ms` (gap 02).
   *  - else `estimated` from the first monitor `output` segment `timestamp_ms − started_ms`
   *    (the "first-visible-activity" fallback) — LABELLED as dashboard-visible activity.
   *  - else `unavailable` (`—`).
   */
  ttft: Figure;
  /** Wire TTFB — the served attempt's first upstream byte, relative to ingress (gap 03). */
  ttfb: Figure;
  /** Stream tok/s — completion ÷ generation duration (`derived`); unavailable ⇒ `—`. */
  rate: RateFigure;
  /** The ordered waterfall segments (always all six, each tagged; unavailable ones render `—`). */
  segments: PhaseSegment[];
  /**
   * The sum of the KNOWN (non-unavailable) segment durations — the denominator the bar uses to
   * size each segment's width. `0` when nothing is known (the bar then shows an empty rail).
   */
  knownSpanMs: number;
}

/** A flow shape carrying the spine fields — both `FlowSummary` and `FlowDetail` satisfy it. */
export type SpineFlow = Pick<
  FlowSummary & FlowDetail,
  | 'started_ms'
  | 'ingress_ms'
  | 'normalization_done_ms'
  | 'routing_decision_ms'
  | 'first_content_delta_ms'
  | 'stream_end_ms'
  | 'finalize_ms'
  | 'finished_ms'
  | 'elapsed_ms'
  | 'attempts'
  | 'first_upstream_byte_ms'
  | 'usage'
>;

/** A finite, non-negative epoch-ms, else null (an absent/`0`-sentinel/garbage timestamp). */
function epoch(v: number | null | undefined): number | null {
  // A real wall-clock epoch is a large positive integer; treat a non-finite or non-positive value
  // as "unmeasured" (the Rust side never emits `0` for an occurred phase — `skip_serializing_if`).
  return typeof v === 'number' && Number.isFinite(v) && v > 0 ? v : null;
}

/**
 * Derive a sub-duration between two phase epochs. Returns the segment tag:
 *  - both known + in order ⇒ `{ durationMs >= 0, quality }`.
 *  - both known but disordered (end < start) ⇒ clamped to 0, `disordered: true` (never negative).
 *  - either unknown ⇒ `durationMs: null, quality: 'unavailable'` (render `—`, no bar).
 */
function span(
  id: PhaseId,
  label: string,
  startMs: number | null,
  endMs: number | null,
  quality: Quality,
  okDetail: string,
  missingDetail: string,
): PhaseSegment {
  if (startMs === null || endMs === null) {
    return { id, label, durationMs: null, quality: 'unavailable', disordered: false, detail: missingDetail };
  }
  const raw = endMs - startMs;
  if (raw < 0) {
    // Clock disorder: clamp to 0 + flag, NEVER render a negative duration (DQ invariant).
    return { id, label, durationMs: 0, quality, disordered: true, detail: `${okDetail} (clock skew: clamped)` };
  }
  return { id, label, durationMs: raw, quality, disordered: false, detail: okDetail };
}

/** The first monitor `output` segment's epoch (the derived "first-visible-activity" instant). */
function firstOutputEpoch(segments: DebugSegment[] | undefined): number | null {
  if (!segments) return null;
  for (const s of segments) {
    if (s.kind === 'output') {
      const e = epoch(s.timestamp_ms);
      if (e !== null) return e;
    }
  }
  return null;
}

/** The SERVED attempt (status `served`), if any — its `first_upstream_byte_ms` is the wire TTFB. */
function servedAttempt(attempts: Attempt[] | undefined): Attempt | null {
  if (!attempts) return null;
  return attempts.find((a) => a.status === 'served') ?? null;
}

/** completion tokens from usage, or null when unreported. */
function completionTokens(usage: Usage | null | undefined): number | null {
  if (!usage) return null;
  return typeof usage.completion === 'number' && Number.isFinite(usage.completion) ? usage.completion : null;
}

/**
 * Build the latency breakdown from the (merged live+frozen) flow spine + the monitor output
 * segments. `flow` is the freshest spine source (caller merges live row + frozen detail);
 * `monitorOutputs` are the joined monitor segments (for the derived TTFT fallback).
 */
export function latencyBreakdown(
  flow: SpineFlow | null | undefined,
  monitorOutputs?: DebugSegment[],
): LatencyBreakdown {
  // Anchor (ingress): prefer the measured `ingress_ms`; else `started_ms` (≈ ingress, always set).
  const startedMs = flow ? epoch(flow.started_ms) : null;
  const ingress = (flow ? epoch(flow.ingress_ms) : null) ?? startedMs;
  const normalize = flow ? epoch(flow.normalization_done_ms) : null;
  const routing = flow ? epoch(flow.routing_decision_ms) : null;
  const firstContent = flow ? epoch(flow.first_content_delta_ms) : null;
  const streamEnd = flow ? epoch(flow.stream_end_ms) : null;
  const finalize = flow ? epoch(flow.finalize_ms) : null;

  // Wire TTFB: the served attempt's first byte, else the flow-level `first_upstream_byte_ms`.
  const served = flow ? servedAttempt(flow.attempts) : null;
  const upstreamByte = (served ? epoch(served.first_upstream_byte_ms) : null)
    ?? (flow ? epoch(flow.first_upstream_byte_ms) : null);

  // ---- Segments (each from a KNOWN pair; a missing endpoint ⇒ unavailable, never 0ms) ----
  const segments: PhaseSegment[] = [
    span(
      'queue',
      'queue · normalize',
      ingress,
      normalize,
      'measured',
      'ingress → normalization',
      'unavailable — normalization not reached (errored before normalize, or unmeasured)',
    ),
    span(
      'routing',
      'routing',
      normalize,
      routing,
      'measured',
      'normalization → routing decision',
      'unavailable — routing decision not reached (never lowered to the wire, or unmeasured)',
    ),
    // Upstream wait: routing → wire TTFB (gap-03 enriched). When no TTFB was measured the segment
    // is UNAVAILABLE (we do NOT silently stretch it to first-content — that would conflate the
    // provider wait with prefill). The prefill segment then covers routing→first-content instead.
    span(
      'upstream',
      'upstream wait (TTFB)',
      routing,
      upstreamByte,
      'measured',
      'routing → first upstream byte (wire TTFB)',
      'unavailable — no upstream first-byte measured (attempt failed pre-headers, or unmeasured)',
    ),
    // Prefill→first content: from the wire first byte (preferred) to the first CONTENT delta. When
    // the wire byte is unknown, anchor on routing so the provider-side latency is still shown
    // (labelled accordingly) rather than dropped — still a KNOWN pair (routing, firstContent).
    upstreamByte !== null
      ? span(
          'prefill',
          'prefill → first token',
          upstreamByte,
          firstContent,
          'measured',
          'first upstream byte → first content delta',
          'unavailable — no first content delta (errored before content, or unmeasured)',
        )
      : span(
          'prefill',
          'upstream → first token',
          routing,
          firstContent,
          'measured',
          'routing → first content delta (no wire TTFB measured)',
          'unavailable — no first content delta (errored before content, or unmeasured)',
        ),
    span(
      'generation',
      'generation (stream)',
      firstContent,
      streamEnd,
      'measured',
      'first content delta → stream end',
      'unavailable — stream did not complete after first content (errored/cancelled, or unmeasured)',
    ),
    span(
      'finalize',
      'finalize',
      streamEnd,
      finalize,
      'measured',
      'stream end → finalize',
      'unavailable — finalize not reached after stream end (or unmeasured)',
    ),
  ];

  // The bar denominator = the sum of the KNOWN segment durations (unavailable ones contribute 0
  // width, NOT a fabricated slice). Never includes a negative (spans clamp to 0).
  const knownSpanMs = segments.reduce((sum, s) => sum + (s.durationMs ?? 0), 0);

  // ---- Total wall-clock (ingress → the latest KNOWN right edge) — never fabricated ----
  // Right edge precedence: finalize ≥ stream_end ≥ first_content ≥ upstreamByte ≥ routing ≥
  // normalize. We use the latest present one so an in-flight/errored flow still shows a bounded
  // span (e.g. ingress→first_content for a still-streaming turn), but only from KNOWN epochs.
  const rightEdge = finalize ?? streamEnd ?? firstContent ?? upstreamByte ?? routing ?? normalize;
  const total: Figure = ingress !== null && rightEdge !== null
    ? {
        valueMs: Math.max(0, rightEdge - ingress),
        quality: 'measured',
        detail: 'wall-clock from ingress to the latest measured phase',
      }
    : { valueMs: null, quality: 'unavailable', detail: 'total unavailable — no measured span yet' };

  // ---- TTFT: measured from first_content, else the derived first-visible-activity fallback ----
  const ttft = computeTtft(ingress, startedMs, firstContent, monitorOutputs);

  // ---- Wire TTFB relative to ingress (gap 03) ----
  const ttfb: Figure = ingress !== null && upstreamByte !== null
    ? {
        valueMs: Math.max(0, upstreamByte - ingress),
        quality: 'measured',
        detail: 'wire time-to-first-byte: ingress → the served attempt’s first upstream byte',
      }
    : { valueMs: null, quality: 'unavailable', detail: 'wire TTFB unavailable — no upstream first byte measured' };

  // ---- Stream tok/s (derived) ----
  const rate = computeRate(flow?.usage, firstContent, streamEnd);

  return { total, ttft, ttfb, rate, segments, knownSpanMs };
}

/** TTFT figure: measured (first content) → derived (first visible activity) → unavailable. */
function computeTtft(
  ingress: number | null,
  startedMs: number | null,
  firstContent: number | null,
  monitorOutputs: DebugSegment[] | undefined,
): Figure {
  // 1. MEASURED — the true client TTFT (first content delta), relative to ingress (gap 02).
  if (ingress !== null && firstContent !== null) {
    return {
      valueMs: Math.max(0, firstContent - ingress),
      quality: 'measured',
      detail: 'true TTFT: ingress → first content delta to the client (measured)',
    };
  }
  // 2. ESTIMATED — the derived "first-visible-activity latency": the first monitor `output`
  // segment timestamp relative to `started_ms`. This is DASHBOARD-VISIBLE activity, NOT the
  // upstream first byte — labelled as such so it is never mistaken for the measured TTFT.
  const firstOutput = firstOutputEpoch(monitorOutputs);
  const anchor = startedMs ?? ingress;
  if (anchor !== null && firstOutput !== null) {
    return {
      valueMs: Math.max(0, firstOutput - anchor),
      quality: 'estimated',
      detail: 'first-visible-activity latency (derived from the first monitor output segment — not upstream first byte)',
    };
  }
  // 3. UNAVAILABLE — no measured TTFT and no visible activity yet (`—`, never a fabricated 0).
  return { valueMs: null, quality: 'unavailable', detail: 'TTFT unavailable — no first content delta and no visible activity' };
}

/** tok/s figure (derived): completion ÷ (stream_end − first_content) seconds; else unavailable. */
function computeRate(
  usage: Usage | null | undefined,
  firstContent: number | null,
  streamEnd: number | null,
): RateFigure {
  const completion = completionTokens(usage);
  // Need a KNOWN generation span AND a reported completion count.
  if (completion === null || firstContent === null || streamEnd === null) {
    return {
      tokensPerSec: null,
      quality: 'unavailable',
      detail: 'tok/s unavailable — needs a measured stream duration and a reported completion count',
    };
  }
  const durationMs = streamEnd - firstContent;
  if (durationMs <= 0) {
    // A zero/negative stream window cannot yield an honest rate (no division by zero / no fake ∞).
    return {
      tokensPerSec: null,
      quality: 'unavailable',
      detail: 'tok/s unavailable — stream duration is zero/disordered',
    };
  }
  return {
    tokensPerSec: (completion / durationMs) * 1000,
    quality: 'derived',
    detail: 'stream throughput: completion tokens ÷ (stream end − first content) (derived)',
  };
}
