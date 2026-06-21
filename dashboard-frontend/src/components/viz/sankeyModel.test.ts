import { describe, it, expect } from 'vitest';
import { buildSankeyModel, costColor, deltaCost, type SankeyUsageDelta } from './sankeyModel';
import type { ModelPrice } from '../../api/types';

const NOW = 1_700_000_000_000;

/** A windowed usage delta at `ts = NOW` by default (inside any rolling window ending at NOW). */
function delta(over: Partial<SankeyUsageDelta> = {}): SankeyUsageDelta {
  return { ts: NOW, upstream: 'vllm-a', model: 'gpt-4o', prompt: 0, cached: 0, completion: 0, total: 0, ...over };
}

const PRICE: Record<string, ModelPrice> = {
  'gpt-4o': { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025 },
  cheap: { input_per_1k: 0.0001, output_per_1k: 0.0001, cached_per_1k: 0.0001 },
};

describe('buildSankeyModel — 3-column client→gateway→(upstream, model)', () => {
  it('produces client + gateway + one node per (upstream, model) lane, with two links each', () => {
    const deltas = [
      delta({ model: 'gpt-4o', upstream: 'vllm-a', prompt: 1000, completion: 500, total: 1500 }),
      delta({ model: 'cheap', upstream: 'vllm-b', prompt: 200, completion: 100, total: 300 }),
    ];
    const m = buildSankeyModel(deltas, PRICE, NOW);
    expect(m.nodes.map((n) => n.id).sort()).toEqual(['client', 'gateway', 'served:vllm-a|gpt-4o', 'served:vllm-b|cheap']);
    expect(m.nodes.find((n) => n.id === 'client')?.col).toBe(0);
    expect(m.nodes.find((n) => n.id === 'gateway')?.col).toBe(1);
    const gpt = m.nodes.find((n) => n.id === 'served:vllm-a|gpt-4o');
    expect(gpt?.col).toBe(2);
    expect(gpt?.model).toBe('gpt-4o');
    expect(gpt?.upstream).toBe('vllm-a');
    // Two links per lane (client→gateway, gateway→lane), 4 total.
    expect(m.links).toHaveLength(4);
    const gptLinks = m.links.filter((l) => l.model === 'gpt-4o');
    expect(gptLinks.every((l) => l.value === 1500 && l.upstream === 'vllm-a')).toBe(true);
  });

  it('keeps the SAME model on different upstreams as DISTINCT lanes (finding 9)', () => {
    const deltas = [
      delta({ model: 'gpt-4o', upstream: 'vllm-a', total: 1000, prompt: 1000 }),
      delta({ model: 'gpt-4o', upstream: 'vllm-b', total: 400, prompt: 400 }),
    ];
    const m = buildSankeyModel(deltas, PRICE, NOW);
    const laneIds = m.nodes.filter((n) => n.col === 2).map((n) => n.id).sort();
    expect(laneIds).toEqual(['served:vllm-a|gpt-4o', 'served:vllm-b|gpt-4o']);
    expect(m.links.find((l) => l.upstream === 'vllm-a' && l.source === 'gateway')?.value).toBe(1000);
    expect(m.links.find((l) => l.upstream === 'vllm-b' && l.source === 'gateway')?.value).toBe(400);
  });

  it('band = summed deltas inside the window; deltas with ts outside the window are excluded (finding 2)', () => {
    const deltas = [
      delta({ model: 'gpt-4o', total: 500, prompt: 400, completion: 100, ts: NOW - 5_000 }),
      delta({ model: 'gpt-4o', total: 1000, prompt: 800, completion: 200, ts: NOW - 10_000 }),
      // Stamped 60s ago → outside the 30s window → excluded (NOT counted as a lifetime total).
      delta({ model: 'gpt-4o', total: 9999, prompt: 9999, ts: NOW - 60_000 }),
    ];
    const m = buildSankeyModel(deltas, PRICE, NOW, 30_000);
    const gatewayToLane = m.links.find((l) => l.source === 'gateway' && l.model === 'gpt-4o');
    expect(gatewayToLane?.value).toBe(1500); // 500 + 1000, the 9999 excluded
    expect(m.totalTokens).toBe(1500);
  });

  it('cost-colors hotter for the more expensive lane; $/min projects the windowed cost', () => {
    const deltas = [
      delta({ model: 'gpt-4o', upstream: 'vllm-a', prompt: 1000, completion: 1000, total: 2000 }),
      delta({ model: 'cheap', upstream: 'vllm-b', prompt: 1000, completion: 1000, total: 2000 }),
    ];
    const m = buildSankeyModel(deltas, PRICE, NOW, 30_000);
    const gptCost = m.links.find((l) => l.model === 'gpt-4o')!.cost;
    const cheapCost = m.links.find((l) => l.model === 'cheap')!.cost;
    expect(gptCost).toBeGreaterThan(cheapCost);
    const maxCost = Math.max(gptCost, cheapCost);
    const hot = costColor(gptCost, maxCost);
    const cool = costColor(cheapCost, maxCost);
    expect(hot).toBe('#ff6b6b');
    expect(cool).not.toBe('#ff6b6b');
    // $/min = total windowed cost projected from 30s to 60s (×2), finite and positive.
    expect(m.costPerMin).toBeCloseTo((gptCost + cheapCost) * 2, 6);
  });

  it('drops deltas with no model or zero tokens (no phantom bands)', () => {
    const deltas = [
      delta({ model: '', total: 2 }),
      delta({ model: 'gpt-4o', total: 0 }),
    ];
    const m = buildSankeyModel(deltas, PRICE, NOW);
    expect(m.links).toHaveLength(0);
    expect(m.nodes.map((n) => n.id)).toEqual(['client', 'gateway']);
  });

  it('a null-upstream lane collapses to a `?` segment but still renders', () => {
    const m = buildSankeyModel([delta({ upstream: null, model: 'gpt-4o', total: 100, prompt: 100 })], PRICE, NOW);
    const lane = m.nodes.find((n) => n.col === 2);
    expect(lane?.id).toBe('served:?|gpt-4o');
    expect(lane?.upstream).toBeNull();
  });

  it('deltaCost bills input+cached+output at their respective rates', () => {
    const d = delta({ prompt: 1000, completion: 1000, cached: 200, total: 2000 });
    // input = (1000-200)=800 @0.005/1k, cached=200 @0.0025/1k, output=1000 @0.015/1k.
    const expected = (800 / 1000) * 0.005 + (200 / 1000) * 0.0025 + (1000 / 1000) * 0.015;
    expect(deltaCost(d, PRICE['gpt-4o'])).toBeCloseTo(expected, 9);
    expect(deltaCost(d, undefined)).toBe(0);
  });
});
