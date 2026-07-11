import { describe, expect, it } from 'vitest';
import type { OverviewLaneRollup } from '../../api/types';
import { buildServerSankeyModel, costColor } from './sankeyModel';

function lane(over: Partial<OverviewLaneRollup> = {}): OverviewLaneRollup {
  return {
    provider: 'vllm-a',
    model: 'gpt-4o',
    requests: 1,
    tokens: { samples: 1, prompt: 1_000, completion: 500, cached: 200, reasoning: 100 },
    cost: { samples: 1, total_usd: 0.02, confidence: 'confident' },
    ...over,
  };
}

describe('buildServerSankeyModel', () => {
  it('builds distinct provider/model lanes from server-authored rollups', () => {
    const model = buildServerSankeyModel([
      lane(),
      lane({ provider: 'vllm-b', tokens: { samples: 1, prompt: 300, completion: 100, cached: null, reasoning: null } }),
    ], 60);
    expect(model.nodes.filter((node) => node.col === 2).map((node) => node.id)).toEqual([
      'served:vllm-a|gpt-4o',
      'served:vllm-b|gpt-4o',
    ]);
    expect(model.links.find((link) => link.target === 'served:vllm-a|gpt-4o')?.value).toBe(1_500);
    expect(model.totalTokens).toBe(1_900);
  });

  it('counts prompt plus completion once and never adds cached/reasoning subsets', () => {
    const model = buildServerSankeyModel([
      lane({ tokens: { samples: 1, prompt: 70_000, completion: 41_994, cached: 20_000, reasoning: 10_000 } }),
    ], 3_600);
    expect(model.totalTokens).toBe(111_994);
  });

  it('uses persisted lane cost and preserves unpriced availability', () => {
    const model = buildServerSankeyModel([
      lane({ cost: { samples: 1, total_usd: 0.12, confidence: 'confident' } }),
      lane({ provider: 'vllm-b', cost: { samples: 0, total_usd: null, confidence: 'unavailable' } }),
    ], 60);
    expect(model.costPerMin).toBeCloseTo(0.12);
    expect(model.links.find((link) => link.target === 'served:vllm-a|gpt-4o')?.costAvailable).toBe(true);
    expect(model.links.find((link) => link.target === 'served:vllm-b|gpt-4o')?.costAvailable).toBe(false);
  });

  it('drops zero-volume lanes and handles a null provider', () => {
    const model = buildServerSankeyModel([
      lane({ provider: '', tokens: { samples: 1, prompt: 100, completion: 0, cached: null, reasoning: null } }),
      lane({ model: 'empty', tokens: { samples: 1, prompt: 0, completion: 0, cached: null, reasoning: null } }),
    ], 60);
    expect(model.nodes.find((node) => node.col === 2)?.id).toBe('served:|gpt-4o');
    expect(model.links).toHaveLength(2);
  });
});

describe('costColor', () => {
  it('maps the largest terminal cost to the hot end', () => {
    expect(costColor(2, 2)).toBe('#ff6b6b');
    expect(costColor(0, 2)).toBe('#6bb6ff');
  });
});
