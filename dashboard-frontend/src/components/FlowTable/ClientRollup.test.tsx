import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { cleanup, fireEvent } from '@testing-library/react';
import { ClientRollup } from './ClientRollup';
import { makeFlow, renderWithQuery, resetWorld } from '../testHarness';
import { flowFilterStore } from '../../store/flowFilterStore';
import type { FlowSummary } from '../../api/types';

/**
 * ClientRollup (gap 15) component: the AGGREGATE "by client" roll-up (cost / errors / latency per
 * non-secret client) — collapsed by default, expands to a table; a row cross-links into the
 * per-client filter. Asserts the cross-cutting rules: source-strength DQ tags (weak-UA visibly
 * distinct), don't-lie-with-zeros (no attributed client ⇒ explicit unavailable —), and the
 * cost/err/latency figures + the filter cross-link.
 */
beforeEach(() => resetWorld());
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** `ClientRollup` reads its rows from a prop, but seeding the store keeps the harness consistent. */
function rollupOf(rows: FlowSummary[]) {
  return renderWithQuery(<ClientRollup rows={rows} />);
}

describe('ClientRollup — don\'t-lie-with-zeros', () => {
  it('NO attributed flow ⇒ panel data-available=false + an explicit unavailable — line (not hidden, not 0)', () => {
    const { getByTestId } = rollupOf([
      makeFlow({ api_call_id: 'u1', client_label: null }),
      makeFlow({ api_call_id: 'u2', client_label: null }),
    ]);
    const panel = getByTestId('client-rollup-panel');
    expect(panel.getAttribute('data-available')).toBe('false');
    // Expand to reveal the explicit unavailable companion line.
    fireEvent.click(getByTestId('client-rollup-toggle'));
    const unavail = getByTestId('client-rollup-unavailable');
    expect(unavail.getAttribute('data-quality')).toBe('unavailable');
    expect(unavail.textContent).toContain('—');
    expect(unavail.textContent).toContain('unattributed');
  });
});

describe('ClientRollup — aggregate by client with source-strength tags + cross-link', () => {
  it('rolls cost/err/latency up by client, tags strong (measured) vs weak-UA (derived), orders by flow count', () => {
    const { getByTestId, getAllByTestId } = rollupOf([
      // key-A: 2 flows, both completed ⇒ 0% err; cost summed (priced, confident), latency averaged.
      makeFlow({ api_call_id: 'a1', client_label: 'key-A', client_source: 'key_hash', status: 'completed', cost: 0.006, cost_confidence: 'confident', elapsed_ms: 2000 }),
      makeFlow({ api_call_id: 'a2', client_label: 'key-A', client_source: 'key_hash', status: 'completed', cost: 0.002, cost_confidence: 'confident', elapsed_ms: 4000 }),
      // ua-client: 1 flow, WEAK user-agent fallback.
      makeFlow({ api_call_id: 'b1', client_label: 'python-httpx/0.27', client_source: 'user_agent', status: 'completed', cost: null, elapsed_ms: 1000 }),
    ]);
    fireEvent.click(getByTestId('client-rollup-toggle'));
    expect(getByTestId('client-rollup-table')).toBeTruthy();

    const rows = getAllByTestId('client-rollup-row');
    expect(rows.length).toBe(2);
    // Ordered by observed flow count desc ⇒ key-A (2) first.
    const keyRow = rows[0]!;
    expect(keyRow.getAttribute('data-client')).toBe('key-A');
    expect(keyRow.getAttribute('data-strength')).toBe('strong');
    expect(keyRow.querySelector('[data-testid="client-rollup-flows"]')!.textContent).toBe('2');
    expect(keyRow.querySelector('[data-testid="client-rollup-err"]')!.textContent).toBe('0%');
    expect(keyRow.querySelector('[data-testid="client-rollup-err"]')!.getAttribute('data-quality')).toBe('derived');
    // cost summed (all-confident priced ⇒ derived); latency mean + derived.
    const cost = keyRow.querySelector('[data-testid="client-rollup-cost"]')!;
    expect(cost.getAttribute('data-quality')).toBe('derived');
    expect(cost.textContent).toContain('$');
    const lat = keyRow.querySelector('[data-testid="client-rollup-latency"]')!;
    expect(lat.getAttribute('data-quality')).toBe('derived');
    expect(lat.textContent).toBe('3.0s'); // mean of 2000 + 4000
    // The strong source tag reads measured.
    expect(keyRow.querySelector('[data-testid="client-rollup-source"]')!.getAttribute('data-quality')).toBe('measured');

    // The WEAK UA client is tagged derived + carries the `ua` source badge; its (unpriced) cost reads —.
    const uaRow = rows[1]!;
    expect(uaRow.getAttribute('data-strength')).toBe('weak');
    const uaSource = uaRow.querySelector('[data-testid="client-rollup-source"]')!;
    expect(uaSource.getAttribute('data-quality')).toBe('derived'); // derived, NOT measured
    expect(uaSource.textContent).toBe('ua');
    expect(uaRow.querySelector('[data-testid="client-rollup-cost"]')!.getAttribute('data-quality')).toBe('unavailable');
    expect(uaRow.querySelector('[data-testid="client-rollup-cost"]')!.textContent).toBe('—');
  });

  it('a labelled row with NO source is an UNAVAILABLE source tag — NOT a configured-id + ua badge (review MEDIUM)', () => {
    const { getByTestId, getAllByTestId } = rollupOf([
      // A real label but missing provenance (client_source absent) ⇒ the model marks it `unavailable`.
      makeFlow({ api_call_id: 'ns1', client_label: 'mystery-client', client_source: null, status: 'completed' }),
    ]);
    fireEvent.click(getByTestId('client-rollup-toggle'));
    const row = getAllByTestId('client-rollup-row')[0]!;
    expect(row.getAttribute('data-strength')).toBe('unavailable');
    const tag = row.querySelector('[data-testid="client-rollup-source"]')!;
    // The DQ tag is `unavailable` — NOT masqueraded as a strong identity.
    expect(tag.getAttribute('data-quality')).toBe('unavailable');
    // The badge is the source-unavailable `?` marker — NOT `ua` (would imply a UA fallback) nor `id`.
    expect(tag.textContent).toBe('?');
    expect(tag.textContent).not.toBe('ua');
    expect(tag.textContent).not.toBe('id');
    expect(tag.getAttribute('data-source')).toBeNull();
    // The pick title does NOT claim a configured-id / UA identity.
    const pick = row.querySelector('[data-testid="client-rollup-pick"]') as HTMLElement;
    expect(pick.getAttribute('title')).toContain('source unavailable');
    expect(pick.getAttribute('title')).not.toContain('configured caller-id');
  });

  it('a failing client shows a derived error rate; clicking a row SETS the per-client filter (cross-link)', () => {
    const { getByTestId, getAllByTestId } = rollupOf([
      makeFlow({ api_call_id: 'f1', client_label: 'svc-checkout', client_source: 'configured_header', status: 'failed' }),
      makeFlow({ api_call_id: 'f2', client_label: 'svc-checkout', client_source: 'configured_header', status: 'completed' }),
    ]);
    fireEvent.click(getByTestId('client-rollup-toggle'));
    const row = getAllByTestId('client-rollup-row')[0]!;
    // 1 of 2 failed ⇒ 50% derived.
    expect(row.querySelector('[data-testid="client-rollup-err"]')!.textContent).toBe('50%');

    // Click the row's pick button → the shared filter store gets the client facet set.
    expect(flowFilterStore.getState().filters.client).toBeNull();
    fireEvent.click(row.querySelector('[data-testid="client-rollup-pick"]') as HTMLElement);
    expect(flowFilterStore.getState().filters.client).toBe('svc-checkout');
  });
});
