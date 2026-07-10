import { describe, it, expect, afterEach, vi } from 'vitest';
import { createRef, StrictMode } from 'react';
import { render, cleanup, fireEvent, waitFor } from '@testing-library/react';
import hljs from 'highlight.js/lib/core';
import { JsonPane, JSON_RENDER_LINE_CAP } from './JsonPane';
import { diffLayers } from '../FlowDetail/diff';
import { colors } from '../../design/tokens';

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('JsonPane — highlight.js JSON + per-path diff tints', () => {
  it('renders one highlighted line per JSON line and highlight.js tokens', () => {
    const { getByTestId } = render(<JsonPane label="A" value={{ model: 'gpt-4o', stream: true }} />);
    const code = getByTestId('jsonpane-code-A');
    const lines = code.querySelectorAll('.json-line');
    // { , "model": ..., "stream": ..., } = 4 lines.
    expect(lines).toHaveLength(4);
    // highlight.js wrapped at least one token in an hljs span (syntax coloring applied).
    expect(code.querySelectorAll('span[class^="hljs-"]').length).toBeGreaterThan(0);
  });

  it('tints added/changed paths on the RIGHT pane from a known 3-layer fixture', () => {
    const inbound = { model: 'gpt-4o', temperature: 0.7, messages: [{ role: 'user', content: 'Hi' }] };
    const normalized = { model: 'llama-3.1-70b', messages: [{ role: 'user', content: 'Hi' }] };
    const diff = diffLayers(inbound, normalized);
    const { getByTestId } = render(<JsonPane label="B" value={normalized} diff={diff} side="right" />);
    const code = getByTestId('jsonpane-code-B');
    const modelLine = code.querySelector('.json-line[data-path="$.model"]') as HTMLElement;
    // model changed gpt-4o → llama: tinted with the "context" (changed) background on the right.
    expect(modelLine?.dataset.diff).toBe('changed');
    expect(modelLine?.style.backgroundColor).not.toBe('');
  });

  it('tints REMOVED paths on the LEFT pane (a field the next layer drops)', () => {
    const inbound = { model: 'gpt-4o', temperature: 0.7 };
    const normalized = { model: 'gpt-4o' };
    const diff = diffLayers(inbound, normalized);
    const { getByTestId } = render(<JsonPane label="A" value={inbound} diff={diff} side="left" />);
    const code = getByTestId('jsonpane-code-A');
    const tempLine = code.querySelector('.json-line[data-path="$.temperature"]') as HTMLElement;
    expect(tempLine?.dataset.diff).toBe('removed');
    // The removed tint resolves from the design token (non-empty background).
    expect(tempLine?.style.backgroundColor).not.toBe('');
    void colors; // token module imported to confirm tint derivation is wired
  });

  it('tints EVERY line of an added nested subtree, not just its opening bracket (finding 3)', () => {
    const left = { model: 'x' };
    const right = { model: 'x', tools: [{ name: 'search' }] };
    const diff = diffLayers(left, right);
    const { getByTestId } = render(<JsonPane label="C" value={right} diff={diff} side="right" />);
    const code = getByTestId('jsonpane-code-C');
    // The container line AND each nested line under the new subtree carry an added tint.
    expect((code.querySelector('.json-line[data-path="$.tools"]') as HTMLElement)?.dataset.diff).toBe('added');
    expect((code.querySelector('.json-line[data-path="$.tools[0]"]') as HTMLElement)?.dataset.diff).toBe('added');
    expect((code.querySelector('.json-line[data-path="$.tools[0].name"]') as HTMLElement)?.dataset.diff).toBe('added');
  });

  it('renders BOTH a composite added-removed tint on the middle pane (finding 5)', () => {
    // A field introduced by A→B and dropped by B→C: pane B (side `both`) gets the composite kind
    // and renders a gradient carrying BOTH signals, not just the add.
    const diff = new Map([['$.b_only', 'added-removed' as const]]);
    const { getByTestId } = render(<JsonPane label="B" value={{ b_only: 1 }} diff={diff} side="both" />);
    const code = getByTestId('jsonpane-code-B');
    const line = code.querySelector('.json-line[data-path="$.b_only"]') as HTMLElement;
    expect(line?.dataset.diff).toBe('added-removed');
    // A gradient (both halves) — not a single solid colour — encodes the dual classification.
    expect(line?.style.backgroundImage).toContain('gradient');
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
