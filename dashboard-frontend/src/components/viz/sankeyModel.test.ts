import { describe, it, expect } from 'vitest';
import { buildSankeyModel, costColor, flowWindowCost } from './sankeyModel';
import type { FlowSummary, ModelPrice } from '../../api/types';

const NOW = 1_700_000_000_000;

function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: `api_${Math.random().toString(36).slice(2, 8)}`,
    method: 'POST',
    uri: '/v1/responses',
    status: 'completed',
    started_ms: NOW - 5_000,
    finished_ms: NOW - 1_000,
    ...over,
  };
}

const PRICE: Record<string, ModelPrice> = {
  'gpt-4o': { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025 },
  cheap: { input_per_1k: 0.0001, output_per_1k: 0.0001, cached_per_1k: 0.0001 },
};

describe('buildSankeyModel — 3-column client→gateway→model', () => {
  it('produces client + gateway + one node per served model, with two links per model', () => {
    const flows = [
      flow({ model_served: 'gpt-4o', usage: { prompt: 1000, completion: 500, total: 1500, cached: 0, reasoning: 0 } }),
      flow({ model_served: 'cheap', usage: { prompt: 200, completion: 100, total: 300, cached: 0, reasoning: 0 } }),
    ];
    const m = buildSankeyModel(flows, PRICE, NOW);
    // client, gateway, + 2 model nodes.
    expect(m.nodes.map((n) => n.id).sort()).toEqual(['client', 'gateway', 'model:cheap', 'model:gpt-4o']);
    expect(m.nodes.find((n) => n.id === 'client')?.col).toBe(0);
    expect(m.nodes.find((n) => n.id === 'gateway')?.col).toBe(1);
    expect(m.nodes.find((n) => n.id === 'model:gpt-4o')?.col).toBe(2);
    // Two links per model lane (client→gateway, gateway→model), 4 total.
    expect(m.links).toHaveLength(4);
    const gpt = m.links.filter((l) => l.model === 'gpt-4o');
    expect(gpt.every((l) => l.value === 1500)).toBe(true);
  });

  it('band height (value) = summed tokens/window; tokens outside the window are excluded', () => {
    const flows = [
      flow({ model_served: 'gpt-4o', usage: { prompt: 400, completion: 100, total: 500, cached: 0, reasoning: 0 } }),
      flow({ model_served: 'gpt-4o', usage: { prompt: 800, completion: 200, total: 1000, cached: 0, reasoning: 0 } }),
      // Outside the 30s window (finished 60s ago) → excluded.
      flow({
        model_served: 'gpt-4o', started_ms: NOW - 90_000, finished_ms: NOW - 60_000,
        usage: { prompt: 9999, completion: 9999, total: 9999, cached: 0, reasoning: 0 },
      }),
    ];
    const m = buildSankeyModel(flows, PRICE, NOW, 30_000);
    const gatewayToModel = m.links.find((l) => l.source === 'gateway' && l.model === 'gpt-4o');
    expect(gatewayToModel?.value).toBe(1500); // 500 + 1000, the 9999 excluded
    expect(m.totalTokens).toBe(1500);
  });

  it('cost-colors hotter for the more expensive lane; $/min projects the windowed cost', () => {
    const flows = [
      flow({ model_served: 'gpt-4o', usage: { prompt: 1000, completion: 1000, total: 2000, cached: 0, reasoning: 0 } }),
      flow({ model_served: 'cheap', usage: { prompt: 1000, completion: 1000, total: 2000, cached: 0, reasoning: 0 } }),
    ];
    const m = buildSankeyModel(flows, PRICE, NOW, 30_000);
    const gptCost = m.links.find((l) => l.model === 'gpt-4o')!.cost;
    const cheapCost = m.links.find((l) => l.model === 'cheap')!.cost;
    expect(gptCost).toBeGreaterThan(cheapCost);
    const maxCost = Math.max(gptCost, cheapCost);
    // The expensive lane maps to the hot (red) end; the cheap lane stays cooler (more blue).
    const hot = costColor(gptCost, maxCost);
    const cool = costColor(cheapCost, maxCost);
    expect(hot).toBe('#ff6b6b');
    expect(cool).not.toBe('#ff6b6b');
    // $/min = total windowed cost projected from 30s to 60s (×2), finite and positive.
    expect(m.costPerMin).toBeCloseTo((gptCost + cheapCost) * 2, 6);
  });

  it('drops flows with no served model, no usage, or zero tokens (no phantom bands)', () => {
    const flows = [
      flow({ model_served: null, model_requested: null, usage: { prompt: 1, completion: 1, total: 2, cached: 0, reasoning: 0 } }),
      flow({ model_served: 'gpt-4o', usage: null }),
      flow({ model_served: 'gpt-4o', usage: { prompt: 0, completion: 0, total: 0, cached: 0, reasoning: 0 } }),
    ];
    const m = buildSankeyModel(flows, PRICE, NOW);
    expect(m.links).toHaveLength(0);
    expect(m.nodes.map((n) => n.id)).toEqual(['client', 'gateway']);
  });

  it('flowWindowCost bills input+cached+output at their respective rates', () => {
    const f = flow({ usage: { prompt: 1000, completion: 1000, total: 2000, cached: 200, reasoning: 0 } });
    // input = (1000-200)=800 @0.005/1k, cached=200 @0.0025/1k, output=1000 @0.015/1k.
    const expected = (800 / 1000) * 0.005 + (200 / 1000) * 0.0025 + (1000 / 1000) * 0.015;
    expect(flowWindowCost(f, PRICE['gpt-4o'])).toBeCloseTo(expected, 9);
    expect(flowWindowCost(f, undefined)).toBe(0);
  });
});
