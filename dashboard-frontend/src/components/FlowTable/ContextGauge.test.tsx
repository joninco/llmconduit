import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { ContextGauge } from './ContextGauge';
import { contextUtilization } from './contextUtilization';
import type { Usage } from '../../api/types';

const usage = (over: Partial<Usage> = {}): Usage => ({ prompt: 0, completion: 0, total: 0, ...over });

describe('ContextGauge — per-flow context-window utilization (gap 09)', () => {
  afterEach(cleanup);

  it('renders a DERIVED % (PROMPT-only) + headroom + a filled track for a known limit + usage', () => {
    // numerator is prompt 6000 (NOT total 8000): 6000/32768 ⇒ 18.3%. completion 2000 is ignored.
    const util = contextUtilization(usage({ prompt: 6000, completion: 2000, total: 8000 }), 32768);
    const { getByTestId } = render(<ContextGauge util={util} />);
    const gauge = getByTestId('context-gauge');
    expect(gauge.getAttribute('data-quality')).toBe('derived');
    expect(getByTestId('context-util-pct').textContent).toBe('18.3%');
    expect(getByTestId('context-headroom').textContent).toContain('26.8k left');
    // The fill is present (a real reading) with a clamped width.
    const fill = getByTestId('context-gauge-fill');
    expect(fill).toBeTruthy();
    expect(fill.style.width).toBe('18.310546875%');
    // No near/over badge at 18%.
    expect(gauge.querySelector('[data-testid="context-risk-badge"]')).toBeNull();
    // Caption shows prompt / capacity.
    expect(getByTestId('context-gauge-caption').textContent).toContain('6.0k / 32.8k');
  });

  it('flags a NEAR-limit flow with an amber `near` badge', () => {
    const util = contextUtilization(usage({ prompt: 900, completion: 0, total: 900 }), 1000);
    const { getByTestId } = render(<ContextGauge util={util} />);
    expect(getByTestId('context-gauge').getAttribute('data-risk')).toBe('near');
    const badge = getByTestId('context-risk-badge');
    expect(badge.textContent).toBe('near');
  });

  it('flags an OVER-budget flow with an `over` badge and a clamped (100%) bar but honest % text', () => {
    const util = contextUtilization(usage({ prompt: 1040, completion: 0, total: 1040 }), 1000);
    const { getByTestId } = render(<ContextGauge util={util} />);
    expect(getByTestId('context-gauge').getAttribute('data-risk')).toBe('over');
    expect(getByTestId('context-risk-badge').textContent).toBe('over');
    // The NUMBER is honest (>100%) …
    expect(getByTestId('context-util-pct').textContent).toBe('104.0%');
    // … but the visual fill cannot exceed the track.
    expect(getByTestId('context-gauge-fill').style.width).toBe('100%');
  });

  it('a GENUINE 0% (measured 0 used / known limit) shows 0.0% (derived), NOT "—"', () => {
    const util = contextUtilization(usage({ prompt: 0, completion: 0, total: 0 }), 32768);
    const { getByTestId } = render(<ContextGauge util={util} />);
    expect(getByTestId('context-gauge').getAttribute('data-quality')).toBe('derived');
    expect(getByTestId('context-util-pct').textContent).toBe('0.0%');
    expect(getByTestId('context-util-pct').textContent).not.toBe('—');
    // A derived 0% still has a fill element (at 0 width) — distinct from the unavailable track.
    expect(getByTestId('context-gauge-fill').style.width).toBe('0%');
  });

  it('UNKNOWN capacity (null limit) ⇒ "—", a dashed empty track, NO fill, NO badge', () => {
    const util = contextUtilization(usage({ prompt: 800, completion: 200, total: 1000 }), null);
    const { getByTestId, queryByTestId } = render(<ContextGauge util={util} />);
    expect(getByTestId('context-gauge').getAttribute('data-quality')).toBe('unavailable');
    expect(getByTestId('context-util-pct').textContent).toBe('—');
    expect(getByTestId('context-util-pct').textContent).not.toBe('0.0%');
    expect(getByTestId('context-headroom').textContent).toBe('—');
    // No fill at all for an unavailable reading (the track is the dashed "no reading" rail).
    expect(queryByTestId('context-gauge-fill')).toBeNull();
    expect(queryByTestId('context-risk-badge')).toBeNull();
    // The capacity half of the caption reads "—" (unknown); the prompt half still shows (it was
    // measured): prompt 800 (NOT the 1000 total) ⇒ "800 / — ctx".
    expect(getByTestId('context-gauge-caption').textContent).toContain('800 / — ctx');
  });

  it('UNREPORTED usage (null) ⇒ "—" even with a known limit (no 0% fabrication)', () => {
    const util = contextUtilization(null, 32768);
    const { getByTestId, queryByTestId } = render(<ContextGauge util={util} />);
    expect(getByTestId('context-gauge').getAttribute('data-quality')).toBe('unavailable');
    expect(getByTestId('context-util-pct').textContent).toBe('—');
    expect(queryByTestId('context-gauge-fill')).toBeNull();
    // Used "—" (unreported), capacity shown (known).
    expect(getByTestId('context-gauge-caption').textContent).toContain('— / 32.8k ctx');
  });
});
