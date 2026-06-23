import { describe, it, expect, afterEach } from 'vitest';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { AttemptTrace } from './AttemptTrace';
import { attemptTrace } from './attemptTrace';
import type { Attempt } from '../../api/types';

const T = 1_700_000_000_000;

const FAILED_A: Attempt = {
  provider: 'vllm-a', model: 'llama-3.1-70b', start_ms: T, end_ms: T + 800,
  status: 'failed', error_class: 'http_status', failover_reason: 'provider_failed',
};
const SERVED_B: Attempt = {
  provider: 'openai', model: 'gpt-4o', start_ms: T + 800, end_ms: T + 2000,
  first_upstream_byte_ms: T + 1100, status: 'served',
};

function renderTrace(attempts: Attempt[] | null | undefined) {
  return render(<AttemptTrace model={attemptTrace(attempts)} />);
}

describe('AttemptTrace component (gap 11)', () => {
  afterEach(cleanup);

  it('renders a failover chain: a failed node + the served node + a connector arrow', () => {
    const { getByTestId } = renderTrace([FAILED_A, SERVED_B]);
    const trace = getByTestId('attempt-trace');
    expect(trace.getAttribute('data-failover')).toBe('true');
    // Two nodes…
    const nodeA = getByTestId('attempt-node-0');
    const nodeB = getByTestId('attempt-node-1');
    // …the failed node carries its error class on the headline.
    expect(nodeA.getAttribute('data-status')).toBe('failed');
    expect(getByTestId('attempt-error-0').textContent).toContain('http status');
    // …the served node is marked distinct (data-served) — the visual difference (spec 11).
    expect(nodeB.getAttribute('data-served')).toBe('true');
    expect(getByTestId('attempt-status-1').textContent).toBe('served');
    // The chain shows the failed→served handoff summary + the connector arrow between nodes.
    expect(getByTestId('attempt-failover-label').textContent).toContain('B served');
    expect(getByTestId('attempt-arrow')).toBeTruthy();
  });

  it('shows measured durations and — for an unmeasured first byte (never 0)', () => {
    const { getByTestId } = renderTrace([FAILED_A, SERVED_B]);
    // Failed attempt duration 800ms (measured).
    const durA = getByTestId('attempt-duration-0');
    expect(durA.getAttribute('data-quality')).toBe('measured');
    expect(durA.textContent).toBe('800ms');

    // Expand the FAILED node: its first byte is UNAVAILABLE (—, never 0) — no header arrived.
    fireEvent.click(getByTestId('attempt-toggle-0'));
    const byteA = getByTestId('attempt-firstbyte-0');
    expect(byteA.getAttribute('data-quality')).toBe('unavailable');
    expect(byteA.textContent).toBe('—');

    // Expand the SERVED node: its first byte is measured (300ms relative to its start).
    fireEvent.click(getByTestId('attempt-toggle-1'));
    const byteB = getByTestId('attempt-firstbyte-1');
    expect(byteB.getAttribute('data-quality')).toBe('measured');
    expect(byteB.textContent).toBe('300ms');
    // The expanded detail exposes the model + the failover reason on the failed node.
    expect(getByTestId('attempt-detail-1').textContent).toContain('gpt-4o');
    expect(getByTestId('attempt-failover-0').textContent).toContain('failover');
  });

  it('a SINGLE attempt renders one node + the "no failover" label (no fake chain)', () => {
    const { getByTestId, queryByTestId } = renderTrace([SERVED_B]);
    expect(getByTestId('attempt-trace').getAttribute('data-failover')).toBe('false');
    expect(getByTestId('attempt-single-label')).toBeTruthy();
    expect(queryByTestId('attempt-failover-label')).toBeNull();
    // Exactly one node, no connector arrow.
    expect(getByTestId('attempt-node-0')).toBeTruthy();
    expect(queryByTestId('attempt-node-1')).toBeNull();
    expect(queryByTestId('attempt-arrow')).toBeNull();
  });

  it('renders nothing when there is no trace (empty/absent attempts)', () => {
    const { container } = renderTrace([]);
    expect(container.querySelector('[data-testid="attempt-trace"]')).toBeNull();
    const { container: c2 } = renderTrace(undefined);
    expect(c2.querySelector('[data-testid="attempt-trace"]')).toBeNull();
  });

  it('a measured 0ms attempt duration reads 0ms (distinct from unavailable —)', () => {
    const zero: Attempt = { ...SERVED_B, start_ms: T, end_ms: T };
    const { getByTestId } = renderTrace([zero]);
    const dur = getByTestId('attempt-duration-0');
    expect(dur.getAttribute('data-quality')).toBe('measured');
    expect(dur.textContent).toBe('0ms');
  });

  it('flags a clock-disordered attempt with a skew marker (clamped, not negative)', () => {
    const disordered: Attempt = { ...FAILED_A, start_ms: T + 500, end_ms: T + 100 };
    const { getByTestId } = renderTrace([disordered]);
    expect(getByTestId('attempt-skew-0')).toBeTruthy();
    expect(getByTestId('attempt-duration-0').textContent).toBe('0ms');
  });

  it('flags a DISORDERED first byte (byte before start) with a skew marker, not a bare measured 0ms', () => {
    // The review MEDIUM: a first byte before the attempt start was clamped to a `measured` 0ms with
    // NO marker — indistinguishable from a real 0. The expanded detail must now show a `skew` flag.
    const skewedByte: Attempt = { ...SERVED_B, start_ms: T + 500, first_upstream_byte_ms: T + 100 };
    const { getByTestId } = renderTrace([skewedByte]);
    fireEvent.click(getByTestId('attempt-toggle-0'));
    const byte = getByTestId('attempt-firstbyte-0');
    expect(byte.getAttribute('data-quality')).toBe('measured');
    expect(byte.textContent).toContain('0ms'); // clamped
    expect(getByTestId('attempt-firstbyte-skew-0')).toBeTruthy(); // FLAGGED — distinct from a real 0
  });

  it('renders an all-failed chain with a "none served" summary (no served node)', () => {
    const second: Attempt = { ...FAILED_A, provider: 'vllm-b', start_ms: T + 800, end_ms: T + 1500, error_class: 'timeout' };
    const { getByTestId } = renderTrace([FAILED_A, second]);
    expect(getByTestId('attempt-failover-label').textContent).toContain('none served');
    expect(getByTestId('attempt-node-0').getAttribute('data-served')).toBe('false');
    expect(getByTestId('attempt-node-1').getAttribute('data-served')).toBe('false');
  });
});
