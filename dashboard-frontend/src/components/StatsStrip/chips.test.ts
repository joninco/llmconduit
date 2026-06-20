import { describe, it, expect } from 'vitest';
import type { MetricWindow } from '../../api/types';
import { CHIP_METRICS, deriveChips, deltaGlyph, ERROR_PCT_THRESHOLD } from './chips';

function win(over: Partial<MetricWindow> = {}): MetricWindow {
  return {
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    ...over,
  };
}

describe('chips', () => {
  it('emits one descriptor per chip metric in strip order', () => {
    const chips = deriveChips(win(), null);
    expect(chips.map((c) => c.key)).toEqual([...CHIP_METRICS]);
  });

  it('formats values (rate/ms/pct/tokens/money)', () => {
    const chips = deriveChips(win({ reqs_per_sec: 4.2, p95: 920, error_pct: 1.1, tokens_per_sec: 1500, cost_per_min: 0.21 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    expect(byKey.reqs_per_sec).toBe('4.2');
    expect(byKey.p95).toBe('920');
    expect(byKey.error_pct).toBe('1.1');
    expect(byKey.tokens_per_sec).toBe('1.5k'); // fmtTokens compaction
    expect(byKey.cost_per_min).toBe('0.21');
  });

  it('renders "—" for every chip when there is no sample', () => {
    const chips = deriveChips(null, null);
    expect(chips.every((c) => c.value === '—')).toBe(true);
    expect(chips.every((c) => c.delta === 'flat')).toBe(true);
  });

  it('turns the err% chip red ONLY above the threshold', () => {
    const below = deriveChips(win({ error_pct: ERROR_PCT_THRESHOLD - 0.1 }), null).find((c) => c.key === 'error_pct')!;
    const above = deriveChips(win({ error_pct: ERROR_PCT_THRESHOLD + 0.1 }), null).find((c) => c.key === 'error_pct')!;
    expect(below.accent).not.toBe('down');
    expect(above.accent).toBe('down');
  });

  it('computes the delta direction vs. the previous sample', () => {
    const up = deriveChips(win({ reqs_per_sec: 5 }), win({ reqs_per_sec: 4 })).find((c) => c.key === 'reqs_per_sec')!;
    const down = deriveChips(win({ reqs_per_sec: 3 }), win({ reqs_per_sec: 4 })).find((c) => c.key === 'reqs_per_sec')!;
    const flat = deriveChips(win({ reqs_per_sec: 4 }), win({ reqs_per_sec: 4 })).find((c) => c.key === 'reqs_per_sec')!;
    expect(up.delta).toBe('up');
    expect(down.delta).toBe('down');
    expect(flat.delta).toBe('flat');
  });

  it('deltaGlyph maps direction → arrow', () => {
    expect(deltaGlyph('up')).toBe('▲');
    expect(deltaGlyph('down')).toBe('▼');
    expect(deltaGlyph('flat')).toBe('·');
  });
});
