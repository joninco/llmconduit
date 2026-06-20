import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, waitFor } from '@testing-library/react';
import { FlowsView } from './FlowsView';
import { dashboardStore } from '../store/dashboardStore';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from '../components/testHarness';

/**
 * FlowsView seek coherence (HIGH finding 1): while seeking, the store holds ONLY the frozen
 * snapshot cut. A selection that is ABSENT from that cut (a row selected while live, then scrubbed
 * to before it existed) must NOT open the inspector — opening it would fetch live `/flows/:id`
 * detail and leak a future flow into the frozen view. The inspector reopens once the row is back
 * in the live store.
 *
 * The virtualizer needs a real viewport (jsdom reports 0), so we stub ResizeObserver + a fixed
 * 600px scroll container height, mirroring FlowTable.test.tsx.
 */
const VIEWPORT = 600;
let restoreLayout: (() => void) | null = null;

beforeEach(() => {
  resetWorld();
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  const hgt = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetHeight');
  const wdt = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetWidth');
  Object.defineProperty(HTMLElement.prototype, 'offsetHeight', {
    configurable: true,
    get(this: HTMLElement) {
      return this.getAttribute('data-testid') === 'flow-table-scroll' ? VIEWPORT : 0;
    },
  });
  Object.defineProperty(HTMLElement.prototype, 'offsetWidth', {
    configurable: true,
    get(this: HTMLElement) {
      return this.getAttribute('data-testid') === 'flow-table-scroll' ? 900 : 0;
    },
  });
  restoreLayout = () => {
    if (hgt) Object.defineProperty(HTMLElement.prototype, 'offsetHeight', hgt);
    if (wdt) Object.defineProperty(HTMLElement.prototype, 'offsetWidth', wdt);
  };
});

afterEach(() => {
  cleanup();
  restoreLayout?.();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

/** Click the first rendered flow row's button → drives FlowsView's selection (onSelect). */
function selectFirstRow(getAllByTestId: (id: string) => HTMLElement[]): void {
  fireEvent.click(getAllByTestId('flow-row')[0]!.querySelector('button')!);
}

describe('FlowsView — seek suppresses a selection absent from the frozen snapshot (finding 1)', () => {
  it('opens the inspector for a selected row that IS in the frozen snapshot', async () => {
    seedFlows([makeFlow({ api_call_id: 'api_present', status: 'completed', started_ms: 1_700_000_000_000 })]);
    const { getAllByTestId, queryByTestId } = renderWithQuery(<FlowsView />);
    await waitFor(() => expect(getAllByTestId('flow-row').length).toBeGreaterThan(0));
    act(() => selectFirstRow(getAllByTestId));
    expect(queryByTestId('flow-detail')).toBeTruthy();
    // Enter seek: the row is STILL in the frozen store → the inspector stays open.
    act(() => dashboardStore.getState().setConnection('seeking'));
    expect(queryByTestId('flow-detail')).toBeTruthy();
  });

  it('does NOT open the inspector when the selected row is absent from the frozen cut', async () => {
    // Select a live row, then scrub to a frozen cut that no longer contains it → the inspector
    // must close (no live `/flows/:id` fetch for a future flow).
    seedFlows([makeFlow({ api_call_id: 'api_future', status: 'open', started_ms: 1_700_000_999_999 })]);
    const { getAllByTestId, queryByTestId } = renderWithQuery(<FlowsView />);
    await waitFor(() => expect(getAllByTestId('flow-row').length).toBeGreaterThan(0));
    act(() => selectFirstRow(getAllByTestId));
    expect(queryByTestId('flow-detail')).toBeTruthy();

    // Enter seek with a frozen cut that does NOT contain api_future (a moment before it existed).
    // `applySnapshot` replaces the store rows; we then flip to 'seeking'.
    act(() => {
      seedFlows([makeFlow({ api_call_id: 'api_older', status: 'completed', started_ms: 1_700_000_000_000 })]);
      dashboardStore.getState().setConnection('seeking');
    });
    // The selection is absent from the frozen rows → inspector suppressed (no future flow detail).
    expect(queryByTestId('flow-detail')).toBeNull();

    // Back to LIVE: the original selection re-reveals once the row returns to the live store.
    act(() => {
      seedFlows([makeFlow({ api_call_id: 'api_future', status: 'open', started_ms: 1_700_000_999_999 })]);
      dashboardStore.getState().setConnection('live');
    });
    expect(queryByTestId('flow-detail')).toBeTruthy();
  });
});
