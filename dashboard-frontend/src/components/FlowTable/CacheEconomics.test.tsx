import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, fireEvent, render, within } from '@testing-library/react';
import { CacheEconomics } from './CacheEconomics';
import { makeFlow } from '../testHarness';
import type { ModelPrice } from '../../api/types';

const PRICE_TABLE: Record<string, ModelPrice> = {
  'gpt-4o': { input_per_1k: 0.005, output_per_1k: 0.015, cached_per_1k: 0.0025, cached_price_configured: true },
  'llama-3.1-70b': { input_per_1k: 0.0008, output_per_1k: 0.0008, cached_per_1k: 0, cached_price_configured: false },
};

/** Expand the collapsed panel and return its table. */
function expand(container: HTMLElement): HTMLElement {
  fireEvent.click(within(container).getByTestId('cache-economics-toggle'));
  return within(container).getByTestId('cache-economics-table');
}

describe('CacheEconomics — aggregate cache-hit by model (gap 08)', () => {
  afterEach(cleanup);

  it('is collapsed by default; the summary reports measured-group coverage', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'gpt-4o', cost_confidence: 'confident', usage: { prompt: 1000, completion: 100, total: 1100, cached: 200 } }),
      makeFlow({ api_call_id: 'b', model_served: 'llama-3.1-70b', usage: { prompt: 1000, completion: 100, total: 1100 } }), // unreported cached
    ];
    const { getByTestId, queryByTestId } = render(<CacheEconomics rows={rows} priceTable={PRICE_TABLE} />);
    // Collapsed: no table yet.
    expect(queryByTestId('cache-economics-table')).toBeNull();
    // 1 of 2 model groups has a measured hit rate (llama never reported cached).
    expect(getByTestId('cache-economics-summary').textContent).toContain('1/2 models with measured hit rate');
  });

  it('shows a derived hit rate + $ saved for a confident gpt-4o group (no est badge)', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'gpt-4o', cost_confidence: 'confident', usage: { prompt: 1000, completion: 100, total: 1100, cached: 200 } }),
      makeFlow({ api_call_id: 'b', model_served: 'gpt-4o', cost_confidence: 'confident', usage: { prompt: 1000, completion: 100, total: 1100, cached: 400 } }),
    ];
    const { container } = render(<CacheEconomics rows={rows} priceTable={PRICE_TABLE} />);
    const table = expand(container);
    const row = within(table).getByTestId('cache-economics-row');
    // (200+400)/(1000+1000) = 30.0%
    expect(within(row).getByTestId('agg-hit-rate').textContent).toContain('30.0%');
    expect(within(row).getByTestId('agg-hit-rate').getAttribute('data-quality')).toBe('derived');
    // saved = (600/1000)*(0.005-0.0025) = 0.0015
    expect(within(row).getByTestId('agg-saved').textContent).toContain('$0.0015');
    // fully confident ⇒ NO est badge.
    expect(within(row).queryByTestId('agg-est')).toBeNull();
    expect(within(row).getByTestId('agg-reported').textContent).toBe('2/2');
  });

  it('labels an ESTIMATED group (a non-confident member) with an est badge', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'gpt-4o', cost_confidence: 'confident', usage: { prompt: 1000, completion: 100, total: 1100, cached: 100 } }),
      makeFlow({ api_call_id: 'b', model_served: 'gpt-4o', cost_confidence: 'estimated', usage: { prompt: 1000, completion: 100, total: 1100, cached: 100 } }),
    ];
    const { container } = render(<CacheEconomics rows={rows} priceTable={PRICE_TABLE} />);
    const table = expand(container);
    expect(within(table).getByTestId('agg-est')).toBeTruthy();
  });

  // Round-1 regression (review gap-08 #2): the `est` badge MUST render whenever the group is
  // estimated, INDEPENDENT of the hit-rate quality. Previously the badge was suppressed on an
  // unavailable rate, so an estimated row with a derived `$ saved` (zero-denominator case below)
  // showed the dollar figure with NO estimate label.
  it('shows the est badge on an estimated row even when the hit rate is unavailable (zero denominator + derived $ saved)', () => {
    const rows = [
      // prompt 0 ⇒ hit-rate denominator 0 ⇒ rate unavailable; cached 100 reported with gpt-4o's
      // CONFIGURED price ⇒ a DERIVED `$ saved`. One estimated member ⇒ the group is an estimate.
      makeFlow({ api_call_id: 'a', model_served: 'gpt-4o', cost_confidence: 'confident', usage: { prompt: 0, completion: 100, total: 100, cached: 100 } }),
      makeFlow({ api_call_id: 'b', model_served: 'gpt-4o', cost_confidence: 'estimated', usage: { prompt: 0, completion: 100, total: 100, cached: 100 } }),
    ];
    const { container } = render(<CacheEconomics rows={rows} priceTable={PRICE_TABLE} />);
    const table = expand(container);
    const row = within(table).getByTestId('cache-economics-row');
    // Rate is unavailable (zero denominator) — the badge must NOT be gated on this.
    expect(within(row).getByTestId('agg-hit-rate').getAttribute('data-quality')).toBe('unavailable');
    // A derived `$ saved` IS shown — so an estimate label is mandatory beside it.
    expect(within(row).getByTestId('agg-saved').getAttribute('data-quality')).toBe('derived');
    // The fix: the est badge is present despite the unavailable rate.
    expect(within(row).getByTestId('agg-est')).toBeTruthy();
  });

  // The unreported-cached variant: an estimated group whose flows never reported cached (rate AND
  // $ saved unavailable) still carries the est badge — the label tracks confidence, not the rate.
  it('shows the est badge on an estimated group with no reported cached (unavailable rate)', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'gpt-4o', cost_confidence: 'estimated', usage: { prompt: 1000, completion: 100, total: 1100 } }),
      makeFlow({ api_call_id: 'b', model_served: 'gpt-4o', cost_confidence: 'confident', usage: { prompt: 1000, completion: 100, total: 1100 } }),
    ];
    const { container } = render(<CacheEconomics rows={rows} priceTable={PRICE_TABLE} />);
    const table = expand(container);
    const row = within(table).getByTestId('cache-economics-row');
    expect(within(row).getByTestId('agg-hit-rate').getAttribute('data-quality')).toBe('unavailable');
    expect(within(row).getByTestId('agg-est')).toBeTruthy();
  });

  it('a model whose flows never reported cached ⇒ "—" hit rate (unavailable), not a fabricated 0%', () => {
    const rows = [
      makeFlow({ api_call_id: 'a', model_served: 'llama-3.1-70b', usage: { prompt: 1000, completion: 100, total: 1100 } }),
    ];
    const { container } = render(<CacheEconomics rows={rows} priceTable={PRICE_TABLE} />);
    const table = expand(container);
    const rate = within(table).getByTestId('agg-hit-rate');
    expect(rate.getAttribute('data-quality')).toBe('unavailable');
    expect(rate.textContent).toContain('—');
    expect(rate.textContent).not.toContain('0.0%');
    // No configured cache price + no reported cached ⇒ "$ saved" unavailable.
    expect(within(table).getByTestId('agg-saved').getAttribute('data-quality')).toBe('unavailable');
    expect(within(table).getByTestId('agg-reported').textContent).toBe('0/1');
  });

  it('renders an empty state when there is no model usage', () => {
    const { container, getByTestId } = render(<CacheEconomics rows={[]} priceTable={PRICE_TABLE} />);
    expect(getByTestId('cache-economics-summary').textContent).toContain('no models');
    fireEvent.click(within(container).getByTestId('cache-economics-toggle'));
    expect(getByTestId('cache-economics-empty')).toBeTruthy();
  });
});
