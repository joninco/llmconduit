import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, fireEvent, render, within } from '@testing-library/react';
import { TokensCell } from './TokensCell';
import { makeFlow } from '../testHarness';

/** Hover the tokens cell to reveal the popover, then return the popover element. */
function openPopover(container: HTMLElement): HTMLElement {
  const cell = within(container).getByTestId('tokens-cell');
  fireEvent.mouseEnter(cell);
  return within(container).getByTestId('tokens-popover');
}

describe('TokensCell — token-economics popover (gap 08)', () => {
  afterEach(cleanup);

  it('reveals the cached/reasoning split, cache-hit, and $ saved on a MEASURED priced flow', () => {
    const flow = makeFlow({
      model_served: 'gpt-4o',
      cost_confidence: 'confident',
      usage: { prompt: 1000, completion: 200, total: 1200, cached: 250, reasoning: 64 },
      cache_price_impact_usd: -0.000625,
    });
    const { container, queryByTestId } = render(<TokensCell flow={flow} />);
    // No popover until hovered.
    expect(queryByTestId('tokens-popover')).toBeNull();
    const pop = openPopover(container);

    const cached = within(pop).getByTestId('econ-line-cached');
    expect(cached.getAttribute('data-quality')).toBe('measured');
    expect(cached.textContent).toContain('250');
    const reasoning = within(pop).getByTestId('econ-line-reasoning');
    expect(reasoning.textContent).toContain('64');

    const hit = within(pop).getByTestId('econ-line-cache hit');
    expect(hit.getAttribute('data-quality')).toBe('derived');
    expect(hit.textContent).toContain('25.0%');

    const saved = within(pop).getByTestId('econ-line-$ saved');
    expect(saved.getAttribute('data-quality')).toBe('derived');
    expect(saved.textContent).toContain('$0.0006');
  });

  it('renders "—" for UNREPORTED cached/reasoning (never "0") and "—" for hit + $ saved', () => {
    const flow = makeFlow({
      model_served: 'gpt-4o',
      usage: { prompt: 1000, completion: 200, total: 1200 }, // cached/reasoning unreported
    });
    const { container } = render(<TokensCell flow={flow} />);
    const pop = openPopover(container);

    const cached = within(pop).getByTestId('econ-line-cached');
    expect(cached.getAttribute('data-quality')).toBe('unavailable');
    expect(cached.querySelector('dd')!.textContent).toBe('—');
    expect(cached.querySelector('dd')!.textContent).not.toBe('0');

    expect(within(pop).getByTestId('econ-line-cache hit').getAttribute('data-quality')).toBe('unavailable');
    // gpt-4o HAS a configured cache price, but with no reported cached count there is no saving.
    expect(within(pop).getByTestId('econ-line-$ saved').getAttribute('data-quality')).toBe('unavailable');
  });

  it('a REPORTED cached 0 reads "0" (a real miss), distinct from unavailable', () => {
    const flow = makeFlow({
      model_served: 'gpt-4o',
      cost_confidence: 'confident',
      usage: { prompt: 1000, completion: 200, total: 1200, cached: 0, reasoning: 0 },
      cache_price_impact_usd: 0,
    });
    const { container } = render(<TokensCell flow={flow} />);
    const pop = openPopover(container);
    const cached = within(pop).getByTestId('econ-line-cached');
    expect(cached.getAttribute('data-quality')).toBe('measured');
    expect(cached.querySelector('dd')!.textContent).toBe('0');
    // 0% hit + $0.00 saved (both measured/derived, not "—").
    expect(within(pop).getByTestId('econ-line-cache hit').textContent).toContain('0.0%');
    expect(within(pop).getByTestId('econ-line-$ saved').querySelector('dd')!.textContent).toBe('$0.00');
  });

  it('cached present but model has NO configured cache price ⇒ split shows, $ saved is "—"', () => {
    const flow = makeFlow({
      model_served: 'llama-3.1-70b',
      cost_confidence: 'estimated',
      usage: { prompt: 1000, completion: 200, total: 1200, cached: 300 },
    });
    const { container } = render(<TokensCell flow={flow} />);
    const pop = openPopover(container);
    // Split still shows the measured cached count…
    expect(within(pop).getByTestId('econ-line-cached').textContent).toContain('300');
    // …but the presence gate keeps "$ saved" unavailable (no fabricated saving from numeric 0.0).
    const saved = within(pop).getByTestId('econ-line-$ saved');
    expect(saved.getAttribute('data-quality')).toBe('unavailable');
    expect(saved.querySelector('dd')!.textContent).toBe('—');
  });

  it('a flow with NO usage renders the plain dual-count and offers no popover', () => {
    const flow = makeFlow({ usage: null });
    const { container, queryByTestId } = render(<TokensCell flow={flow} />);
    const cell = within(container).getByTestId('tokens-cell');
    expect(cell.textContent).toContain('—');
    fireEvent.mouseEnter(cell);
    expect(queryByTestId('tokens-popover')).toBeNull();
  });
});
