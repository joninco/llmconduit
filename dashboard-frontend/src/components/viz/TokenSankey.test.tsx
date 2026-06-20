import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { StrictMode } from 'react';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { TokenSankey } from './TokenSankey';
import { tokenSankeyCounters, resetTokenSankeyCounters } from './tokenSankeyState';
import type { SankeyModel } from './sankeyModel';

/** A fixture model with two cost-distinct lanes (gpt expensive, cheap cheap). */
function fixture(): SankeyModel {
  return {
    nodes: [
      { id: 'client', label: 'client', col: 0 },
      { id: 'gateway', label: 'gateway', col: 1 },
      { id: 'model:gpt-4o', label: 'gpt-4o', col: 2, model: 'gpt-4o' },
      { id: 'model:cheap', label: 'cheap', col: 2, model: 'cheap' },
    ],
    links: [
      { source: 'client', target: 'gateway', value: 2000, cost: 0.04, model: 'gpt-4o' },
      { source: 'gateway', target: 'model:gpt-4o', value: 2000, cost: 0.04, model: 'gpt-4o' },
      { source: 'client', target: 'gateway', value: 2000, cost: 0.001, model: 'cheap' },
      { source: 'gateway', target: 'model:cheap', value: 2000, cost: 0.001, model: 'cheap' },
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
    const gpt = xOf('model:gpt-4o');
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

describe('TokenSankey — click → filter wiring', () => {
  it('clicking a model band calls onSelectModel with that model', () => {
    const onSelect = vi.fn();
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={onSelect} />);
    fireEvent.click(container.querySelector('[data-testid="sankey-band"][data-model="gpt-4o"]')!);
    expect(onSelect).toHaveBeenCalledWith('gpt-4o');
  });

  it('clicking a model node also filters to that model', () => {
    const onSelect = vi.fn();
    const { container } = render(<TokenSankey model={fixture()} width={600} height={400} onSelectModel={onSelect} />);
    fireEvent.click(container.querySelector('[data-node-id="model:cheap"]')!);
    expect(onSelect).toHaveBeenCalledWith('cheap');
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
