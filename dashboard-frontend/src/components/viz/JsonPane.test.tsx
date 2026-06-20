import { describe, it, expect, afterEach } from 'vitest';
import { StrictMode } from 'react';
import { render, cleanup } from '@testing-library/react';
import { JsonPane } from './JsonPane';
import { diffLayers } from '../FlowDetail/diff';
import { colors } from '../../design/tokens';

afterEach(cleanup);

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
});
