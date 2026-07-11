import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { act, cleanup, fireEvent, waitFor, within } from '@testing-library/react';
import { FlowTable } from './FlowTable';
import { dashboardStore } from '../../store/dashboardStore';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from '../testHarness';
import { getConnection } from '../../api/connection';
import { flowFilterStore } from '../../store/flowFilterStore';

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

function useNarrowViewport(): void {
  vi.stubGlobal('matchMedia', (query: string) => ({
    matches: query === '(max-width: 1023px)',
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent: () => false,
  }));
}

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

  it('uses fixed-height virtualized cards below lg without rendering the desktop grid', () => {
    useNarrowViewport();
    const flows = Array.from({ length: 10_000 }, (_, i) =>
      makeFlow({ api_call_id: `api_mobile_${String(i).padStart(5, '0')}`, started_ms: 1_700_000_000_000 + i }),
    );
    seedFlows(flows);

    const { getByRole, getAllByTestId, queryByRole } = renderWithQuery(
      <FlowTable selectedId={null} onSelect={noop} />,
    );

    expect(getByRole('list', { name: 'Flows' })).toBeTruthy();
    expect(queryByRole('grid', { name: 'Flows' })).toBeNull();
    expect(queryByRole('columnheader')).toBeNull();
    const cards = getAllByTestId('flow-card');
    expect(cards.length).toBeGreaterThan(0);
    expect(cards.length).toBeLessThan(200);
    expect(getAllByTestId('flow-row')[0]?.style.height).toBe('144px');
  });
});

describe('FlowTable — desktop ARIA grid keyboard model', () => {
  function renderGrid(onSelect = vi.fn()) {
    seedFlows([
      makeFlow({ api_call_id: 'api_keyboard_1', started_ms: 1_700_000_000_003 }),
      makeFlow({ api_call_id: 'api_keyboard_2', started_ms: 1_700_000_000_002 }),
      makeFlow({ api_call_id: 'api_keyboard_3', started_ms: 1_700_000_000_001 }),
    ]);
    const rendered = renderWithQuery(<FlowTable selectedId={null} onSelect={onSelect} />);
    const grid = rendered.getByRole('grid', { name: 'Flows' });
    const rows = within(grid)
      .getAllByRole('row')
      .filter((row) => row.tagName === 'BUTTON') as HTMLButtonElement[];
    return { ...rendered, grid, rows, onSelect };
  }

  it('exposes headers, grid cells, total row count, and one-based ARIA row indexes', () => {
    const { grid, rows } = renderGrid();
    expect(grid.getAttribute('aria-colcount')).toBe('10');
    expect(grid.getAttribute('aria-rowcount')).toBe('4');
    expect(within(grid).getAllByRole('columnheader')).toHaveLength(10);
    expect(rows).toHaveLength(3);
    expect(rows.map((row) => row.getAttribute('aria-rowindex'))).toEqual(['2', '3', '4']);
    rows.forEach((row) => expect(within(row).getAllByRole('gridcell')).toHaveLength(10));
  });

  it('roves one row tab stop with arrows/Home/End and activates with Enter/Space', () => {
    const { rows, onSelect } = renderGrid();
    const [first, second, third] = rows;
    expect(first?.tabIndex).toBe(0);
    expect(second?.tabIndex).toBe(-1);
    expect(third?.tabIndex).toBe(-1);

    act(() => first?.focus());
    fireEvent.keyDown(first!, { key: 'ArrowDown' });
    expect(document.activeElement).toBe(second);
    expect(second?.tabIndex).toBe(0);
    expect(first?.tabIndex).toBe(-1);

    fireEvent.keyDown(second!, { key: 'End' });
    expect(document.activeElement).toBe(third);
    fireEvent.keyDown(third!, { key: 'Home' });
    expect(document.activeElement).toBe(first);
    fireEvent.keyDown(first!, { key: 'ArrowRight' });
    expect(document.activeElement).toBe(second);
    fireEvent.keyDown(second!, { key: 'ArrowLeft' });
    expect(document.activeElement).toBe(first);

    fireEvent.keyDown(first!, { key: 'Enter' });
    fireEvent.keyDown(first!, { key: ' ' });
    expect(onSelect).toHaveBeenNthCalledWith(1, first?.title);
    expect(onSelect).toHaveBeenNthCalledWith(2, first?.title);
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
        ...makeFlow({
          revision: 2,
          api_call_id: 'api_live',
          status: 'completed',
          model_served: 'm',
          upstream_target: 'u',
          started_ms: 1_700_000_000_000,
          elapsed_ms: 1200,
        }),
        type: 'flow_status',
        phase: 'terminal',
        usage: null,
        cost: null,
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

  // Gap 07 (review round 2): the per-flow cost cell consumes `cost_confidence`, so an estimated row
  // is visually distinct from a confident one and an unavailable one renders `—` (never `$0.00`).
  it('a confident cost renders plain dollars with NO est marker', () => {
    seedFlows([makeFlow({ api_call_id: 'api_conf', status: 'completed', cost: 0.0061, cost_confidence: 'confident' })]);
    const { getByTestId, queryByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getByTestId('flow-cost').textContent).toBe('$0.0061');
    expect(getByTestId('flow-cost').getAttribute('data-confidence')).toBe('confident');
    expect(queryByTestId('flow-cost-est')).toBeNull();
  });

  it('an estimated cost is LABELLED with an est marker', () => {
    seedFlows([makeFlow({ api_call_id: 'api_est', status: 'completed', cost: 0.0019, cost_confidence: 'estimated' })]);
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getByTestId('flow-cost').textContent).toBe('$0.0019');
    expect(getByTestId('flow-cost-est')).toBeTruthy();
    expect(getByTestId('flow-cost').getAttribute('data-confidence')).toBe('estimated');
  });

  it('an unavailable cost renders — (never $0.00) and no est marker', () => {
    // The default makeFlow row is unpriced (cost_confidence unavailable, no cost) — it must read `—`.
    seedFlows([makeFlow({ api_call_id: 'api_unp', status: 'failed', cost: null, cost_confidence: 'unavailable' })]);
    const { getByTestId, queryByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getByTestId('flow-cost').textContent).toBe('—');
    expect(queryByTestId('flow-cost-est')).toBeNull();
  });

  it('the client column does NOT mislabel the HTTP method; renders "—" when absent (finding 6 / gap 15 don\'t-lie-with-zeros)', () => {
    // No client attribution (no key/configured-id/UA) ⇒ the client cell is the honest unavailable
    // marker — NOT the request method (POST), and NOT a fabricated id.
    seedFlows([makeFlow({ api_call_id: 'api_client', method: 'POST', status: 'completed', client_label: null })]);
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    const cell = getByTestId('flow-client');
    expect(cell.textContent).toBe('—');
    expect(cell.textContent).not.toBe('POST');
    expect(cell.getAttribute('data-quality')).toBe('unavailable');
    expect(cell.getAttribute('data-attributed')).toBe('false');
  });

  // Gap 15: the CLIENT column renders the non-secret attribution label with a source-strength marker.
  it('renders a key-hash client as a STRONG measured identity (label + key badge) — gap 15', () => {
    seedFlows([makeFlow({ api_call_id: 'api_kh', status: 'completed', client_label: 'key-9f3a1c0b2d4e', client_source: 'key_hash' })]);
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    const cell = getByTestId('flow-client');
    expect(cell.textContent).toContain('key-9f3a1c0b2d4e'); // the hash prefix — never a raw key
    expect(cell.getAttribute('data-quality')).toBe('measured');
    expect(cell.getAttribute('data-strength')).toBe('strong');
    expect(getByTestId('flow-client-source').textContent).toBe('key');
  });

  it('renders a User-Agent client as a WEAK derived fallback (visibly weaker, ua badge) — gap 15', () => {
    seedFlows([makeFlow({ api_call_id: 'api_ua', status: 'completed', client_label: 'python-httpx/0.27', client_source: 'user_agent' })]);
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    const cell = getByTestId('flow-client');
    expect(cell.textContent).toContain('python-httpx/0.27');
    // The KEY distinction: a UA fallback is `derived` (weak), NOT `measured` — never a confirmed identity.
    expect(cell.getAttribute('data-quality')).toBe('derived');
    expect(cell.getAttribute('data-strength')).toBe('weak');
    const badge = getByTestId('flow-client-source');
    expect(badge.textContent).toBe('ua');
    expect(badge.getAttribute('data-source')).toBe('user_agent');
  });

  // Gap 15: the per-client filter chip narrows the table to one client_label.
  it('a client filter chip narrows the rows to that client (gap 15)', () => {
    seedFlows([
      makeFlow({ api_call_id: 'api_x1', status: 'completed', client_label: 'key-A', client_source: 'key_hash' }),
      makeFlow({ api_call_id: 'api_x2', status: 'completed', client_label: 'key-A', client_source: 'key_hash' }),
      makeFlow({ api_call_id: 'api_y1', status: 'completed', client_label: 'svc-checkout', client_source: 'configured_header' }),
    ]);
    const { getAllByTestId, getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getAllByTestId('flow-row')).toHaveLength(3);
    // `key-A` appears as a client filter chip (its label is in a bounded truncate span); resolve the
    // enclosing chip button via the filter-bar chip-label testid (the CLIENT cell also renders `key-A`).
    const chip = getAllByTestId('flow-filter-chip-label')
      .find((el) => el.textContent === 'key-A')!
      .closest('button')!;
    fireEvent.click(chip);
    expect(getAllByTestId('flow-row')).toHaveLength(2);
    expect(getByTestId('flow-count').textContent).toContain('2 / 3');
  });
});

describe('FlowTable — loading, failure, empty, and filtered-empty states', () => {
  it('shows loading before either REST or live data establishes a row set', () => {
    vi.stubGlobal('fetch', vi.fn(() => new Promise<Response>(() => {})));
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    expect(getByTestId('flow-table-loading').textContent).toContain('Loading flows');
  });

  it('shows a retryable transport failure instead of calling it an empty result', async () => {
    const fetchMock = vi.fn(async () => new Response('', { status: 503 }));
    vi.stubGlobal('fetch', fetchMock);
    getConnection().queryClient.setDefaultOptions({ queries: { retry: false } });
    const { getByTestId, getByRole } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);

    await waitFor(() => expect(getByTestId('flow-table-error')).toBeTruthy());
    expect(getByTestId('flow-table-error').textContent).not.toContain('No flows');
    const calls = fetchMock.mock.calls.length;
    fireEvent.click(getByRole('button', { name: 'Retry' }));
    await waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThan(calls));
  });

  it('distinguishes an honest unfiltered empty result from filtered-empty', async () => {
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const body = url.includes('/catalog') ? [] : { flows: [], total: 0, flow_seq: 0 };
      return new Response(JSON.stringify(body), {
        status: 200,
        headers: {
          'Content-Type': 'application/json',
          'X-LLMConduit-Dashboard-Schema': '5',
        },
      });
    }));
    const { getByTestId } = renderWithQuery(<FlowTable selectedId={null} onSelect={noop} />);
    await waitFor(() => expect(getByTestId('flow-table-empty').textContent).toContain('No flows yet'));

    act(() => {
      seedFlows([makeFlow({ api_call_id: 'only-complete', status: 'completed' })]);
      flowFilterStore.getState().setFilters({ status: 'failed', model: null, upstream: null, client: null });
    });
    await waitFor(() => expect(getByTestId('flow-table-filtered-empty').textContent).toContain('No flows match'));
  });
});
