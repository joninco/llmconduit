import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, within } from '@testing-library/react';
import { FlowTable } from './FlowTable';
import { dashboardStore } from '../../store/dashboardStore';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from '../testHarness';

/**
 * jsdom reports zero layout, so `@tanstack/react-virtual` would render an empty window. We stub a
 * ResizeObserver and a fixed 600px viewport height on the scroll container so the virtualizer has
 * a real window to compute — then we can assert it renders only a SLICE of 10k rows.
 */
const VIEWPORT = 600;
let restoreLayout: (() => void) | null = null;

beforeEach(() => {
  resetWorld();
  // jsdom has no ResizeObserver; provide a no-op one so the virtualizer's observe path doesn't
  // throw. The viewport size comes from offset* below (the virtualizer reads `offsetHeight`).
  vi.stubGlobal('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} });
  // `@tanstack/virtual-core`'s getRect reads element.offsetWidth/offsetHeight, which jsdom hard
  // -codes to 0. Override the getters so ONLY the scroll container reports a real 600px viewport
  // (rows keep 0 — they don't self-measure here; positions come from the fixed estimateSize).
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
      return this.getAttribute('data-testid') === 'flow-table-scroll' ? 1000 : 0;
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
  restoreLayout = null;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

function noop() {}

describe('FlowTable — virtualization', () => {
  it('renders only a windowed SLICE of 10k rows (not 10k DOM nodes)', () => {
    const flows = Array.from({ length: 10_000 }, (_, i) =>
      makeFlow({ api_call_id: `api_${String(i).padStart(5, '0')}`, started_ms: 1_700_000_000_000 + i }),
    );
    seedFlows(flows);
    const { getByTestId, getAllByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);

    // The list reports 10k total via the filter-bar count…
    expect(getByTestId('flow-count').textContent).toContain('10000');
    // …but only the visible window + overscan is in the DOM (far fewer than 10k rows).
    const rows = getAllByTestId('flow-row');
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.length).toBeLessThan(200);
    void getByTestId('flow-table-scroll');
  });
});

describe('FlowTable — filtering', () => {
  beforeEach(() => {
    seedFlows([
      makeFlow({ api_call_id: 'api_ok', status: 'completed', model_requested: 'gpt-4o', model_served: 'gpt-4o', upstream_target: 'vllm-a' }),
      makeFlow({ api_call_id: 'api_open', status: 'open', model_requested: 'llama-3.1-70b', model_served: 'llama-3.1-70b', upstream_target: 'vllm-b' }),
      makeFlow({ api_call_id: 'api_fail', status: 'failed', model_requested: 'gpt-4o', model_served: 'gpt-4o', upstream_target: 'openai', terminal_reason: 'upstream 503' }),
    ]);
  });

  it('a status chip narrows the rows', () => {
    const { getByText, getByTestId, getAllByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getAllByTestId('flow-row')).toHaveLength(3);
    // Click the `open` status chip.
    fireEvent.click(getByText('open'));
    const rows = getAllByTestId('flow-row');
    expect(rows).toHaveLength(1);
    expect(within(rows[0]!).getByText('running')).toBeTruthy();
    expect(getByTestId('flow-count').textContent).toContain('1 / 3');
  });

  it('a model chip narrows the rows', () => {
    const { getAllByText, getAllByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    // `gpt-4o` appears as a model chip; clicking it keeps the two gpt-4o rows.
    const chip = getAllByText('gpt-4o').find((el) => el.tagName === 'BUTTON')!;
    fireEvent.click(chip);
    expect(getAllByTestId('flow-row')).toHaveLength(2);
  });
});

describe('FlowTable — live WS update + interactions', () => {
  it('a live flow_status patch updates the matching row in place', () => {
    seedFlows([makeFlow({ api_call_id: 'api_live', status: 'open' })]);
    const { getAllByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(within(getAllByTestId('flow-row')[0]!).getByText('running')).toBeTruthy();

    // A flow_status frame completes the flow.
    act(() => {
      dashboardStore.getState().patchFlowStatus({
        type: 'flow_status', api_call_id: 'api_live', status: 'completed',
        model_served: 'm', upstream_target: 'u', usage: null, started_ms: 1_700_000_000_000, elapsed_ms: 1200,
      });
    });
    expect(within(getAllByTestId('flow-row')[0]!).getByText('2xx')).toBeTruthy();
  });

  it('tags a failover row and reports error styling', () => {
    seedFlows([makeFlow({ api_call_id: 'api_fo', status: 'completed', model_requested: 'gpt-4o', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a' })]);
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getByTestId('failover-tag')).toBeTruthy();
  });

  it('clicking a row calls onSelect with its api_call_id', () => {
    seedFlows([makeFlow({ api_call_id: 'api_click', status: 'completed' })]);
    const onSelect = vi.fn();
    const { getAllByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={onSelect} />);
    fireEvent.click(getAllByTestId('flow-row')[0]!.querySelector('button')!);
    expect(onSelect).toHaveBeenCalledWith('api_click');
  });
});
