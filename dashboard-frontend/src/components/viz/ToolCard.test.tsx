/**
 * ToolCard (U8/R2) — pretty-printed tool payloads fold on bulk, degrade gracefully, and never
 * take the route down on pathological JSON.
 */
import { describe, expect, it } from 'vitest';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { ToolCard } from './River';

describe('ToolCard folding and crash safety', () => {
  it('folds a huge single-line string payload on bulk (chars), with a KB label', () => {
    const text = `tool output tool-1: {"data": "${'x'.repeat(50_000)}"}`;
    const { getByTestId } = render(<ToolCard text={text} />);
    const pre = getByTestId('river-tool-json');
    // Folded to the char budget, not the full 50KB.
    expect(pre.textContent!.length).toBeLessThanOrEqual(2_100);
    const fold = getByTestId('river-tool-fold');
    expect(fold.textContent).toMatch(/KB more/);
    fireEvent.click(fold);
    expect(getByTestId('river-tool-json').textContent!.length).toBeGreaterThan(49_000);
    cleanup();
  });

  it('folds by line count with a line label when lines exceed the threshold', () => {
    const value = Object.fromEntries(Array.from({ length: 30 }, (_, i) => [`k${i}`, i]));
    const { getByTestId } = render(<ToolCard text={`tool args: ${JSON.stringify(value)}`} />);
    expect(getByTestId('river-tool-fold').textContent).toMatch(/more lines/);
    cleanup();
  });

  it('renders pathologically deep JSON as plain text instead of crashing the route', () => {
    // Deep enough that JSON.parse (or a later stringify) blows the stack — the card must
    // degrade to the raw text, never throw to the error boundary.
    const depth = 200_000;
    const text = `tool args: ${'['.repeat(depth)}${']'.repeat(depth)}`;
    const { container } = render(<ToolCard text={text} />);
    expect(container.textContent).toContain('tool args:');
    cleanup();
  });

  it('renders non-JSON tool lines unchanged', () => {
    const { container, queryByTestId } = render(<ToolCard text="completed" />);
    expect(container.textContent).toBe('completed');
    expect(queryByTestId('river-tool-fold')).toBeNull();
    cleanup();
  });
});
