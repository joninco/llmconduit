import { describe, it, expect } from 'vitest';
import { latencyBreakdown, type SpineFlow } from './latencyBreakdown';
import type { Attempt, DebugSegment } from '../../api/types';

/** A base epoch (a real, large wall-clock ms) so the `> 0` epoch guard is exercised honestly. */
const T = 1_700_000_000_000;

function flow(over: Partial<SpineFlow> = {}): SpineFlow {
  return { started_ms: T, ...over };
}

function output(timestamp_ms: number): DebugSegment {
  return { timestamp_ms, kind: 'output', text: 'x' };
}

/** Pull a segment by id. */
function seg(model: ReturnType<typeof latencyBreakdown>, id: string) {
  const s = model.segments.find((x) => x.id === id);
  if (!s) throw new Error(`no segment ${id}`);
  return s;
}

describe('latencyBreakdown — honest per-flow phase decomposition (gap 10)', () => {
  it('all phases present ⇒ a full breakdown with correct measured sub-durations', () => {
    const f = flow({
      started_ms: T,
      ingress_ms: T,
      normalization_done_ms: T + 30,
      routing_decision_ms: T + 50,
      first_upstream_byte_ms: T + 270, // wire TTFB
      first_content_delta_ms: T + 450, // client TTFT
      stream_end_ms: T + 1450,
      finalize_ms: T + 1470,
      usage: { prompt: 100, completion: 500, total: 600 },
      attempts: [{ provider: 'p', model: 'm', start_ms: T + 50, end_ms: T + 270, first_upstream_byte_ms: T + 270, status: 'served' }],
    });
    const m = latencyBreakdown(f, []);

    // Each segment is derived from a KNOWN pair, with the exact sub-duration.
    expect(seg(m, 'queue')).toMatchObject({ durationMs: 30, quality: 'measured', disordered: false });
    expect(seg(m, 'routing')).toMatchObject({ durationMs: 20, quality: 'measured' });
    expect(seg(m, 'upstream')).toMatchObject({ durationMs: 220, quality: 'measured' }); // routing→TTFB
    expect(seg(m, 'prefill')).toMatchObject({ durationMs: 180, quality: 'measured' }); // TTFB→first content
    expect(seg(m, 'generation')).toMatchObject({ durationMs: 1000, quality: 'measured' }); // content→stream end
    expect(seg(m, 'finalize')).toMatchObject({ durationMs: 20, quality: 'measured' });

    // TTFT is MEASURED (first content − ingress), tok/s DERIVED, total measured + not fabricated.
    expect(m.ttft).toMatchObject({ valueMs: 450, quality: 'measured' });
    expect(m.ttfb).toMatchObject({ valueMs: 270, quality: 'measured' });
    expect(m.total).toMatchObject({ valueMs: 1470, quality: 'measured' });
    // tok/s = 500 tokens / 1.0s = 500.
    expect(m.rate.quality).toBe('derived');
    expect(m.rate.tokensPerSec).toBeCloseTo(500, 5);
    // The bar denominator is the sum of the known segments (no fabricated slice).
    expect(m.knownSpanMs).toBe(30 + 20 + 220 + 180 + 1000 + 20);
  });

  it('a missing phase (no first_content_delta — error before content) ⇒ those segments are UNAVAILABLE, not 0', () => {
    const f = flow({
      ingress_ms: T,
      normalization_done_ms: T + 30,
      routing_decision_ms: T + 50,
      finalize_ms: T + 800,
      // NO first_upstream_byte, NO first_content_delta, NO stream_end (errored before content).
      attempts: [{ provider: 'p', model: 'm', start_ms: T + 50, end_ms: T + 800, status: 'failed', error_class: 'http_status' }],
    });
    const m = latencyBreakdown(f, []);

    // queue + routing are measured (known pairs).
    expect(seg(m, 'queue').quality).toBe('measured');
    expect(seg(m, 'routing').quality).toBe('measured');
    // upstream wait UNAVAILABLE (no wire byte) — `durationMs` null, NOT 0.
    expect(seg(m, 'upstream')).toMatchObject({ durationMs: null, quality: 'unavailable' });
    // prefill UNAVAILABLE (no first content delta) — null, NOT 0.
    expect(seg(m, 'prefill')).toMatchObject({ durationMs: null, quality: 'unavailable' });
    // generation UNAVAILABLE (no first content / stream end) — null, NOT 0.
    expect(seg(m, 'generation')).toMatchObject({ durationMs: null, quality: 'unavailable' });
    // finalize UNAVAILABLE (no stream end endpoint) — the LEFT endpoint is missing ⇒ `—`, not 0.
    expect(seg(m, 'finalize')).toMatchObject({ durationMs: null, quality: 'unavailable' });

    // TTFT unavailable (no content + no monitor output) ⇒ `—`, never 0; tok/s unavailable.
    expect(m.ttft).toMatchObject({ valueMs: null, quality: 'unavailable' });
    expect(m.ttfb).toMatchObject({ valueMs: null, quality: 'unavailable' });
    expect(m.rate).toMatchObject({ tokensPerSec: null, quality: 'unavailable' });
    // The total still derives from a KNOWN pair (ingress → finalize), not fabricated.
    expect(m.total).toMatchObject({ valueMs: 800, quality: 'measured' });
    // No unavailable segment leaks width into the bar denominator.
    expect(m.knownSpanMs).toBe(30 + 20);
  });

  it('a served attempt with first_upstream_byte ENRICHES the upstream segment (wire TTFB)', () => {
    const withByte = latencyBreakdown(
      flow({ ingress_ms: T, routing_decision_ms: T + 50, first_content_delta_ms: T + 400, first_upstream_byte_ms: T + 250 }),
      [],
    );
    // Upstream wait = routing→TTFB = 200ms; prefill = TTFB→content = 150ms.
    expect(seg(withByte, 'upstream')).toMatchObject({ durationMs: 200, quality: 'measured' });
    expect(seg(withByte, 'prefill')).toMatchObject({ durationMs: 150, quality: 'measured' });
    expect(withByte.ttfb).toMatchObject({ valueMs: 250, quality: 'measured' });

    // WITHOUT the wire byte: upstream wait is unavailable, and the prefill segment is shown as a
    // SEPARATELY-LABELLED `derived` "routing → first token" span (a known pair, but NOT the measured
    // prefill phase — its true left endpoint, the wire first byte, is absent). Gap-10 review round 1:
    // it must NOT be tagged `measured`, and it must NOT be labelled as the measured prefill.
    const noByte = latencyBreakdown(
      flow({ ingress_ms: T, routing_decision_ms: T + 50, first_content_delta_ms: T + 400 }),
      [],
    );
    expect(seg(noByte, 'upstream')).toMatchObject({ durationMs: null, quality: 'unavailable' });
    const noBytePrefill = seg(noByte, 'prefill');
    expect(noBytePrefill).toMatchObject({ durationMs: 350, quality: 'derived' }); // routing→content
    // It is DERIVED, never measured, and labelled as a routing→first-token span (not "prefill").
    expect(noBytePrefill.quality).not.toBe('measured');
    expect(noBytePrefill.label).toMatch(/routing/i);
    expect(noBytePrefill.label).not.toMatch(/^prefill/i);
    expect(noBytePrefill.detail).toMatch(/derived/i);
    expect(noBytePrefill.detail).toMatch(/not the measured prefill/i);
    expect(noByte.ttfb).toMatchObject({ valueMs: null, quality: 'unavailable' });
  });

  it('prefill is MEASURED only with a real wire TTFB; with TTFB absent (content present) it is a labelled DERIVED span, not measured (gap-10 review round 1)', () => {
    // BOTH endpoints present (wire first byte + first content) ⇒ a genuine MEASURED prefill phase.
    const both = latencyBreakdown(
      flow({ ingress_ms: T, routing_decision_ms: T + 50, first_upstream_byte_ms: T + 240, first_content_delta_ms: T + 400 }),
      [],
    );
    const measuredPrefill = seg(both, 'prefill');
    expect(measuredPrefill).toMatchObject({ durationMs: 160, quality: 'measured' }); // TTFB→content
    expect(measuredPrefill.label).toMatch(/prefill/i);
    expect(measuredPrefill.detail).toMatch(/first upstream byte/i);

    // TTFB ABSENT but first content present ⇒ prefill is NOT measured. Honest options: a labelled
    // `derived` stand-in span (chosen here) — never a `measured` prefill built from routing→content.
    const noTtfb = latencyBreakdown(
      flow({ ingress_ms: T, routing_decision_ms: T + 50, first_content_delta_ms: T + 400 }),
      [],
    );
    const derivedSpan = seg(noTtfb, 'prefill');
    expect(derivedSpan.quality).toBe('derived');
    expect(derivedSpan.quality).not.toBe('measured');
    // The known pair still yields the real routing→content duration (no fabrication, no negative).
    expect(derivedSpan.durationMs).toBe(350);
    expect(derivedSpan.durationMs).toBeGreaterThanOrEqual(0);
    // Labelled as a routing→first-token span — never presented as the measured prefill phase.
    expect(derivedSpan.label).not.toMatch(/^prefill/i);
    expect(derivedSpan.detail.toLowerCase()).toContain('not the measured prefill');
  });

  it('reads the wire TTFB from the SERVED attempt, ignoring failed attempts', () => {
    const attempts: Attempt[] = [
      { provider: 'a', model: 'm', start_ms: T + 50, end_ms: T + 100, status: 'failed', error_class: 'connect' },
      { provider: 'b', model: 'm', start_ms: T + 100, end_ms: T + 260, first_upstream_byte_ms: T + 260, status: 'served' },
    ];
    const m = latencyBreakdown(flow({ ingress_ms: T, routing_decision_ms: T + 100, first_content_delta_ms: T + 400, attempts }), []);
    // TTFB is the SERVED attempt's byte (T+260), not the failed one (which has none).
    expect(m.ttfb.valueMs).toBe(260);
    expect(seg(m, 'upstream')).toMatchObject({ durationMs: 160, quality: 'measured' }); // routing(100)→TTFB(260)
  });

  it('disordered timestamps (end < start) clamp to 0 + flag, NEVER negative', () => {
    // routing BEFORE normalization (clock skew) ⇒ the routing segment clamps to 0 + disordered.
    const m = latencyBreakdown(
      flow({ ingress_ms: T, normalization_done_ms: T + 100, routing_decision_ms: T + 40 }),
      [],
    );
    const routing = seg(m, 'routing');
    expect(routing.durationMs).toBe(0); // clamped, NOT -60
    expect(routing.disordered).toBe(true);
    expect(routing.durationMs).toBeGreaterThanOrEqual(0);
    // The clamped 0 is a REAL (disordered) segment, distinct from unavailable.
    expect(routing.quality).not.toBe('unavailable');
  });

  it('a genuine measured ~0ms phase is DISTINCT from unavailable (0 vs null)', () => {
    // normalization stamped at the SAME ms as ingress ⇒ a real 0ms queue segment (measured), while
    // routing is absent ⇒ the routing segment is unavailable (null).
    const m = latencyBreakdown(flow({ ingress_ms: T, normalization_done_ms: T }), []);
    expect(seg(m, 'queue')).toMatchObject({ durationMs: 0, quality: 'measured', disordered: false });
    expect(seg(m, 'routing')).toMatchObject({ durationMs: null, quality: 'unavailable' });
    // The 0ms queue counts as known span; the unavailable routing contributes nothing.
    expect(m.knownSpanMs).toBe(0);
  });

  it('no measured TTFT ⇒ DERIVED first-visible-activity fallback from monitor output, labelled estimated', () => {
    // No first_content_delta, but a monitor `output` segment 320ms after started_ms.
    const m = latencyBreakdown(
      flow({ started_ms: T, ingress_ms: T, normalization_done_ms: T + 30, routing_decision_ms: T + 50 }),
      [output(T - 5), output(T + 320)].slice(1), // first usable output at +320 (the −5 is filtered as garbage)
    );
    expect(m.ttft).toMatchObject({ valueMs: 320, quality: 'estimated' });
    // It is explicitly labelled as activity, NOT upstream first byte.
    expect(m.ttft.detail).toMatch(/first-visible-activity/i);
    expect(m.ttft.detail).not.toMatch(/measured/i);
  });

  it('the MEASURED first_content_delta WINS over the monitor-output fallback', () => {
    const m = latencyBreakdown(
      flow({ started_ms: T, ingress_ms: T, first_content_delta_ms: T + 200 }),
      [output(T + 999)], // a later monitor output must NOT override the measured TTFT
    );
    expect(m.ttft).toMatchObject({ valueMs: 200, quality: 'measured' });
  });

  it('ignores non-output monitor segments for the derived TTFT fallback', () => {
    const reasoning: DebugSegment = { timestamp_ms: T + 100, kind: 'reasoning', text: 'think' };
    const tool: DebugSegment = { timestamp_ms: T + 150, kind: 'tool', text: '{}' };
    const m = latencyBreakdown(flow({ started_ms: T, ingress_ms: T }), [reasoning, tool]);
    // Only `output` segments count — none here ⇒ TTFT unavailable (`—`), never 0.
    expect(m.ttft).toMatchObject({ valueMs: null, quality: 'unavailable' });
  });

  it('tok/s is unavailable (not 0/∞) when the stream duration is zero or completion is unreported', () => {
    // Zero stream window (first_content == stream_end) ⇒ no honest rate.
    const zeroWindow = latencyBreakdown(
      flow({ ingress_ms: T, first_content_delta_ms: T + 400, stream_end_ms: T + 400, usage: { prompt: 1, completion: 10, total: 11 } }),
      [],
    );
    expect(zeroWindow.rate).toMatchObject({ tokensPerSec: null, quality: 'unavailable' });

    // Unreported completion ⇒ unavailable even with a valid stream window.
    const noUsage = latencyBreakdown(
      flow({ ingress_ms: T, first_content_delta_ms: T + 400, stream_end_ms: T + 1400, usage: null }),
      [],
    );
    expect(noUsage.rate).toMatchObject({ tokensPerSec: null, quality: 'unavailable' });
  });

  it('an empty/absent spine ⇒ everything unavailable, nothing fabricated', () => {
    const empty = latencyBreakdown(null, []);
    expect(empty.total).toMatchObject({ valueMs: null, quality: 'unavailable' });
    expect(empty.ttft).toMatchObject({ valueMs: null, quality: 'unavailable' });
    expect(empty.ttfb).toMatchObject({ valueMs: null, quality: 'unavailable' });
    expect(empty.rate).toMatchObject({ tokensPerSec: null, quality: 'unavailable' });
    expect(empty.knownSpanMs).toBe(0);
    expect(empty.segments.every((s) => s.quality === 'unavailable' && s.durationMs === null)).toBe(true);
  });

  it('falls back to started_ms as the ingress anchor when ingress_ms is absent', () => {
    // No ingress_ms, but started_ms + first_content ⇒ TTFT is still measured from started_ms.
    const m = latencyBreakdown(flow({ started_ms: T, first_content_delta_ms: T + 350 }), []);
    expect(m.ttft).toMatchObject({ valueMs: 350, quality: 'measured' });
  });

  it('treats a 0 / non-positive phase epoch as UNMEASURED (don’t-lie-with-zeros), not a real instant', () => {
    // A `0` first_content_delta (a sentinel that must never occur on the wire) is rejected as
    // unmeasured ⇒ TTFT unavailable, NOT a fabricated huge value from `0 - ingress`.
    const m = latencyBreakdown(flow({ ingress_ms: T, first_content_delta_ms: 0 }), []);
    expect(m.ttft.quality).toBe('unavailable');
  });
});
