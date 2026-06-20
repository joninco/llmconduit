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

  it('coalesces fragmented tool-arg deltas into ONE card (finding 2)', () => {
    // A real tool call streams its arguments as MANY adjacent `tool` fragments. They must
    // accumulate into a SINGLE card (not one card per fragment), and the reassembled JSON drives
    // the tool-name label.
    const { getByTestId, queryAllByTestId } = render(
      <DeltasPanel
        segments={[
          seg('tool', '{"name":"get_'),
          seg('tool', 'weather","arg'),
          seg('tool', 'uments":{"city":"SF"}}'),
        ]}
      />,
    );
    const cards = queryAllByTestId('tool-card');
    expect(cards).toHaveLength(1); // one coalesced card, not three
    // Collapsed label resolves the tool name from the REASSEMBLED argument JSON.
    expect(cards[0]!.textContent).toContain('get_weather');
    // Expanded body shows the full accumulated argument fragments.
    fireEvent.click(cards[0]!.querySelector('button')!);
    expect(getByTestId('tool-card-body').textContent).toContain('"city":"SF"');
  });

  it('keeps DISTINCT tool runs separated by another kind as separate cards', () => {
    // A tool run, then output, then another tool run → two cards (only ADJACENT tool fragments
    // coalesce; an intervening output kind starts a fresh block).
    const { queryAllByTestId } = render(
      <DeltasPanel
        segments={[seg('tool', '{"name":"a"}'), seg('output', 'mid'), seg('tool', '{"name":"b"}')]}
      />,
    );
    expect(queryAllByTestId('tool-card')).toHaveLength(2);
  });

  it('shows the empty state with no segments', () => {
    const { getByTestId } = render(<DeltasPanel segments={[]} />);
    expect(getByTestId('deltas-empty')).toBeTruthy();
  });
});
