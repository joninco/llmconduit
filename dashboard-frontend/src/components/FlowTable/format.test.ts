import { describe, expect, it } from 'vitest';
import { fmtClock, fmtCost, fmtCostRate, fmtElapsed, fmtModelPair, fmtPercent, fmtRate, fmtSamples, fmtTokens, fmtTokensPerSec } from './format';

describe('format helpers — null ⇒ "—" (don\'t-lie-with-zeros)', () => {
  it('fmtElapsed: null/non-finite ⇒ "—", else compact', () => {
    expect(fmtElapsed(null)).toBe('—');
    expect(fmtElapsed(Number.NaN)).toBe('—');
    expect(fmtElapsed(820)).toBe('820 ms');
    expect(fmtElapsed(4200)).toBe('4.2 s');
    expect(fmtElapsed(62000)).toBe('1 m 02 s');
  });

  it('fmtCost: null ⇒ "—", real 0 ⇒ "$0.00" (a measured zero is NOT unavailable)', () => {
    expect(fmtCost(null)).toBe('—');
    expect(fmtCost(0)).toBe('$0.00');
    expect(fmtCost(0.0061)).toBe('$0.0061');
  });

  it('fmtModelPair: elides identical, falls back to "—" when both absent', () => {
    expect(fmtModelPair('a', 'a')).toBe('a');
    expect(fmtModelPair('a', 'b')).toBe('a → b');
    expect(fmtModelPair(null, null)).toBe('—');
  });

  it('fmtClock: HH:MM:SS.mmm', () => {
    expect(fmtClock(new Date(2026, 0, 1, 9, 8, 7, 6).getTime())).toBe('09:08:07.006');
  });
});

describe('fmtTokens — the catalog context_limit renderer (gap 06)', () => {
  // `CatalogEntry.context_limit` is `number | null` (nullable end-to-end). The UI
  // MUST render `—` (unavailable) on a missing window, NEVER `0` — a `0` ceiling
  // reads as garbage/infinite utilization in the gap-09 gauge.
  it('null/undefined context_limit ⇒ "—" (unavailable), never "0"', () => {
    expect(fmtTokens(null)).toBe('—');
    expect(fmtTokens(undefined)).toBe('—');
    expect(fmtTokens(Number.NaN)).toBe('—');
    // The invariant the spec calls out explicitly:
    expect(fmtTokens(null)).not.toBe('0');
  });

  it('a real measured context_limit ⇒ the formatted number (NOT "—")', () => {
    // A genuine measured `0` is a number, not unavailable — distinct from null.
    expect(fmtTokens(0)).toBe('0');
    expect(fmtTokens(812)).toBe('812');
    expect(fmtTokens(32768)).toBe('32.8k'); // qwen2.5-coder-32b
    expect(fmtTokens(128000)).toBe('128k'); // gpt-4o
    expect(fmtTokens(131072)).toBe('131.1k'); // llama-3.1-70b
    expect(fmtTokens(2_500_000)).toBe('2.5m');
  });
});

describe('fmtTokensPerSec — the gap-10 stream throughput renderer', () => {
  it('null/non-finite ⇒ "—" (unavailable), never "0"', () => {
    expect(fmtTokensPerSec(null)).toBe('—');
    expect(fmtTokensPerSec(Number.NaN)).toBe('—');
    expect(fmtTokensPerSec(null)).not.toBe('0 tok/s');
  });

  it('a real measured throughput ⇒ a compact tok/s string', () => {
    expect(fmtTokensPerSec(0)).toBe('0 tok/s'); // a measured zero is distinct from unavailable
    expect(fmtTokensPerSec(4.2)).toBe('4.2 tok/s');
    expect(fmtTokensPerSec(142)).toBe('142 tok/s');
    expect(fmtTokensPerSec(1500)).toBe('1.5k tok/s');
  });
});

describe('shared operational formatters', () => {
  it('removes float noise while retaining useful precision', () => {
    expect(fmtRate(2.50000001)).toBe('2.5 req/s');
    expect(fmtPercent(1.666666)).toBe('1.7%');
    expect(fmtSamples(1)).toBe('1 sample');
    expect(fmtSamples(12, 'request')).toBe('12 requests');
    expect(fmtCostRate(0.006123)).toBe('$0.0061/min');
  });

  it('renders invalid and negative measurements as unavailable', () => {
    expect(fmtRate(Number.POSITIVE_INFINITY)).toBe('—');
    expect(fmtElapsed(-1)).toBe('—');
    expect(fmtPercent(-0.1)).toBe('—');
    expect(fmtCostRate(Number.NaN)).toBe('—');
  });
});
