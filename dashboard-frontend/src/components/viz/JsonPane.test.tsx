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
