import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { LatencyBreakdown } from './LatencyBreakdown';
import { latencyBreakdown, type SpineFlow } from './latencyBreakdown';
import type { DebugSegment } from '../../api/types';

const T = 1_700_000_000_000;

function flow(over: Partial<SpineFlow> = {}): SpineFlow {
  return { started_ms: T, ...over };
}

function renderModel(f: SpineFlow | null, outputs: DebugSegment[] = []) {
  return render(<LatencyBreakdown model={latencyBreakdown(f, outputs)} />);
}

describe('LatencyBreakdown component (gap 10)', () => {
  afterEach(cleanup);

  it('renders a measured TTFT (no est badge) + a derived tok/s + the full waterfall', () => {
    const { getByTestId, queryByTestId } = renderModel(
      flow({
        ingress_ms: T,
        normalization_done_ms: T + 30,
        routing_decision_ms: T + 50,
        first_upstream_byte_ms: T + 270,
        first_content_delta_ms: T + 450,
        stream_end_ms: T + 1450,
        finalize_ms: T + 1470,
        usage: { prompt: 100, completion: 500, total: 600 },
      }),
    );
    // TTFT is measured ⇒ no estimated badge inside the TTFT cell.
    const ttft = getByTestId('latency-ttft');
    expect(ttft.getAttribute('data-quality')).toBe('measured');
    expect(ttft.textContent).toContain('450ms');
    expect(ttft.querySelector('[data-testid="latency-quality-badge"]')).toBeNull();

    // tok/s is derived (500/s) and labelled derived.
    const rate = getByTestId('latency-rate');
    expect(rate.getAttribute('data-quality')).toBe('derived');
    expect(rate.textContent).toContain('tok/s');

    // Every segment has a colored fill (none unavailable) in the bar.
    for (const id of ['queue', 'routing', 'upstream', 'prefill', 'generation', 'finalize']) {
      expect(getByTestId(`latency-seg-${id}`)).toBeTruthy();
      expect(getByTestId(`latency-legend-${id}`).getAttribute('data-quality')).toBe('measured');
    }
    // No skew flag on a well-ordered flow.
    expect(queryByTestId('latency-skew-routing')).toBeNull();
  });

  it('renders an UNAVAILABLE segment as `—` with NO bar fill — never 0ms (error before content)', () => {
    const { getByTestId, queryByTestId } = renderModel(
      flow({
        ingress_ms: T,
        normalization_done_ms: T + 30,
        routing_decision_ms: T + 50,
        finalize_ms: T + 800,
        // no first content / stream end / wire byte
      }),
    );
    // The prefill + generation segments are unavailable: legend reads `—`, NO bar segment rendered.
    const prefill = getByTestId('latency-legend-prefill');
    expect(prefill.getAttribute('data-quality')).toBe('unavailable');
    expect(getByTestId('latency-dur-prefill').textContent).toBe('—');
    expect(queryByTestId('latency-seg-prefill')).toBeNull(); // no width in the bar
    expect(queryByTestId('latency-seg-generation')).toBeNull();

    // TTFT cell reads `—` (unavailable), not 0ms.
    const ttft = getByTestId('latency-ttft');
    expect(ttft.getAttribute('data-quality')).toBe('unavailable');
    expect(ttft.textContent).toContain('—');
    // The measured queue/routing segments still render in the bar.
    expect(getByTestId('latency-seg-queue')).toBeTruthy();
    expect(getByTestId('latency-seg-routing')).toBeTruthy();
  });

  it('renders the no-TTFB prefill as a labelled DERIVED span (badge + routing label), not a measured prefill (gap-10 review round 1)', () => {
    const { getByTestId } = renderModel(
      // full content spine but NO wire first byte / no served attempt (the no-TTFB path).
      flow({ ingress_ms: T, routing_decision_ms: T + 50, first_content_delta_ms: T + 400 }),
    );
    const prefill = getByTestId('latency-legend-prefill');
    // Provenance is DERIVED (never measured) and a visible `derived` badge is rendered.
    expect(prefill.getAttribute('data-quality')).toBe('derived');
    expect(getByTestId('latency-derived-prefill').textContent).toBe('derived');
    // The label is the routing→first-token span, NOT the measured "prefill → first token".
    expect(prefill.textContent).toContain('routing');
    expect(prefill.textContent).not.toContain('prefill →');
    // A real (derived) duration renders — not `—`, not a fabricated 0.
    expect(getByTestId('latency-dur-prefill').textContent).toBe('350ms');
    // The segment still gets a bar fill (it IS a known span), with the derived provenance attribute.
    expect(getByTestId('latency-seg-prefill').getAttribute('data-quality')).toBe('derived');
    // Wire TTFB headline is unavailable (no first byte measured) ⇒ `—`.
    expect(getByTestId('latency-ttfb').getAttribute('data-quality')).toBe('unavailable');
  });

  it('labels a DERIVED first-visible-activity TTFT with an `est` badge', () => {
    const { getByTestId } = renderModel(
      flow({ started_ms: T, ingress_ms: T, routing_decision_ms: T + 50 }),
      [{ timestamp_ms: T + 320, kind: 'output', text: 'hi' }],
    );
    const ttft = getByTestId('latency-ttft');
    expect(ttft.getAttribute('data-quality')).toBe('estimated');
    const badge = ttft.querySelector('[data-testid="latency-quality-badge"]');
    expect(badge).toBeTruthy();
    expect(badge?.textContent).toBe('est');
  });

  it('flags a clock-disordered segment with a skew marker (clamped, not negative)', () => {
    const { getByTestId } = renderModel(
      flow({ ingress_ms: T, normalization_done_ms: T + 100, routing_decision_ms: T + 40 }),
    );
    // The routing segment is disordered ⇒ a skew marker + a clamped 0ms duration.
    expect(getByTestId('latency-skew-routing')).toBeTruthy();
    expect(getByTestId('latency-dur-routing').textContent).toBe('0ms');
  });

  it('renders an empty bar (no segments) but the whole block when the spine is absent', () => {
    const { getByTestId, queryByTestId } = renderModel(null);
    expect(getByTestId('latency-breakdown')).toBeTruthy();
    expect(getByTestId('latency-bar')).toBeTruthy();
    // No measured segments ⇒ no fills.
    expect(queryByTestId('latency-seg-queue')).toBeNull();
    // Every headline reads `—`.
    expect(getByTestId('latency-total').textContent).toContain('—');
    expect(getByTestId('latency-rate').textContent).toContain('—');
  });
});
