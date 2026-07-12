import { describe, it, expect, afterEach, vi } from 'vitest';
import { createRef, StrictMode } from 'react';
import { render, cleanup, fireEvent, waitFor } from '@testing-library/react';
import hljs from 'highlight.js/lib/core';
import { JsonPane, JSON_RENDER_LINE_CAP } from './JsonPane';
import { describeChanges, diffLayers } from '../FlowDetail/diff';

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('JsonPane — highlight.js JSON + explicit transformation annotations', () => {
  it('renders one highlighted line per JSON line and highlight.js tokens', () => {
    const { getByTestId } = render(<JsonPane label="A" value={{ model: 'gpt-4o', stream: true }} />);
    const code = getByTestId('jsonpane-code-A');
    const lines = code.querySelectorAll('.json-line');
    // { , "model": ..., "stream": ..., } = 4 lines.
    expect(lines).toHaveLength(4);
    // highlight.js wrapped at least one token in an hljs span (syntax coloring applied).
    expect(code.querySelectorAll('span[class^="hljs-"]').length).toBeGreaterThan(0);
  });

  it('explains a rewritten value on the RIGHT pane with its prior value', () => {
    const inbound = { model: 'gpt-4o', temperature: 0.7, messages: [{ role: 'user', content: 'Hi' }] };
    const normalized = { model: 'llama-3.1-70b', messages: [{ role: 'user', content: 'Hi' }] };
    const diff = diffLayers(inbound, normalized);
    const { getByTestId, getByLabelText } = render(
      <JsonPane label="B" value={normalized} diff={diff} incomingChanges={describeChanges(inbound, normalized)} side="right" />,
    );
    const code = getByTestId('jsonpane-code-B');
    const modelLine = code.querySelector('.json-line[data-path="$.model"]') as HTMLElement;
    expect(modelLine?.dataset.diff).toBe('changed');
    expect(modelLine?.dataset.operation).toBe('rewritten');
    expect(getByLabelText('was "gpt-4o"')).toBeTruthy();
    // The redesign does not rely on a red/green background to carry the meaning.
    expect(modelLine?.style.backgroundColor).toBe('');
  });

  it('labels a removed path on the LEFT pane with its destination', () => {
    const inbound = { model: 'gpt-4o', temperature: 0.7 };
    const normalized = { model: 'gpt-4o' };
    const diff = diffLayers(inbound, normalized);
    const { getByTestId, getByLabelText } = render(
      <JsonPane
        label="A"
        stage={{ step: 'A', title: 'Client payload', subtitle: 'captured at ingress', nextLabel: 'canonical' }}
        value={inbound}
        diff={diff}
        outgoingChanges={describeChanges(inbound, normalized)}
        side="left"
      />,
    );
    const code = getByTestId('jsonpane-code-A');
    const tempLine = code.querySelector('.json-line[data-path="$.temperature"]') as HTMLElement;
    expect(tempLine?.dataset.diff).toBe('removed');
    expect(tempLine?.dataset.operation).toBe('omitted');
    expect(getByLabelText('not in canonical')).toBeTruthy();
  });

  it('retains descendant classifications for diagnostics while annotating the operation root once', () => {
    const left = { model: 'x' };
    const right = { model: 'x', tools: [{ name: 'search' }] };
    const diff = diffLayers(left, right);
    const { getByTestId, getAllByLabelText } = render(
      <JsonPane label="C" value={right} diff={diff} incomingChanges={describeChanges(left, right)} side="right" />,
    );
    const code = getByTestId('jsonpane-code-C');
    // The structural map remains descendant-aware, but the visible explanation is one operation
    // at the subtree root — “tools added”, not a badge on every tool-schema line.
    expect((code.querySelector('.json-line[data-path="$.tools"]') as HTMLElement)?.dataset.diff).toBe('added');
    expect((code.querySelector('.json-line[data-path="$.tools[0]"]') as HTMLElement)?.dataset.diff).toBe('added');
    expect((code.querySelector('.json-line[data-path="$.tools[0].name"]') as HTMLElement)?.dataset.diff).toBe('added');
    expect(getAllByLabelText('introduced here')).toHaveLength(1);
  });

  it('renders BOTH explicit operations when B introduces a field and drops it before C', () => {
    const a = {};
    const b = { b_only: 1 };
    const c = {};
    const diff = new Map([['$.b_only', 'added-removed' as const]]);
    const { getByTestId, getByLabelText } = render(
      <JsonPane
        label="B"
        stage={{ step: 'B', title: 'Gateway canonical', subtitle: 'Responses protocol', nextLabel: 'upstream' }}
        value={b}
        diff={diff}
        incomingChanges={describeChanges(a, b)}
        outgoingChanges={describeChanges(b, c)}
        side="both"
      />,
    );
    const code = getByTestId('jsonpane-code-B');
    const line = code.querySelector('.json-line[data-path="$.b_only"]') as HTMLElement;
    expect(line?.dataset.diff).toBe('added-removed');
    expect(line?.dataset.operation).toBe('introduced omitted');
    expect(getByLabelText('introduced here')).toBeTruthy();
    expect(getByLabelText('not sent upstream')).toBeTruthy();
  });

  it('changes-only mode shows operation roots + ancestors and folds a large new subtree', () => {
    const left = { model: 'x', keep: 'unchanged' };
    const right = { model: 'y', keep: 'unchanged', tools: [{ name: 'search' }, { name: 'fetch' }] };
    const { getByTestId, queryByText, rerender } = render(
      <JsonPane
        label="focused"
        value={right}
        diff={diffLayers(left, right)}
        incomingChanges={describeChanges(left, right)}
        changesOnly
      />,
    );
    const code = getByTestId('jsonpane-code-focused');
    expect(code.querySelector('.json-line[data-path="$.model"]')).toBeTruthy();
    expect(code.querySelector('.json-line[data-path="$.keep"]')).toBeNull();
    const tools = code.querySelector('.json-line[data-path="$.tools"]');
    expect(tools?.textContent).toContain('… ]');
    expect(tools?.textContent).toContain('2');
    expect(code.querySelector('.json-line[data-path="$.tools[0].name"]')).toBeNull();
    expect(getByTestId('jsonpane-change-count-focused').textContent).toContain('2 ops');

    // Search intentionally overrides the compact filter and scans the complete representation.
    rerender(
      <JsonPane
        label="focused"
        value={right}
        diff={diffLayers(left, right)}
        incomingChanges={describeChanges(left, right)}
        changesOnly
        query="unchanged"
      />,
    );
    expect(queryByText(/unchanged/)).toBeTruthy();
  });

  it('states explicitly when a stage has no operations in changes-only mode', () => {
    const value = { model: 'same' };
    const { getByTestId, queryByTestId } = render(
      <JsonPane label="same" value={value} incomingChanges={describeChanges(value, value)} changesOnly />,
    );
    expect(getByTestId('jsonpane-no-changes-same').textContent).toContain('no operations');
    expect(queryByTestId('jsonpane-code-same')).toBeNull();
  });

  it('shows the evicted placeholder (not undefined) when the body is absent', () => {
    const { getByTestId, queryByTestId } = render(<JsonPane label="C" value={undefined} emptyLabel="body evicted" />);
    expect(getByTestId('jsonpane-empty-C').textContent).toBe('body evicted');
    expect(queryByTestId('jsonpane-code-C')).toBeNull();
  });

  it('StrictMode double-invoke leaves exactly ONE set of highlighted lines (no leak)', () => {
    const { container } = render(
      <StrictMode>
        <JsonPane label="A" value={{ a: 1, b: 2 }} />
      </StrictMode>,
    );
    // The imperative build cleans up on the discarded first mount; only one pane's lines survive.
    const codes = container.querySelectorAll('[data-testid="jsonpane-code-A"]');
    expect(codes).toHaveLength(1);
    expect(codes[0]!.querySelectorAll('.json-line')).toHaveLength(4);
  });

  it('mounts and highlights only a viewport slice, then renders diff-marked tail rows on scroll', async () => {
    const highlightSpy = vi.spyOn(hljs, 'highlight');
    const value = Array.from({ length: 5_000 }, (_, index) => `value-${index}`);
    const diff = new Map([['$[4999]', 'changed' as const]]);
    const { getByTestId } = render(<JsonPane label="large" value={value} diff={diff} side="right" />);
    const code = getByTestId('jsonpane-code-large');
    const scroll = getByTestId('jsonpane-scroll-large');
    expect(code.getAttribute('data-total-lines')).toBe('5002');

    const initiallyMounted = code.querySelectorAll('.json-line');
    expect(initiallyMounted.length).toBeGreaterThan(0);
    expect(initiallyMounted.length).toBeLessThan(100);
    expect(highlightSpy.mock.calls.length).toBeLessThan(100);
    expect(code.querySelectorAll('span[class^="hljs-"]').length).toBeGreaterThan(0);
    expect(code.querySelector('.json-line[data-path="$[4999]"]')).toBeNull();

    scroll.scrollTop = 5_002 * 20;
    fireEvent.scroll(scroll);
    await waitFor(() => {
      const tail = code.querySelector('.json-line[data-path="$[4999]"]') as HTMLElement | null;
      expect(tail?.dataset.diff).toBe('changed');
    });
    expect(code.querySelectorAll('.json-line').length).toBeLessThan(100);
  });

  it('preserves tail search matches and folding while virtualized', async () => {
    const value = Array.from({ length: 500 }, (_, index) => ({
      id: index,
      message: index === 499 ? 'tail needle' : `ordinary ${index}`,
    }));
    const { getByTestId, getByRole, rerender } = render(<JsonPane label="search" value={value} />);
    const code = getByTestId('jsonpane-code-search');

    fireEvent.click(getByRole('button', { name: 'collapse $' }));
    await waitFor(() => expect(code.querySelectorAll('.json-line')).toHaveLength(1));
    expect(code.textContent).toContain('500');

    rerender(<JsonPane label="search" value={value} query="tail needle" />);
    await waitFor(() => {
      expect(code.querySelector('.json-line[data-path="$[499].message"]')).toBeTruthy();
    });
    expect(getByTestId('jsonpane-matches-search').textContent).toBe('1');
  });

  it('keeps the external scroll ref/handler contract used by pane scroll-sync', () => {
    const scrollRef = createRef<HTMLDivElement>();
    const onScroll = vi.fn();
    const { getByTestId } = render(
      <JsonPane label="sync" value={{ a: 1, b: 2 }} scrollRef={scrollRef} onScroll={onScroll} />,
    );
    const scroll = getByTestId('jsonpane-scroll-sync');
    expect(scrollRef.current).toBe(scroll);
    expect(scroll.tabIndex).toBe(0);
    expect(scroll.getAttribute('aria-label')).toBe('sync JSON');
    scroll.scrollTop = 20;
    fireEvent.scroll(scroll);
    expect(onScroll).toHaveBeenCalledTimes(1);
  });

  it('caps an oversized expanded surface explicitly without hiding a tail search match', async () => {
    const value = Array.from({ length: JSON_RENDER_LINE_CAP + 50 }, (_, index) =>
      index === JSON_RENDER_LINE_CAP + 49 ? 'unique tail target' : index,
    );
    const { getByTestId, rerender } = render(<JsonPane label="capped" value={value} />);
    const code = getByTestId('jsonpane-code-capped');
    const marker = getByTestId('jsonpane-render-cap-capped');
    expect(code.getAttribute('data-render-lines')).toBe(String(JSON_RENDER_LINE_CAP));
    expect(marker.textContent).toContain('52 omitted');
    expect(code.querySelectorAll('.json-line').length).toBeLessThan(100);

    // Search is computed against the complete line model BEFORE the render cap, so an operator can
    // still find a value that was outside the expanded view's first 10k lines.
    rerender(<JsonPane label="capped" value={value} query="unique tail target" />);
    await waitFor(() => {
      expect(code.querySelector(`.json-line[data-path="$[${JSON_RENDER_LINE_CAP + 49}]"]`)).toBeTruthy();
    });
    expect(getByTestId('jsonpane-matches-capped').textContent).toBe('1');
  });
});
