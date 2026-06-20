import { describe, it, expect, afterEach } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/react';
import type { DebugSegment } from '../../api/types';
import { DeltasPanel } from './DeltasPanel';

afterEach(cleanup);

function seg(kind: DebugSegment['kind'], text: string, ts = 1): DebugSegment {
  return { timestamp_ms: ts, kind, text };
}

describe('DeltasPanel — output/reasoning/tool from segment_append', () => {
  it('renders output bright and reasoning dim, coalescing adjacent runs', () => {
    const { queryAllByTestId, getByTestId } = render(
      <DeltasPanel segments={[seg('output', 'Hello'), seg('output', ', world'), seg('reasoning', 'thinking…')]} />,
    );
    void getByTestId('deltas-panel');
    const out = document.querySelector('[data-segment-kind="output"]');
    const reason = document.querySelector('[data-segment-kind="reasoning"]');
    // Adjacent output segments coalesced into one block.
    expect(out?.textContent).toBe('Hello, world');
    expect(reason?.textContent).toBe('thinking…');
    // Reasoning block is styled dim/italic (distinct from output).
    expect(reason?.className).toContain('italic');
    expect(queryAllByTestId('tool-card')).toHaveLength(0);
  });

  it('renders a tool segment as an EXPANDABLE card (collapsed → expanded payload)', () => {
    const payload = JSON.stringify({ name: 'get_weather', args: { city: 'SF' } });
    const { getByTestId, queryByTestId } = render(<DeltasPanel segments={[seg('tool', payload)]} />);
    const card = getByTestId('tool-card');
    // Collapsed: the label shows the tool name; the body is not rendered.
    expect(card.textContent).toContain('get_weather');
    expect(queryByTestId('tool-card-body')).toBeNull();
    // Expand.
    fireEvent.click(card.querySelector('button')!);
    expect(getByTestId('tool-card-body').textContent).toContain('get_weather');
  });

  it('shows the empty state with no segments', () => {
    const { getByTestId } = render(<DeltasPanel segments={[]} />);
    expect(getByTestId('deltas-empty')).toBeTruthy();
  });
});
