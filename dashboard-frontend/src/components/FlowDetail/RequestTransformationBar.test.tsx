import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render } from '@testing-library/react';
import { RequestTransformationBar } from './RequestTransformationBar';

afterEach(cleanup);

const normalization = {
  label: 'normalize',
  available: true,
  summary: { added: 2, changed: 1, removed: 3, total: 6 },
};
const lowering = {
  label: 'lower',
  available: true,
  summary: { added: 1, changed: 2, removed: 0, total: 3 },
};

describe('RequestTransformationBar', () => {
  it('names all stages and exposes operation counts without relying on color', () => {
    const { getByTestId, getByText } = render(
      <RequestTransformationBar
        query=""
        onQueryChange={() => {}}
        mode="changes"
        onModeChange={() => {}}
        normalization={normalization}
        lowering={lowering}
      />,
    );
    expect(getByText('Client payload')).toBeTruthy();
    expect(getByText('Gateway canonical')).toBeTruthy();
    expect(getByText('Provider payload')).toBeTruthy();
    expect(getByTestId('request-stage-rail').tabIndex).toBe(0);
    expect(getByTestId('request-stage-rail').getAttribute('aria-label')).toBe('Request transformation stages');
    expect(getByTestId('request-hop-normalize').getAttribute('aria-label')).toBe(
      'normalize: 2 introduced, 1 rewritten, 3 omitted',
    );
    expect(getByTestId('request-hop-lower').getAttribute('aria-label')).toBe(
      'lower: 1 introduced, 2 rewritten, 0 omitted',
    );
    expect(getByTestId('request-hop-normalize').textContent).toContain('+2 new');
    expect(getByTestId('request-hop-normalize').textContent).toContain('~1 rewrite');
    expect(getByTestId('request-hop-normalize').textContent).toContain('−3 drop');
    expect(getByTestId('request-view-changes').textContent).toContain('9');
  });

  it('switches representation mode and preserves shared search controls', () => {
    const onModeChange = vi.fn();
    const onQueryChange = vi.fn();
    const { getByTestId, getByRole } = render(
      <RequestTransformationBar
        query="model"
        onQueryChange={onQueryChange}
        mode="changes"
        onModeChange={onModeChange}
        normalization={normalization}
        lowering={{ label: 'lower', available: false, summary: { added: 0, changed: 0, removed: 0, total: 0 } }}
      />,
    );
    fireEvent.click(getByTestId('request-view-all'));
    expect(onModeChange).toHaveBeenCalledWith('all');
    fireEvent.change(getByTestId('json-search-input'), { target: { value: 'tools' } });
    expect(onQueryChange).toHaveBeenCalledWith('tools');
    fireEvent.click(getByRole('button', { name: 'clear search' }));
    expect(onQueryChange).toHaveBeenCalledWith('');
    expect(getByTestId('request-hop-lower').textContent).toContain('capture unavailable');
  });
});
