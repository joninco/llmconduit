import { describe, it, expect } from 'vitest';
import {
  topByVolume,
  topByCost,
  tokenMix,
  costConfidenceQuality,
  fmtMixShare,
  TOP_N,
  type LeaderboardDimension,
} from './overviewModel';
import type { FlowSummary } from '../../api/types';

/** A minimal valid `FlowSummary` for the overview roll-up tests; override per-test. */
function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: `api_${Math.random().toString(36).slice(2, 8)}`,
    method: 'POST',
    uri: '/v1/responses',
    status: 'completed',
    started_ms: 1_700_000_000_000,
    cost_confidence: 'unavailable',
    ...over,
  };
}

describe('overviewModel — costConfidenceQuality (the weakest-tag → DQ mapping)', () => {
  it('confident ⇒ derived, estimated ⇒ estimated, unavailable ⇒ unavailable', () => {
    expect(costConfidenceQuality('confident')).toBe('derived');
    expect(costConfidenceQuality('estimated')).toBe('estimated');
    expect(costConfidenceQuality('unavailable')).toBe('unavailable');
  });
});

describe('overviewModel — topByVolume (volume measured, cost weakest-tag)', () => {
  const dim: LeaderboardDimension = 'model';

  it('empty input ⇒ available:false (the empty-state —, not an all-0 board)', () => {
    const lb = topByVolume([], dim);
    expect(lb.available).toBe(false);
    expect(lb.rows).toEqual([]);
    expect(lb.totalFlows).toBe(0);
  });

  it('counts volume per model (served wins) and orders by volume desc', () => {
    const lb = topByVolume(
      [
        flow({ model_served: 'llama', model_requested: 'gpt-4o' }),
        flow({ model_served: 'llama' }),
        flow({ model_served: 'gpt-4o' }),
      ],
      dim,
    );
    expect(lb.available).toBe(true);
    expect(lb.totalFlows).toBe(3);
    expect(lb.rows[0]).toMatchObject({ key: 'llama', volume: 2 });
    expect(lb.rows[1]).toMatchObject({ key: 'gpt-4o', volume: 1 });
  });

  it('a group with NO priced flow ⇒ cost null/unavailable (—), NEVER a fabricated $0.00', () => {
    const lb = topByVolume([flow({ model_served: 'm', cost: null, cost_confidence: 'unavailable' })], dim);
    expect(lb.rows[0]!.cost).toBeNull();
    expect(lb.rows[0]!.costConfidence).toBe('unavailable');
    expect(lb.rows[0]!.costQuality).toBe('unavailable');
  });

  it('a group mixing confident + estimated priced flows inherits the WEAKER (estimated) — never upgraded', () => {
    const lb = topByVolume(
      [
        flow({ model_served: 'm', cost: 0.01, cost_confidence: 'confident' }),
        flow({ model_served: 'm', cost: 0.02, cost_confidence: 'estimated' }),
      ],
      dim,
    );
    const row = lb.rows[0]!;
    expect(row.cost).toBeCloseTo(0.03, 6);
    expect(row.costConfidence).toBe('estimated'); // weakest wins
    expect(row.costQuality).toBe('estimated');
  });

  it('an all-confident priced group reads derived (a real summed cost)', () => {
    const lb = topByVolume(
      [
        flow({ model_served: 'm', cost: 0.01, cost_confidence: 'confident' }),
        flow({ model_served: 'm', cost: 0.02, cost_confidence: 'confident' }),
      ],
      dim,
    );
    expect(lb.rows[0]!.costConfidence).toBe('confident');
    expect(lb.rows[0]!.costQuality).toBe('derived');
  });

  it('an unpriced flow does NOT drag a priced group total to — (only priced flows weaken the tag)', () => {
    const lb = topByVolume(
      [
        flow({ model_served: 'm', cost: 0.05, cost_confidence: 'confident' }),
        flow({ model_served: 'm', cost: null, cost_confidence: 'unavailable' }),
      ],
      dim,
    );
    const row = lb.rows[0]!;
    expect(row.cost).toBeCloseTo(0.05, 6);
    expect(row.pricedFlows).toBe(1);
    expect(row.costConfidence).toBe('confident'); // the unpriced flow didn't weaken it
  });

  it('groups by provider when the dimension is provider, with a — sentinel for an absent target', () => {
    const lb = topByVolume(
      [flow({ upstream_target: 'vllm-a' }), flow({ upstream_target: null })],
      'provider',
    );
    const keys = lb.rows.map((r) => r.key).sort();
    expect(keys).toEqual(['vllm-a', '—']);
  });

  it('caps to TOP_N rows but reports the full groupCount', () => {
    const many = Array.from({ length: TOP_N + 3 }, (_, i) => flow({ model_served: `m${i}` }));
    const lb = topByVolume(many, dim);
    expect(lb.rows.length).toBe(TOP_N);
    expect(lb.groupCount).toBe(TOP_N + 3);
  });
});

describe('overviewModel — topByCost (only priced groups ranked; empty when none priced)', () => {
  const dim: LeaderboardDimension = 'model';

  it('ranks only PRICED groups by cost desc; an unpriced group is excluded (no fabricated $0 rank)', () => {
    const lb = topByCost(
      [
        flow({ model_served: 'cheap', cost: 0.001, cost_confidence: 'confident' }),
        flow({ model_served: 'pricey', cost: 0.5, cost_confidence: 'confident' }),
        flow({ model_served: 'free', cost: null, cost_confidence: 'unavailable' }),
      ],
      dim,
    );
    expect(lb.available).toBe(true);
    expect(lb.rows.map((r) => r.key)).toEqual(['pricey', 'cheap']); // unpriced 'free' excluded
  });

  it('a window with flows but NONE priced ⇒ available:false (the cost-board empty-state —, not $0.00)', () => {
    const lb = topByCost([flow({ model_served: 'm', cost: null, cost_confidence: 'unavailable' })], dim);
    expect(lb.available).toBe(false);
    expect(lb.rows).toEqual([]);
    // totalFlows still reflects the observed population (context for the empty-state message).
    expect(lb.totalFlows).toBe(1);
  });
});

describe('overviewModel — tokenMix (measured classes; unreported optional ⇒ —)', () => {
  it('a window with no usage-bearing flow ⇒ available:false (every class —)', () => {
    const mix = tokenMix([flow({ usage: null })]);
    expect(mix.available).toBe(false);
    expect(mix.usageFlows).toBe(0);
    for (const c of mix.classes) {
      expect(c.tokens).toBeNull();
      expect(c.quality).toBe('unavailable');
    }
  });

  it('sums prompt/completion (measured) + shares over their total', () => {
    const mix = tokenMix([
      flow({ usage: { prompt: 100, completion: 100, total: 200 } }),
      flow({ usage: { prompt: 300, completion: 100, total: 400 } }),
    ]);
    expect(mix.available).toBe(true);
    expect(mix.usageFlows).toBe(2);
    expect(mix.totalTokens).toBe(600);
    const prompt = mix.classes.find((c) => c.key === 'prompt')!;
    expect(prompt.tokens).toBe(400);
    expect(prompt.quality).toBe('measured');
    expect(prompt.fraction).toBeCloseTo(400 / 600, 6);
  });

  it('an UNREPORTED cached/reasoning class across the window ⇒ unavailable (—), NEVER a fabricated 0', () => {
    const mix = tokenMix([flow({ usage: { prompt: 100, completion: 50, total: 150 } })]); // no cached/reasoning
    const cached = mix.classes.find((c) => c.key === 'cached')!;
    expect(cached.tokens).toBeNull();
    expect(cached.quality).toBe('unavailable');
  });

  it('a REPORTED cached 0 is a MEASURED zero (available), distinct from unreported (—)', () => {
    const mix = tokenMix([flow({ usage: { prompt: 100, completion: 50, total: 150, cached: 0, reasoning: 8 } })]);
    const cached = mix.classes.find((c) => c.key === 'cached')!;
    expect(cached.tokens).toBe(0);
    expect(cached.quality).toBe('measured'); // a real measured 0, NOT unavailable
    const reasoning = mix.classes.find((c) => c.key === 'reasoning')!;
    expect(reasoning.tokens).toBe(8);
    expect(reasoning.quality).toBe('measured');
  });

  it('partial reporting: cached reported by ONE flow, absent on another ⇒ measured sum (not dragged to —)', () => {
    const mix = tokenMix([
      flow({ usage: { prompt: 100, completion: 50, total: 150, cached: 40 } }),
      flow({ usage: { prompt: 100, completion: 50, total: 150 } }), // no cached
    ]);
    const cached = mix.classes.find((c) => c.key === 'cached')!;
    expect(cached.tokens).toBe(40); // the one reported flow's value; the absent one contributes nothing
    expect(cached.quality).toBe('measured');
  });

  it('the stacked bar segments are EXCLUSIVE (cached ⊆ prompt, reasoning ⊆ completion) and sum to ≤100%', () => {
    // prompt 1000 (of which 400 cached), completion 600 (of which 200 reasoning). Total 1600.
    const mix = tokenMix([flow({ usage: { prompt: 1000, completion: 600, total: 1600, cached: 400, reasoning: 200 } })]);
    const sum = mix.barSegments.reduce((s, seg) => s + seg.fraction, 0);
    expect(sum).toBeLessThanOrEqual(1 + 1e-9);
    expect(sum).toBeCloseTo(1, 6); // full coverage when total > 0
    const byKey = Object.fromEntries(mix.barSegments.map((s) => [s.key, s.fraction]));
    // EXCLUSIVE carve-outs: non-cached input = 600/1600, cached = 400/1600, non-reasoned output = 400/1600, reasoning = 200/1600.
    expect(byKey['prompt_uncached']).toBeCloseTo(600 / 1600, 6);
    expect(byKey['cached']).toBeCloseTo(400 / 1600, 6);
    expect(byKey['completion_unreasoned']).toBeCloseTo(400 / 1600, 6);
    expect(byKey['reasoning']).toBeCloseTo(200 / 1600, 6);
    // The per-class READOUT shares (over total) are OVERLAPPING annotations — cached's own share is
    // 400/1600 even though it is part of prompt's 1000/1600 (NOT additive — that's why the bar uses segments).
    const cachedClass = mix.classes.find((c) => c.key === 'cached')!;
    expect(cachedClass.fraction).toBeCloseTo(400 / 1600, 6);
  });

  it('unreported cached/reasoning ⇒ the bar is just prompt + completion (carve-outs omitted), still ≤100%', () => {
    const mix = tokenMix([flow({ usage: { prompt: 100, completion: 100, total: 200 } })]);
    const keys = mix.barSegments.map((s) => s.key).sort();
    expect(keys).toEqual(['completion_unreasoned', 'prompt_uncached']); // no cached/reasoning segments
    expect(mix.barSegments.reduce((s, seg) => s + seg.fraction, 0)).toBeCloseTo(1, 6);
  });

  it('a malformed cached > prompt is clamped so the prompt_uncached segment never goes negative', () => {
    const mix = tokenMix([flow({ usage: { prompt: 100, completion: 100, total: 200, cached: 500 } })]);
    const promptUncached = mix.barSegments.find((s) => s.key === 'prompt_uncached');
    // cached clamps to prompt (100) ⇒ prompt_uncached is 0 (omitted), cached segment is 100/200.
    expect(promptUncached).toBeUndefined();
    expect(mix.barSegments.find((s) => s.key === 'cached')!.fraction).toBeCloseTo(100 / 200, 6);
    expect(mix.barSegments.reduce((s, seg) => s + seg.fraction, 0)).toBeLessThanOrEqual(1 + 1e-9);
  });

  it('a malformed subset (cached > prompt / reasoning > completion) ⇒ the per-class READOUT shares are ≤100% (review R2 MEDIUM), consistent with the clamped bar', () => {
    // cached 500 > prompt 100, reasoning 400 > completion 100. Raw shares would be 500/200=250% and
    // 400/200=200%; the readout must instead use the parent-clamped amounts ⇒ ≤100% each.
    const mix = tokenMix([flow({ usage: { prompt: 100, completion: 100, total: 200, cached: 500, reasoning: 400 } })]);
    const cached = mix.classes.find((c) => c.key === 'cached')!;
    const reasoning = mix.classes.find((c) => c.key === 'reasoning')!;
    // The displayed token COUNT stays the raw measured value (honest), but the SHARE is clamped.
    expect(cached.tokens).toBe(500); // raw count preserved
    expect(cached.quality).toBe('measured');
    expect(cached.fraction!).toBeLessThanOrEqual(1 + 1e-9);
    expect(cached.fraction!).toBeCloseTo(100 / 200, 6); // clamped to prompt (100) / total (200)
    expect(reasoning.tokens).toBe(400);
    expect(reasoning.fraction!).toBeLessThanOrEqual(1 + 1e-9);
    expect(reasoning.fraction!).toBeCloseTo(100 / 200, 6); // clamped to completion (100) / total (200)
    // prompt/completion own shares are unaffected (each 100/200).
    expect(mix.classes.find((c) => c.key === 'prompt')!.fraction).toBeCloseTo(100 / 200, 6);
  });
});

describe('overviewModel — fmtMixShare', () => {
  it('renders a percent, — when unavailable', () => {
    expect(fmtMixShare(0.62)).toBe('62%');
    expect(fmtMixShare(0)).toBe('0%');
    expect(fmtMixShare(null)).toBe('—');
  });
});
