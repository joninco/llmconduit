import { describe, expect, it } from 'vitest';
import type { FlowSummary, ModelPrice, Usage } from '../../api/types';
import { aggregateCacheByKey, tokenEconomics } from './tokenEconomics';

/** gpt-4o: a CONFIGURED cached rate (presence true) — licenses a "$ saved" figure. */
const PRICED_CACHE: ModelPrice = { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025, cached_price_configured: true };
/** llama: NO configured cached rate (presence false; numeric defaults to 0) — NO "$ saved". */
const PRICED_NO_CACHE: ModelPrice = { input_per_1k: 0.0008, output_per_1k: 0.0008, cached_per_1k: 0, cached_price_configured: false };

const PRICE_TABLE: Record<string, ModelPrice> = {
  'gpt-4o': PRICED_CACHE,
  'llama-3.1-70b': PRICED_NO_CACHE,
};

function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: 'api_x',
    method: 'POST',
    uri: '/v1/responses',
    status: 'completed',
    started_ms: 1_700_000_000_000,
    cost_confidence: 'confident',
    model_served: 'gpt-4o',
    ...over,
  };
}
function usage(over: Partial<Usage> = {}): Usage {
  return { prompt: 1000, completion: 200, total: 1200, ...over };
}

describe('tokenEconomics — per-flow split / cache-hit / $ saved (gap 08)', () => {
  it('a MEASURED split: cached/reasoning reported ⇒ measured counts, derived hit + $ saved', () => {
    const econ = tokenEconomics(flow({ usage: usage({ cached: 250, reasoning: 64 }) }), PRICE_TABLE);
    expect(econ.cached).toEqual({ value: '250', quality: 'measured' });
    expect(econ.reasoning).toEqual({ value: '64', quality: 'measured' });
    // 250 / 1000 = 25.0%
    expect(econ.cacheHit).toEqual({ value: '25.0%', quality: 'derived' });
    // saved = (250/1000) * (0.005 - 0.0025) = 0.000625 → fmtCost 4dp under a cent
    expect(econ.saved.quality).toBe('derived');
    expect(econ.saved.value).toBe('$0.0006');
    expect(econ.cachedPriceConfigured).toBe(true);
  });

  it('UNREPORTED cached/reasoning ⇒ "—" (unavailable), NEVER "0"; hit + $ saved unavailable', () => {
    // gap-07 contract: absent cached/reasoning is unreported, distinct from a reported 0.
    const econ = tokenEconomics(flow({ usage: usage() }), PRICE_TABLE);
    expect(econ.cached).toEqual({ value: '—', quality: 'unavailable' });
    expect(econ.reasoning).toEqual({ value: '—', quality: 'unavailable' });
    expect(econ.cached.value).not.toBe('0');
    // A hit rate cannot be claimed for an unreported class — NOT a 0% miss.
    expect(econ.cacheHit).toEqual({ value: '—', quality: 'unavailable' });
    // No reported cached ⇒ no saving to claim, even though gpt-4o HAS a configured cache price.
    expect(econ.saved).toEqual({ value: '—', quality: 'unavailable' });
  });

  it('a REPORTED cached 0 ⇒ a measured "0" (a real miss): 0% hit + $0.00 saved, distinct from "—"', () => {
    const econ = tokenEconomics(flow({ usage: usage({ cached: 0, reasoning: 0 }) }), PRICE_TABLE);
    // Measured zero reads "0", NOT "—".
    expect(econ.cached).toEqual({ value: '0', quality: 'measured' });
    expect(econ.cached.value).not.toBe('—');
    expect(econ.reasoning).toEqual({ value: '0', quality: 'measured' });
    // A genuine 0% hit (real miss) — derived, distinct from unavailable.
    expect(econ.cacheHit).toEqual({ value: '0.0%', quality: 'derived' });
    // A measured $0.00 saving (the miss saved nothing) — derived, distinct from "—".
    expect(econ.saved).toEqual({ value: '$0.00', quality: 'derived' });
    expect(econ.saved.value).not.toBe('—');
  });

  it('cached reported but NO configured cached price ⇒ split shows, "$ saved" is "—" (no fabrication)', () => {
    // llama has cached tokens but cached_price_configured=false → the numeric 0.0 must NOT be used.
    const econ = tokenEconomics(flow({ model_served: 'llama-3.1-70b', cost_confidence: 'estimated', usage: usage({ cached: 300 }) }), PRICE_TABLE);
    expect(econ.cached).toEqual({ value: '300', quality: 'measured' }); // split still shows
    expect(econ.cacheHit.quality).toBe('derived'); // hit rate still derivable from counts
    // The presence gate: no configured cached price ⇒ NO dollar figure.
    expect(econ.saved).toEqual({ value: '—', quality: 'unavailable' });
    expect(econ.cachedPriceConfigured).toBe(false);
  });

  it('no usage at all ⇒ every figure "—" (unavailable), never "0"', () => {
    const econ = tokenEconomics(flow({ usage: null }), PRICE_TABLE);
    for (const v of [econ.prompt, econ.completion, econ.cached, econ.reasoning, econ.cacheHit, econ.saved]) {
      expect(v).toEqual({ value: '—', quality: 'unavailable' });
    }
  });

  it('cached reported but prompt is 0 ⇒ hit rate "—" (undefined ratio), never a fabricated %', () => {
    const econ = tokenEconomics(flow({ usage: { prompt: 0, completion: 10, total: 10, cached: 0 } }), PRICE_TABLE);
    expect(econ.cacheHit).toEqual({ value: '—', quality: 'unavailable' });
  });

  it('a configured cached rate ABOVE the input rate clamps the saving at $0.00 (never negative)', () => {
    const inverted: ModelPrice = { input_per_1k: 0.001, output_per_1k: 0.002, cached_per_1k: 0.009, cached_price_configured: true };
    const econ = tokenEconomics(flow({ model_served: 'weird', usage: usage({ cached: 500 }) }), { weird: inverted });
    expect(econ.saved.value).toBe('$0.00');
    expect(econ.saved.quality).toBe('derived');
  });
});

describe('aggregateCacheByKey — by-model roll-up (gap 08)', () => {
  it('rolls up the hit rate over flows that REPORTED cached; excludes unreported flows', () => {
    const rows = [
      flow({ api_call_id: 'a', model_served: 'gpt-4o', usage: usage({ prompt: 1000, cached: 200 }) }),
      flow({ api_call_id: 'b', model_served: 'gpt-4o', usage: usage({ prompt: 1000, cached: 400 }) }),
      // unreported cached — must NOT drag the rate toward 0%; excluded entirely.
      flow({ api_call_id: 'c', model_served: 'gpt-4o', usage: usage({ prompt: 9999 }) }),
    ];
    const [agg] = aggregateCacheByKey(rows, (f) => f.model_served, PRICE_TABLE);
    expect(agg!.key).toBe('gpt-4o');
    // (200+400) / (1000+1000) = 30.0% — the unreported flow's 9999 prompt is excluded.
    expect(agg!.hitRate).toEqual({ value: '30.0%', quality: 'derived' });
    expect(agg!.reportedSamples).toBe(2);
    expect(agg!.totalSamples).toBe(3);
    // saved = (600/1000) * (0.005 - 0.0025) = 0.0015
    expect(agg!.saved.quality).toBe('derived');
    expect(agg!.saved.value).toBe('$0.0015');
    // all contributing flows are confident ⇒ not estimated.
    expect(agg!.estimated).toBe(false);
  });

  it('a model whose flows NEVER reported cached ⇒ hit rate "—" (unavailable), not a fabricated 0%', () => {
    const rows = [
      flow({ api_call_id: 'a', model_served: 'gpt-4o', usage: usage() }),
      flow({ api_call_id: 'b', model_served: 'gpt-4o', usage: usage() }),
    ];
    const [agg] = aggregateCacheByKey(rows, (f) => f.model_served, PRICE_TABLE);
    expect(agg!.hitRate).toEqual({ value: '—', quality: 'unavailable' });
    expect(agg!.hitRate.value).not.toBe('0.0%');
    expect(agg!.reportedSamples).toBe(0);
    expect(agg!.saved.quality).toBe('unavailable');
  });

  it('a non-confident contributing flow taints the group ⇒ estimated (labelled)', () => {
    const rows = [
      flow({ api_call_id: 'a', model_served: 'gpt-4o', cost_confidence: 'confident', usage: usage({ cached: 100 }) }),
      flow({ api_call_id: 'b', model_served: 'gpt-4o', cost_confidence: 'estimated', usage: usage({ cached: 100 }) }),
    ];
    const [agg] = aggregateCacheByKey(rows, (f) => f.model_served, PRICE_TABLE);
    expect(agg!.estimated).toBe(true);
  });

  it('a model with cached but NO configured cache price ⇒ hit rate shown, "$ saved" "—"', () => {
    const rows = [flow({ api_call_id: 'a', model_served: 'llama-3.1-70b', cost_confidence: 'estimated', usage: usage({ cached: 250 }) })];
    const [agg] = aggregateCacheByKey(rows, (f) => f.model_served, PRICE_TABLE);
    expect(agg!.hitRate.quality).toBe('derived');
    expect(agg!.saved).toEqual({ value: '—', quality: 'unavailable' });
  });

  it('groups with no key are dropped; rows sort by reported cache volume (busiest first)', () => {
    const rows = [
      flow({ api_call_id: 'a', model_served: 'gpt-4o', usage: usage({ cached: 100 }) }),
      flow({ api_call_id: 'b', model_served: 'llama-3.1-70b', cost_confidence: 'estimated', usage: usage({ cached: 900 }) }),
      flow({ api_call_id: 'c', model_served: null, model_requested: null, usage: usage({ cached: 5 }) }),
    ];
    const aggs = aggregateCacheByKey(rows, (f) => f.model_served ?? f.model_requested, PRICE_TABLE);
    expect(aggs).toHaveLength(2); // the null-model row is dropped
    expect(aggs[0]!.key).toBe('llama-3.1-70b'); // 900 cached tokens — busiest first
    expect(aggs[1]!.key).toBe('gpt-4o');
  });
});
