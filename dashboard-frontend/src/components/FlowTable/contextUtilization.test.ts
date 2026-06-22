import { describe, expect, it } from 'vitest';
import {
  CONTEXT_WARN_PCT,
  aggregateContextPressure,
  contextLimitFor,
  contextPromptTokens,
  contextUtilization,
  type ContextLimitMap,
} from './contextUtilization';
import type { Usage } from '../../api/types';

const usage = (over: Partial<Usage> = {}): Usage => ({ prompt: 0, completion: 0, total: 0, ...over });

describe('contextPromptTokens — the window numerator is PROMPT ONLY (spec 09: prompt ÷ max_context)', () => {
  it('is the prompt tokens — completion/total are NOT counted (they do not inflate the window)', () => {
    // prompt 800, completion 200, total 1100 ⇒ numerator is 800 (the input), NOT 1100/1000.
    expect(contextPromptTokens(usage({ prompt: 800, completion: 200, total: 1100 }))).toBe(800);
  });
  it('ignores `total` entirely (even when total is the larger authoritative sum)', () => {
    expect(contextPromptTokens({ prompt: 500, completion: 9999, total: 10499 })).toBe(500);
  });
  it('a finite prompt is used even when completion/total are non-finite (prompt is what matters)', () => {
    expect(contextPromptTokens({ prompt: 500, completion: Number.NaN, total: Number.NaN })).toBe(500);
  });
  it('a measured 0 prompt is a real 0 (NOT null)', () => {
    expect(contextPromptTokens(usage({ prompt: 0, completion: 1000, total: 1000 }))).toBe(0);
  });
  it('non-finite prompt ⇒ null (unreported numerator), regardless of completion/total', () => {
    expect(contextPromptTokens({ prompt: Number.NaN, completion: 200, total: 200 })).toBeNull();
  });
  it('null usage ⇒ null (unreported numerator)', () => {
    expect(contextPromptTokens(null)).toBeNull();
    expect(contextPromptTokens(undefined)).toBeNull();
  });
});

describe('contextUtilization — derived % only with BOTH inputs known (gap 09 oracle)', () => {
  it('known limit + known usage ⇒ a real DERIVED % from PROMPT ONLY, remaining headroom, ok risk', () => {
    // numerator is prompt 6000 (NOT total 8000): 6000 / 32768 ⇒ 18.3%. completion 2000 is IGNORED
    // (it does not occupy the input window) — this is the spec-09 prompt÷max_context semantics.
    const u = contextUtilization(usage({ prompt: 6000, completion: 2000, total: 8000 }), 32768);
    expect(u.quality).toBe('derived');
    expect(u.percentLabel).toBe('18.3%');
    expect(u.fraction).toBeCloseTo(6000 / 32768, 6);
    expect(u.usedTokens).toBe(6000); // the PROMPT, not the 8000 total
    expect(u.contextLimit).toBe(32768);
    expect(u.remainingTokens).toBe(32768 - 6000);
    expect(u.remainingLabel).toBe('26.8k');
    expect(u.risk).toBe('ok');
  });

  it('completions do NOT inflate the % — a huge completion against a small prompt stays low', () => {
    // prompt 1000 / 100000 ⇒ 1.0%; the (buggy) total-based numerator (1000+90000) would read 91.0%.
    const u = contextUtilization(usage({ prompt: 1000, completion: 90000, total: 91000 }), 100000);
    expect(u.percentLabel).toBe('1.0%');
    expect(u.usedTokens).toBe(1000);
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
    // The prompt count is still surfaced (it WAS measured) even though the % is unavailable.
    expect(u.usedTokens).toBe(800); // the PROMPT, not the 1000 total
    expect(u.contextLimit).toBeNull();
  });

  it('UNREPORTED prompt (non-finite) against a known limit ⇒ unavailable, even if total is finite', () => {
    // A usage block with a non-finite prompt has no numerator ⇒ `—`, never a fabricated %.
    const u = contextUtilization({ prompt: Number.NaN, completion: 500, total: 500 }, 32768);
    expect(u.quality).toBe('unavailable');
    expect(u.percentLabel).toBe('—');
    expect(u.usedTokens).toBeNull();
    expect(u.contextLimit).toBe(32768);
    expect(u.risk).toBe('none');
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

  it('peak + near/over are computed from PROMPT only — completions never inflate the aggregate', () => {
    // Each flow carries a fat completion. Under the old (buggy) total-based numerator EVERY flow
    // would read >= 100% (e.g. the 'big' flow's 12800+128000 total ⇒ 110%), corrupting the peak
    // and the near/over counts. Prompt-only keeps them honest: only the prompt occupies the window.
    const agg = aggregateContextPressure(
      [
        { model_served: 'small', usage: usage({ prompt: 900, completion: 5000, total: 5900 }) }, // prompt 90% near
        { model_served: 'big', usage: usage({ prompt: 12800, completion: 128000, total: 140800 }) }, // prompt 10% ok
        { model_served: 'small', usage: usage({ prompt: 1100, completion: 5000, total: 6100 }) }, // prompt 110% over (peak)
      ],
      limits,
    );
    expect(agg.measuredFlows).toBe(3);
    expect(agg.totalFlows).toBe(3);
    expect(agg.peakFraction).toBeCloseTo(1.1, 6); // 1100/1000, NOT 6100/1000
    expect(agg.peakLabel).toBe('110.0%');
    expect(agg.peakRisk).toBe('over');
    expect(agg.nearCount).toBe(2); // the prompt-90% + the prompt-110% (the 'big' flow stays ok)
    expect(agg.overCount).toBe(1); // only the prompt-110%
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
