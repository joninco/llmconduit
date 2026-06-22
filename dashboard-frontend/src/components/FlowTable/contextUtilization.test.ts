import { describe, expect, it } from 'vitest';
import {
  CONTEXT_WARN_PCT,
  aggregateContextPressure,
  contextLimitFor,
  contextUsedTokens,
  contextUtilization,
  type ContextLimitMap,
} from './contextUtilization';
import type { Usage } from '../../api/types';

const usage = (over: Partial<Usage> = {}): Usage => ({ prompt: 0, completion: 0, total: 0, ...over });

describe('contextUsedTokens — the window numerator (don\'t-lie-with-zeros)', () => {
  it('prefers the reported total (authoritative sum)', () => {
    expect(contextUsedTokens(usage({ prompt: 800, completion: 200, total: 1100 }))).toBe(1100);
  });
  it('falls back to prompt+completion when total is non-finite', () => {
    expect(contextUsedTokens({ prompt: 800, completion: 200, total: Number.NaN })).toBe(1000);
  });
  it('uses a partial (prompt-only) block as a usable lower bound', () => {
    expect(contextUsedTokens({ prompt: 500, completion: Number.NaN, total: Number.NaN })).toBe(500);
  });
  it('null usage ⇒ null (unreported numerator)', () => {
    expect(contextUsedTokens(null)).toBeNull();
    expect(contextUsedTokens(undefined)).toBeNull();
  });
});

describe('contextUtilization — derived % only with BOTH inputs known (gap 09 oracle)', () => {
  it('known limit + known usage ⇒ a real DERIVED %, remaining headroom, ok risk', () => {
    // 8000 used / 32768 ⇒ 24.4%
    const u = contextUtilization(usage({ prompt: 6000, completion: 2000, total: 8000 }), 32768);
    expect(u.quality).toBe('derived');
    expect(u.percentLabel).toBe('24.4%');
    expect(u.fraction).toBeCloseTo(8000 / 32768, 6);
    expect(u.usedTokens).toBe(8000);
    expect(u.contextLimit).toBe(32768);
    expect(u.remainingTokens).toBe(32768 - 8000);
    expect(u.remainingLabel).toBe('24.8k');
    expect(u.risk).toBe('ok');
  });

  it('a NEAR-limit utilization (>= warn threshold, < 100) is flagged `near`', () => {
    // 90% of 1000 = 900 used ⇒ 90.0% ≥ CONTEXT_WARN_PCT(85) and < 100 ⇒ near
    const u = contextUtilization(usage({ prompt: 900, completion: 0, total: 900 }), 1000);
    expect(u.quality).toBe('derived');
    expect(u.percentLabel).toBe('90.0%');
    expect(u.risk).toBe('near');
    expect(CONTEXT_WARN_PCT).toBe(85);
  });

  it('an at/over-budget utilization is flagged `over` and the % honestly exceeds 100', () => {
    // 1040 used / 1000 ⇒ 104.0% (NOT clamped to 100 — the overflow is honestly visible)
    const u = contextUtilization(usage({ prompt: 1040, completion: 0, total: 1040 }), 1000);
    expect(u.risk).toBe('over');
    expect(u.percentLabel).toBe('104.0%');
    expect(u.fraction).toBeGreaterThan(1);
    // No NEGATIVE headroom — it floors at 0.
    expect(u.remainingTokens).toBe(0);
  });

  it('exactly 100% (used == limit) is `over` (at the ceiling)', () => {
    const u = contextUtilization(usage({ prompt: 1000, completion: 0, total: 1000 }), 1000);
    expect(u.percentLabel).toBe('100.0%');
    expect(u.risk).toBe('over');
    expect(u.remainingTokens).toBe(0);
  });

  it('a GENUINE 0% (measured 0 used / known limit) is DERIVED, distinct from unavailable', () => {
    const u = contextUtilization(usage({ prompt: 0, completion: 0, total: 0 }), 32768);
    expect(u.quality).toBe('derived'); // NOT unavailable
    expect(u.percentLabel).toBe('0.0%'); // NOT "—"
    expect(u.percentLabel).not.toBe('—');
    expect(u.risk).toBe('ok');
    expect(u.remainingTokens).toBe(32768);
  });

  it('UNKNOWN capacity (context_limit null) ⇒ unavailable (— ), never 0%/100%', () => {
    const u = contextUtilization(usage({ prompt: 800, completion: 200, total: 1000 }), null);
    expect(u.quality).toBe('unavailable');
    expect(u.percentLabel).toBe('—');
    expect(u.percentLabel).not.toBe('0.0%');
    expect(u.percentLabel).not.toBe('100.0%');
    expect(u.fraction).toBeNull();
    expect(u.remainingLabel).toBe('—');
    expect(u.risk).toBe('none');
    // The used count is still surfaced (it WAS measured) even though the % is unavailable.
    expect(u.usedTokens).toBe(1000);
    expect(u.contextLimit).toBeNull();
  });

  it('a 0 or negative context_limit is treated as UNKNOWN capacity ⇒ unavailable (no /0)', () => {
    expect(contextUtilization(usage({ total: 1000, prompt: 1000 }), 0).quality).toBe('unavailable');
    expect(contextUtilization(usage({ total: 1000, prompt: 1000 }), -5).quality).toBe('unavailable');
    // Never a divide-by-zero Infinity/NaN leaking into the percent.
    expect(contextUtilization(usage({ total: 1000, prompt: 1000 }), 0).percentLabel).toBe('—');
  });

  it('UNREPORTED usage (null) against a known limit ⇒ unavailable (— ), never 0%', () => {
    const u = contextUtilization(null, 32768);
    expect(u.quality).toBe('unavailable');
    expect(u.percentLabel).toBe('—');
    expect(u.percentLabel).not.toBe('0.0%');
    expect(u.usedTokens).toBeNull();
    expect(u.contextLimit).toBe(32768);
    expect(u.risk).toBe('none');
  });
});

describe('contextLimitFor — served-then-requested model resolution', () => {
  const limits: ContextLimitMap = { 'gpt-4o': 128000, mystery: null };
  it('resolves the served model first', () => {
    expect(contextLimitFor('gpt-4o', 'llama', limits)).toBe(128000);
  });
  it('falls back to the requested model when served is absent', () => {
    expect(contextLimitFor(null, 'gpt-4o', limits)).toBe(128000);
  });
  it('a known-but-null window stays null (unknown), distinct from a real integer', () => {
    expect(contextLimitFor('mystery', null, limits)).toBeNull();
  });
  it('an unknown model id ⇒ null (absent ≡ unknown, never 0)', () => {
    expect(contextLimitFor('not-in-catalog', null, limits)).toBeNull();
  });
  it('no model at all ⇒ null', () => {
    expect(contextLimitFor(null, null, limits)).toBeNull();
  });
});

describe('aggregateContextPressure — peak + near/over over a flow set (gap 09 aggregate)', () => {
  const limits: ContextLimitMap = { small: 1000, big: 128000, unknown: null };

  it('peak is the MAX derived utilization; near/over count only measured flows', () => {
    const agg = aggregateContextPressure(
      [
        { model_served: 'small', usage: usage({ prompt: 900, completion: 0, total: 900 }) }, // 90% near
        { model_served: 'big', usage: usage({ prompt: 12800, completion: 0, total: 12800 }) }, // 10% ok
        { model_served: 'small', usage: usage({ prompt: 1100, completion: 0, total: 1100 }) }, // 110% over (peak)
      ],
      limits,
    );
    expect(agg.measuredFlows).toBe(3);
    expect(agg.totalFlows).toBe(3);
    expect(agg.peakFraction).toBeCloseTo(1.1, 6);
    expect(agg.peakLabel).toBe('110.0%');
    expect(agg.peakRisk).toBe('over');
    expect(agg.nearCount).toBe(2); // the 90% + the 110%
    expect(agg.overCount).toBe(1); // the 110%
  });

  it('EXCLUDES unmeasurable flows (unknown limit / unreported usage) from the figures', () => {
    const agg = aggregateContextPressure(
      [
        { model_served: 'unknown', usage: usage({ prompt: 5000, completion: 0, total: 5000 }) }, // unknown limit
        { model_served: 'small', usage: null }, // unreported usage
        { model_served: 'small', usage: usage({ prompt: 500, completion: 0, total: 500 }) }, // 50% measurable
      ],
      limits,
    );
    expect(agg.totalFlows).toBe(3);
    expect(agg.measuredFlows).toBe(1); // only the last
    expect(agg.peakLabel).toBe('50.0%');
    expect(agg.peakRisk).toBe('ok');
    expect(agg.nearCount).toBe(0);
    expect(agg.overCount).toBe(0);
  });

  it('a set with NO measurable flow ⇒ peak "—" (unavailable), never a fabricated 0%', () => {
    const agg = aggregateContextPressure(
      [
        { model_served: 'unknown', usage: usage({ total: 5000, prompt: 5000 }) },
        { model_served: 'small', usage: null },
      ],
      limits,
    );
    expect(agg.measuredFlows).toBe(0);
    expect(agg.peakFraction).toBeNull();
    expect(agg.peakLabel).toBe('—');
    expect(agg.peakLabel).not.toBe('0.0%');
    expect(agg.peakRisk).toBe('none');
  });

  it('an empty set ⇒ unavailable peak, 0/0 measured', () => {
    const agg = aggregateContextPressure([], limits);
    expect(agg.peakLabel).toBe('—');
    expect(agg.measuredFlows).toBe(0);
    expect(agg.totalFlows).toBe(0);
  });
});
