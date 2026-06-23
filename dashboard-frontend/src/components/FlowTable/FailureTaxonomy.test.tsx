import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { cleanup, waitFor } from '@testing-library/react';
import { FailureTaxonomy } from './FailureTaxonomy';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from '../testHarness';

/**
 * FailureTaxonomy (gap 14) component: renders the AGGREGATE failure groups (reason × model/provider
 * with a derived rate) + the overall error-rate chip, driven by the live store flow rows. Asserts the
 * cross-cutting rules: don't-lie-with-zeros (empty ⇒ absent panel; all-success ⇒ measured `0%`, not a
 * blank), the grouping/rate provenance tags, and the reason chips.
 */
beforeEach(() => resetWorld());
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('FailureTaxonomy — don\'t-lie-with-zeros', () => {
  it('a ZERO-sample window renders an EXPLICIT unavailable — state (NOT hidden, NOT 0%) — review MEDIUM', async () => {
    const { getByTestId } = renderWithQuery(<FailureTaxonomy />);
    // The panel ALWAYS renders; with no observed flows it is the unavailable state.
    await waitFor(() => expect(getByTestId('failure-taxonomy')).toBeTruthy());
    expect(getByTestId('failure-taxonomy').getAttribute('data-available')).toBe('false');
    const chip = getByTestId('failure-error-rate');
    expect(chip.getAttribute('data-quality')).toBe('unavailable'); // distinct from a measured 0%
    expect(getByTestId('failure-error-rate-value').textContent).toBe('—'); // —, NOT 0%
    expect(getByTestId('failure-unavailable')).toBeTruthy(); // explicit companion line
    expect(getByTestId('failure-grouping-quality').getAttribute('data-quality')).toBe('unavailable');
  });

  it('an all-SUCCESS window shows a MEASURED 0% (derived) + an explicit "no failures" — DISTINCT from unavailable', async () => {
    seedFlows([
      makeFlow({ api_call_id: 'ok_1', status: 'completed' }),
      makeFlow({ api_call_id: 'ok_2', status: 'completed' }),
    ]);
    const { getByTestId } = renderWithQuery(<FailureTaxonomy />);
    await waitFor(() => expect(getByTestId('failure-taxonomy')).toBeTruthy());
    expect(getByTestId('failure-taxonomy').getAttribute('data-available')).toBe('true');
    const chip = getByTestId('failure-error-rate');
    expect(chip.getAttribute('data-quality')).toBe('derived'); // observed ⇒ derived, not unavailable
    expect(getByTestId('failure-error-rate-value').textContent).toBe('0%'); // measured-base zero, NOT —
    expect(getByTestId('failure-none')).toBeTruthy(); // explicit "no failures", not the unavailable line
  });
});

describe('FailureTaxonomy — aggregate grouping by reason × model/provider with a derived rate', () => {
  it('groups failures, shows the derived per-group + overall rate, and the BOUNDED reason chips', async () => {
    seedFlows([
      // openai/gpt-4o: 1 failed (http_status) of 2 ⇒ 50%.
      makeFlow({ api_call_id: 'a1', status: 'failed', upstream_target: 'openai', model_served: 'gpt-4o', terminal_reason: 'upstream 503', attempts: [{ provider: 'openai', model: 'gpt-4o', start_ms: 10, end_ms: 20, status: 'failed', error_class: 'http_status', failover_reason: 'terminal_no_failover' }] }),
      makeFlow({ api_call_id: 'a2', status: 'completed', upstream_target: 'openai', model_served: 'gpt-4o' }),
      // vllm-a/llama: 2 failed of 2 ⇒ 100%, distinct BOUNDED reasons.
      makeFlow({ api_call_id: 'b1', status: 'failed', upstream_target: 'vllm-a', model_served: 'llama', terminal_reason: 'timeout', attempts: [{ provider: 'vllm-a', model: 'llama', start_ms: 10, end_ms: 20, status: 'failed', error_class: 'timeout', failover_reason: 'terminal_no_failover' }] }),
      makeFlow({ api_call_id: 'b2', status: 'failed', upstream_target: 'vllm-a', model_served: 'llama', terminal_reason: 'upstream 500', attempts: [{ provider: 'vllm-a', model: 'llama', start_ms: 10, end_ms: 20, status: 'failed', error_class: 'connect', failover_reason: 'terminal_no_failover' }] }),
    ]);
    const { getByTestId, getAllByTestId } = renderWithQuery(<FailureTaxonomy />);
    await waitFor(() => expect(getByTestId('failure-taxonomy')).toBeTruthy());

    // Overall: 3 failed of 4 ⇒ 75%, derived.
    expect(getByTestId('failure-error-rate-value').textContent).toBe('75%');
    expect(getByTestId('failure-error-rate').getAttribute('data-quality')).toBe('derived');
    // Grouping is tagged measured.
    expect(getByTestId('failure-grouping-quality').getAttribute('data-quality')).toBe('measured');

    const groups = getAllByTestId('failure-group');
    expect(groups.length).toBe(2);
    // Ordered by failed-count desc ⇒ vllm-a (2) first.
    const first = groups[0]!;
    expect(first.getAttribute('data-group-key')).toBe('vllm-a|llama');
    expect(first.querySelector('[data-testid="failure-group-rate"]')!.textContent).toBe('100%');
    expect(first.querySelector('[data-testid="failure-group-rate"]')!.getAttribute('data-quality')).toBe('derived');
    expect(first.querySelector('[data-testid="failure-group-count"]')!.textContent).toBe('2/2');
    // BOUNDED reason keys (gap-03 classes) — never the free-form terminal_reason strings.
    const reasonKeys = Array.from(first.querySelectorAll('[data-testid="failure-reason"]')).map((n) => n.getAttribute('data-reason-key'));
    expect(reasonKeys.sort()).toEqual(['class:connect', 'class:timeout']);
  });

  it('surfaces the gap-03 error_class as the reason source when terminal_reason is absent', async () => {
    seedFlows([
      makeFlow({
        api_call_id: 'c1',
        status: 'failed',
        upstream_target: 'vllm-b',
        model_served: 'llama',
        terminal_reason: null,
        attempts: [{ provider: 'vllm-b', model: 'llama', start_ms: 10, end_ms: 20, status: 'failed', error_class: 'connect', failover_reason: 'terminal_no_failover' }],
      }),
    ]);
    const { getByTestId } = renderWithQuery(<FailureTaxonomy />);
    await waitFor(() => expect(getByTestId('failure-taxonomy')).toBeTruthy());
    const reason = getByTestId('failure-reason');
    expect(reason.getAttribute('data-source')).toBe('error_class');
    expect(reason.getAttribute('data-reason-key')).toBe('class:connect');
    expect(reason.textContent).toContain('connect');
  });
});
