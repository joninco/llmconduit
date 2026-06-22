import { describe, it, expect } from 'vitest';
import type { MetricWindow } from '../../api/types';
import { CHIP_METRICS, deriveChips, deltaGlyph, ERROR_PCT_THRESHOLD } from './chips';

function win(over: Partial<MetricWindow> = {}): MetricWindow {
  return {
    reqs_per_sec: 4.2, active_streams: 3, error_pct: 1.1,
    p50: 180, p95: 920, p99: 1840, tokens_per_sec: 142, cost_per_min: 0.21,
    samples: 252,
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

  // Gap 01 — don't lie with zeros.
  it('renders sample-derived metrics as UNAVAILABLE (—) when the window has zero samples', () => {
    // A window with traffic in flight but NOTHING finalized: samples 0, but req/s is a
    // genuine measured rate (and active_streams a live count). The numeric fields are 0
    // on the wire (no sample fed them) — they MUST render "—", never "0".
    const chips = deriveChips(win({ samples: 0, error_pct: 0, p50: 0, p95: 0, p99: 0, tokens_per_sec: 0, cost_per_min: 0, reqs_per_sec: 2.5, active_streams: 4 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    // Sample-derived metrics are unavailable (—), NOT a fabricated 0.
    expect(byKey.error_pct).toBe('—');
    expect(byKey.p50).toBe('—');
    expect(byKey.p95).toBe('—');
    expect(byKey.p99).toBe('—');
    expect(byKey.tokens_per_sec).toBe('—');
    expect(byKey.cost_per_min).toBe('—');
    // req/s + active_streams are NOT sample-derived → they show real values.
    expect(byKey.reqs_per_sec).toBe('2.5');
    expect(byKey.active_streams).toBe('4.0');
  });

  it('distinguishes a GENUINE measured zero (samples > 0) from unavailable', () => {
    // Real traffic finalized (samples 12) that genuinely measured 0 latency/cost/err: those
    // are honest zeros and render numerically, NOT "—".
    const chips = deriveChips(win({ samples: 12, error_pct: 0, p50: 0, cost_per_min: 0, reqs_per_sec: 0 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    expect(byKey.error_pct).toBe('0.0');
    expect(byKey.p50).toBe('0');
    expect(byKey.cost_per_min).toBe('0.00');
    expect(byKey.reqs_per_sec).toBe('0.0'); // genuine idle zero, also numeric
  });

  it('an unavailable (zero-sample) window has a FLAT delta and no err% threshold accent', () => {
    const prev = win({ samples: 10, error_pct: 9.9 });
    const cur = win({ samples: 0, error_pct: 0, p50: 0 });
    const chips = deriveChips(cur, prev);
    const err = chips.find((c) => c.key === 'error_pct')!;
    const p50 = chips.find((c) => c.key === 'p50')!;
    expect(err.value).toBe('—');
    expect(err.accent).not.toBe('down'); // an unavailable err% carries no threshold red
    expect(p50.delta).toBe('flat'); // no trend for an unmeasurable value
  });
});
