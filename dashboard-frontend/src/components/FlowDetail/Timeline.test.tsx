import { fireEvent, render, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { Timeline } from './Timeline';

const events = [
  { timestamp_ms: 1_000, kind: 'routing.selected', summary: 'provider a selected', payload_preview: '{"provider":"a"}', images: [] },
  { timestamp_ms: 1_250, kind: 'response.output_text.delta', summary: 'first token', payload_preview: 'large raw body', images: [] },
  { timestamp_ms: 2_000, kind: 'response.failed', summary: 'upstream timeout', payload_preview: '{"error":"timeout"}', images: [] },
];

describe('Timeline', () => {
  it('renders compact timing columns and mounts captured previews only when expanded', () => {
    const view = render(<Timeline events={events} startedAtMs={900} />);
    expect(view.getAllByTestId('timeline-event')).toHaveLength(3);
    expect(view.queryByText('large raw body')).toBeNull();
    expect(view.getByText('+100 ms')).toBeTruthy();
    fireEvent.click(view.getAllByRole('button', { name: 'Show captured preview' })[1]!);
    expect(view.getByText('large raw body')).toBeTruthy();
  });

  it('filters by event class and full-text search without discarding the source events', () => {
    const view = render(<Timeline events={events} startedAtMs={900} />);
    fireEvent.click(view.getByRole('button', { name: 'errors · 1' }));
    expect(view.getAllByTestId('timeline-event')).toHaveLength(1);
    expect(view.getByText('upstream timeout')).toBeTruthy();
    fireEvent.click(view.getByRole('button', { name: 'all · 3' }));
    fireEvent.change(view.getByRole('searchbox'), { target: { value: 'provider a' } });
    expect(view.getAllByTestId('timeline-event')).toHaveLength(1);
    expect(view.getByText('provider a selected')).toBeTruthy();
  });

  it('keeps summary expansion separate from raw expansion and supports copy', () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
    const view = render(<Timeline events={events} />);
    fireEvent.click(view.getByRole('button', { name: 'Expand all summaries' }));
    expect(view.getByText('severity: error')).toBeTruthy();
    expect(view.queryByText('{"error":"timeout"}')).toBeNull();
    fireEvent.click(view.getByRole('button', { name: 'Expand all raw' }));
    const failed = view.getAllByTestId('timeline-event')[2]!;
    fireEvent.click(within(failed).getByRole('button', { name: 'Copy raw' }));
    expect(writeText).toHaveBeenCalledWith('{"error":"timeout"}');
  });
});
