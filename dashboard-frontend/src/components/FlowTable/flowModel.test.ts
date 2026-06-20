import { describe, it, expect } from 'vitest';
import type { FlowSummary, ModelPrice } from '../../api/types';
import { statusClass, flowCost, computeCost, elapsedMs, isFailover, shortId } from './flowModel';

function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: 'api_x',
    method: 'POST',
    uri: '/v1/responses',
    status: 'completed',
    started_ms: 1000,
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

describe('flowCost — server roll-up preferred, else usage × price', () => {
  const price: Record<string, ModelPrice> = {
    'llama-3.1-70b': { input_per_1k: 0.001, output_per_1k: 0.002, cached_per_1k: 0.0005 },
  };
  it('prefers the precomputed flow.cost', () => {
    expect(flowCost(flow({ cost: 0.42 }), price)).toBe(0.42);
  });
  it('computes from usage × served-model price when cost is absent', () => {
    const f = flow({ cost: null, model_served: 'llama-3.1-70b', usage: { prompt: 1000, completion: 500, total: 1500, cached: 200, reasoning: 0 } });
    // billable prompt 800 @0.001 + cached 200 @0.0005 + completion 500 @0.002 = 0.0008+0.0001+0.001
    expect(flowCost(f, price)).toBeCloseTo(0.0019, 6);
  });
  it('returns null when no roll-up and no usable price', () => {
    expect(flowCost(flow({ cost: null, model_served: 'unknown', usage: { prompt: 1, completion: 1, total: 2, cached: 0, reasoning: 0 } }), price)).toBeNull();
    expect(flowCost(flow({ cost: null, usage: null }), price)).toBeNull();
  });
});

describe('computeCost', () => {
  it('prices cached prompt tokens at the cached rate', () => {
    const c = computeCost({ prompt: 2000, completion: 0, total: 2000, cached: 1000, reasoning: 0 }, { input_per_1k: 0.01, output_per_1k: 0, cached_per_1k: 0.001 });
    // 1000 billable @0.01 + 1000 cached @0.001 = 0.01 + 0.001
    expect(c).toBeCloseTo(0.011, 6);
  });
});

describe('elapsedMs', () => {
  it('prefers explicit elapsed_ms, then finished-started, then live now-started', () => {
    expect(elapsedMs(flow({ elapsed_ms: 250 }), 9999)).toBe(250);
    expect(elapsedMs(flow({ elapsed_ms: null, finished_ms: 1800, started_ms: 1000 }), 9999)).toBe(800);
    expect(elapsedMs(flow({ status: 'open', elapsed_ms: null, finished_ms: null, started_ms: 1000 }), 3000)).toBe(2000);
  });
});

describe('isFailover', () => {
  it('tags a requested→served divergence with a target', () => {
    expect(isFailover(flow({ model_requested: 'gpt-4o', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a' }))).toBe(true);
  });
  it('tags an explicit failover terminal reason', () => {
    expect(isFailover(flow({ terminal_reason: 'failover to vllm-b' }))).toBe(true);
  });
  it('does not tag a same-model served row', () => {
    expect(isFailover(flow({ model_requested: 'm', model_served: 'm', upstream_target: 'vllm-a' }))).toBe(false);
  });
});

describe('shortId', () => {
  it('keeps short ids and truncates long ones', () => {
    expect(shortId('api_001')).toBe('api_001');
    expect(shortId('0123456789abcdef')).toBe('…89abcdef');
  });
});
