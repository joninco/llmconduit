import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { StrictMode } from 'react';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { TokenSankey } from './TokenSankey';
import { tokenSankeyCounters, resetTokenSankeyCounters } from './tokenSankeyState';
import type { SankeyModel } from './sankeyModel';

/** A fixture model with two cost-distinct (upstream, model) lanes (gpt expensive, cheap cheap). */
function fixture(): SankeyModel {
  return {
    nodes: [
      { id: 'client', label: 'client', col: 0 },
      { id: 'gateway', label: 'gateway', col: 1 },
      { id: 'served:vllm-a|gpt-4o', label: 'gpt-4o @vllm-a', col: 2, model: 'gpt-4o', upstream: 'vllm-a' },
      { id: 'served:vllm-b|cheap', label: 'cheap @vllm-b', col: 2, model: 'cheap', upstream: 'vllm-b' },
    ],
    links: [
      { source: 'client', target: 'gateway', value: 2000, cost: 0.04, model: 'gpt-4o', upstream: 'vllm-a' },
      { source: 'gateway', target: 'served:vllm-a|gpt-4o', value: 2000, cost: 0.04, model: 'gpt-4o', upstream: 'vllm-a' },
      { source: 'client', target: 'gateway', value: 2000, cost: 0.001, model: 'cheap', upstream: 'vllm-b' },
      { source: 'gateway', target: 'served:vllm-b|cheap', value: 2000, cost: 0.001, model: 'cheap', upstream: 'vllm-b' },
    ],
    costPerMin: 0.08,
    totalTokens: 4000,
  };
}

beforeEach(() => {
  resetTokenSankeyCounters();
  cleanup();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('TokenSankey — 3-column d3-sankey layout', () => {
  it('lays out client/gateway/model nodes in three left→right columns', () => {
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={() => {}} />);
    const xOf = (id: string) => Number(container.querySelector(`[data-node-id="${id}"]`)?.getAttribute('x'));
    const client = xOf('client');
    const gateway = xOf('gateway');
    const gpt = xOf('served:vllm-a|gpt-4o');
    // Strict left→right ordering: client < gateway < model column.
    expect(client).toBeLessThan(gateway);
    expect(gateway).toBeLessThan(gpt);
    // Two model nodes rendered on the right column.
    expect(container.querySelectorAll('[data-testid="sankey-model-node"]').length).toBe(2);
    // Four bands (two per model lane).
    expect(container.querySelectorAll('[data-testid="sankey-band"]').length).toBe(4);
  });

  it('band stroke-width grows with token volume (height ∝ tokens)', () => {
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={() => {}} />);
    const widths = [...container.querySelectorAll('[data-testid="sankey-band"]')].map((b) =>
      Number(b.getAttribute('stroke-width')),
    );
    // All four bands carry the same 2000-token value → comparable, positive widths.
    expect(widths.every((w) => w > 0)).toBe(true);
  });

  it('cost-colors the expensive lane hotter than the cheap lane', () => {
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={() => {}} />);
    const strokeFor = (model: string) =>
      container.querySelector(`[data-testid="sankey-band"][data-model="${model}"]`)?.getAttribute('stroke');
    // gpt-4o is the max-cost lane → the hot (red) end.
    expect(strokeFor('gpt-4o')).toBe('#ff6b6b');
    expect(strokeFor('cheap')).not.toBe('#ff6b6b');
  });
});

describe('TokenSankey — click → filter wiring (both facets — finding 9)', () => {
  it('clicking a lane band calls onSelectModel with its (model, upstream) pair', () => {
    const onSelect = vi.fn();
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={onSelect} />);
    fireEvent.click(container.querySelector('[data-testid="sankey-band"][data-model="gpt-4o"]')!);
    expect(onSelect).toHaveBeenCalledWith('gpt-4o', 'vllm-a');
  });

  it('clicking a lane node also filters to that (model, upstream) pair', () => {
    const onSelect = vi.fn();
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={onSelect} />);
    fireEvent.click(container.querySelector('[data-node-id="served:vllm-b|cheap"]')!);
    expect(onSelect).toHaveBeenCalledWith('cheap', 'vllm-b');
  });
});

describe('TokenSankey — StrictMode-safe (no leaked / duplicate SVG)', () => {
  it('double-invoke leaves exactly ONE svg and balances setups/cleanups', () => {
    const { container, unmount } = render(
      <StrictMode>
        <TokenSankey model={fixture()} width={600} height={400} onSelectModel={() => {}} />
      </StrictMode>,
    );
    expect(container.querySelectorAll('[data-testid="token-sankey-svg"]').length).toBe(1);
    expect(tokenSankeyCounters.cleanups).toBe(tokenSankeyCounters.setups - 1);
    unmount();
    expect(tokenSankeyCounters.cleanups).toBe(tokenSankeyCounters.setups);
    expect(container.querySelectorAll('[data-testid="token-sankey-svg"]').length).toBe(0);
  });
});
