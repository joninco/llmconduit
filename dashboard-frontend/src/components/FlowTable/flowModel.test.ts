import { describe, it, expect } from 'vitest';
import type { FlowSummary } from '../../api/types';
import {
  statusClass,
  flowCost,
  costDisplay,
  costPerMinDisplay,
  elapsedMs,
  isFailover,
  isMapped,
  shortId,
} from './flowModel';

function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    revision: 1,
    api_call_id: 'api_x',
    method: 'POST',
    uri: '/v1/responses',
    status: 'completed',
    started_ms: 1000,
    cost_confidence: 'unavailable',
    ...over,
  };
}

describe('statusClass — running / 2xx / 4xx / 5xx', () => {
  it('open is running, completed is 2xx', () => {
    expect(statusClass('open')).toBe('running');
    expect(statusClass('completed')).toBe('ok');
  });
  it('reads the HTTP class from the terminal reason when present', () => {
    expect(statusClass('failed', 'upstream 503')).toBe('server-error');
    expect(statusClass('failed', 'rejected 429')).toBe('client-error');
  });
  it('falls back: failed→5xx, cancelled→4xx', () => {
    expect(statusClass('failed')).toBe('server-error');
    expect(statusClass('cancelled')).toBe('client-error');
  });
});

describe('flowCost — persisted terminal value only', () => {
  it('prefers the precomputed flow.cost', () => {
    expect(flowCost(flow({ cost: 0.42 }))).toBe(0.42);
  });
  it('does not reprice from current usage and model when cost is absent', () => {
    const f = flow({ cost: null, model_served: 'llama-3.1-70b', usage: { prompt: 1000, completion: 500, total: 1500, cached: 200, reasoning: 0 } });
    // billable prompt 800 @0.001 + cached 200 @0.0005 + completion 500 @0.002 = 0.0008+0.0001+0.001
    expect(flowCost(f)).toBeNull();
  });
  it('returns null when no roll-up and no usable price', () => {
    expect(flowCost(flow({ cost: null, model_served: 'unknown', usage: { prompt: 1, completion: 1, total: 2, cached: 0, reasoning: 0 } }))).toBeNull();
    expect(flowCost(flow({ cost: null, usage: null }))).toBeNull();
  });
});

// Gap 07 (review round 2): the never-lie-with-zero + label-estimated contract, applied uniformly.
describe('costDisplay — per-flow cost honors cost_confidence', () => {
  it('confident: plain dollars, no est marker', () => {
    expect(costDisplay(0.0061, 'confident')).toEqual({ value: '$0.0061', estimated: false, confidence: 'confident' });
  });
  it('confident: a genuine measured $0 stays $0.00 (distinct from unavailable)', () => {
    expect(costDisplay(0, 'confident')).toEqual({ value: '$0.00', estimated: false, confidence: 'confident' });
  });
  it('estimated: dollars WITH the est flag (must be labelled)', () => {
    expect(costDisplay(0.0019, 'estimated')).toEqual({ value: '$0.0019', estimated: true, confidence: 'estimated' });
  });
  it('unavailable: renders — (a null cost), no est marker', () => {
    expect(costDisplay(null, 'unavailable')).toEqual({ value: '—', estimated: false, confidence: 'unavailable' });
  });
  it('unavailable NEVER lies with zero — a stray 0 alongside an unavailable tag still renders —', () => {
    // Defensive: an unpriced row must not masquerade as a measured $0.00 even if a 0 leaks through.
    expect(costDisplay(0, 'unavailable').value).toBe('—');
  });
});

describe('costPerMinDisplay — $/min honors the priced denominator + confidence', () => {
  const win = (over: Partial<{ cost_per_min: number; priced_samples: number; cost_confidence: 'confident' | 'estimated' | 'unavailable' }>) =>
    ({ cost_per_min: 0, priced_samples: 1, cost_confidence: 'confident' as const, ...over });

  it('null metrics ⇒ unavailable (—/min, no readout yet)', () => {
    expect(costPerMinDisplay(null)).toEqual({ value: '—', estimated: false, confidence: 'unavailable' });
  });
  it('priced_samples 0 ⇒ unavailable (unpriced window, NEVER $0.00/min)', () => {
    expect(costPerMinDisplay(win({ cost_per_min: 0, priced_samples: 0, cost_confidence: 'unavailable' })).value).toBe('—');
  });
  it('confident ⇒ plain $/min, no est marker', () => {
    expect(costPerMinDisplay(win({ cost_per_min: 2.5, cost_confidence: 'confident' }))).toEqual({ value: '$2.50', estimated: false, confidence: 'confident' });
  });
  it('a genuine measured $0/min on a priced window stays $0.00 (distinct from unavailable)', () => {
    expect(costPerMinDisplay(win({ cost_per_min: 0, priced_samples: 3, cost_confidence: 'confident' })).value).toBe('$0.00');
  });
  it('estimated ⇒ $/min WITH the est flag (labelled)', () => {
    expect(costPerMinDisplay(win({ cost_per_min: 0.21, cost_confidence: 'estimated' }))).toEqual({ value: '$0.21', estimated: true, confidence: 'estimated' });
  });
  it('unavailable aggregate ⇒ — even if priced_samples > 0 (no silently-confident zero)', () => {
    expect(costPerMinDisplay(win({ cost_per_min: 0, priced_samples: 2, cost_confidence: 'unavailable' })).value).toBe('—');
  });
});

describe('elapsedMs', () => {
  it('prefers explicit elapsed_ms, then finished-started, then live now-started', () => {
    expect(elapsedMs(flow({ elapsed_ms: 250 }), 9999)).toBe(250);
    expect(elapsedMs(flow({ elapsed_ms: null, finished_ms: 1800, started_ms: 1000 }), 9999)).toBe(800);
    expect(elapsedMs(flow({ status: 'open', elapsed_ms: null, finished_ms: null, started_ms: 1000 }), 3000)).toBe(2000);
  });
});

describe('isFailover / isMapped (U6)', () => {
  it('a single-attempt requested→served divergence is MAPPING, not failover', () => {
    const mapped = flow({ model_requested: 'gpt-4o', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a' });
    expect(isFailover(mapped)).toBe(false);
    expect(isMapped(mapped)).toBe(true);
  });
  it('tags ≥2 recorded dispatch attempts as failover', () => {
    const attempts = [
      { attempt: 1, provider: 'vllm-a', outcome: 'failed' },
      { attempt: 2, provider: 'vllm-b', outcome: 'served' },
    ] as never;
    expect(isFailover(flow({ attempts, model_requested: 'm', model_served: 'm' }))).toBe(true);
  });
  it('a single recorded attempt is not failover', () => {
    const attempts = [{ attempt: 1, provider: 'vllm-a', outcome: 'served' }] as never;
    expect(isFailover(flow({ attempts }))).toBe(false);
  });
  it('tags an explicit failover terminal reason', () => {
    expect(isFailover(flow({ terminal_reason: 'failover to vllm-b' }))).toBe(true);
  });
  it('does not tag or map a same-model served row', () => {
    const same = flow({ model_requested: 'm', model_served: 'm', upstream_target: 'vllm-a' });
    expect(isFailover(same)).toBe(false);
    expect(isMapped(same)).toBe(false);
  });
});

describe('shortId', () => {
  it('keeps short ids and truncates long ones', () => {
    expect(shortId('api_001')).toBe('api_001');
    expect(shortId('0123456789abcdef')).toBe('…89abcdef');
  });
});
