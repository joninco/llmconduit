import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { ContextPressure } from './ContextPressure';
import { makeFlow } from '../testHarness';
import type { ContextLimitMap } from './contextUtilization';

const LIMITS: ContextLimitMap = { small: 1000, big: 128000, mystery: null };

describe('ContextPressure — aggregate context-window pressure stat (gap 09)', () => {
  afterEach(cleanup);

  it('shows the PEAK utilization (derived) + near/over counts across the flow set', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'small', usage: { prompt: 900, completion: 0, total: 900 } }), // 90% near
      makeFlow({ api_call_id: 'b', model_served: 'small', usage: { prompt: 1100, completion: 0, total: 1100 } }), // 110% over (peak)
      makeFlow({ api_call_id: 'c', model_served: 'big', usage: { prompt: 12800, completion: 0, total: 12800 } }), // 10% ok
    ];
    const { getByTestId } = render(<ContextPressure rows={rows} limits={LIMITS} />);
    const peak = getByTestId('context-pressure-peak');
    expect(peak.textContent).toBe('110.0%');
    expect(peak.getAttribute('data-quality')).toBe('derived');
    expect(peak.getAttribute('data-risk')).toBe('over');
    expect(getByTestId('context-pressure-near').textContent).toBe('2');
    expect(getByTestId('context-pressure-over').textContent).toBe('1');
    expect(getByTestId('context-pressure-coverage').textContent).toBe('3/3 measured');
  });

  it('EXCLUDES unmeasurable flows (unknown limit / unreported usage) from coverage + figures', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'mystery', usage: { prompt: 5000, completion: 0, total: 5000 } }), // unknown limit
      makeFlow({ api_call_id: 'b', model_served: 'small', usage: null }), // unreported usage
      makeFlow({ api_call_id: 'c', model_served: 'small', usage: { prompt: 500, completion: 0, total: 500 } }), // 50% measurable
    ];
    const { getByTestId } = render(<ContextPressure rows={rows} limits={LIMITS} />);
    expect(getByTestId('context-pressure-peak').textContent).toBe('50.0%');
    expect(getByTestId('context-pressure-coverage').textContent).toBe('1/3 measured');
    expect(getByTestId('context-pressure-near').textContent).toBe('0');
    expect(getByTestId('context-pressure-over').textContent).toBe('0');
  });

  it('a set with NO measurable flow ⇒ peak "—" (unavailable), never a fabricated 0%', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'mystery', usage: { prompt: 5000, completion: 0, total: 5000 } }),
      makeFlow({ api_call_id: 'b', model_served: 'small', usage: null }),
    ];
    const { getByTestId } = render(<ContextPressure rows={rows} limits={LIMITS} />);
    const peak = getByTestId('context-pressure-peak');
    expect(peak.textContent).toBe('—');
    expect(peak.textContent).not.toBe('0.0%');
    expect(peak.getAttribute('data-quality')).toBe('unavailable');
    expect(getByTestId('context-pressure-coverage').textContent).toBe('0/2 measured');
  });

  it('an empty flow set ⇒ "—" peak, 0/0 measured', () => {
    const { getByTestId } = render(<ContextPressure rows={[]} limits={LIMITS} />);
    expect(getByTestId('context-pressure-peak').textContent).toBe('—');
    expect(getByTestId('context-pressure-coverage').textContent).toBe('0/0 measured');
  });
});
