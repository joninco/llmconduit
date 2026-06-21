/**
 * FilterBar (D12 R5 MED) — the active model/upstream filter must ALWAYS render as a visible,
 * toggle-off-able chip even when its value matches NO row in view (a Topology/Sankey cross-link can
 * set a facet to an idle provider or an aged-out model), and a `clear` control is available whenever
 * any facet is active. Without this, the chip options (derived from rows in view) omit the orphaned
 * value, leaving an invisible filter the user cannot clear.
 */
import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, cleanup, fireEvent, within } from '@testing-library/react';
import { FilterBar } from './FilterBar';
import { EMPTY_FILTERS, type FlowFilters } from './filterTypes';

afterEach(cleanup);

function renderBar(over: Partial<Parameters<typeof FilterBar>[0]> = {}) {
  const onChange = vi.fn();
  const props = {
    filters: EMPTY_FILTERS as FlowFilters,
    models: ['gpt-4o'],
    upstreams: ['vllm-a'],
    total: 3,
    shown: 3,
    onChange,
    ...over,
  };
  const r = render(<FilterBar {...props} />);
  return { ...r, onChange };
}

/** The chips inside a labelled group (status/model/upstream). */
function groupChips(container: HTMLElement, label: string): HTMLElement[] {
  const group = within(container).getByText(label).parentElement as HTMLElement;
  return Array.from(group.querySelectorAll('button'));
}

describe('FilterBar — active filter is always visible + clearable (D12 R5 MED)', () => {
  it('renders an active chip for a SELECTED model that matches NO row in view (cross-link to an off-screen model)', () => {
    // The cross-link set model='claude-x' but the in-view rows only carry 'gpt-4o' → 'claude-x' is
    // NOT among the derived options. It must still appear as an active chip.
    const { container, onChange } = renderBar({
      models: ['gpt-4o'],
      filters: { status: null, model: 'claude-x', upstream: null },
    });
    const chip = within(container).getByText('claude-x');
    expect(chip).toBeTruthy();
    expect(chip.getAttribute('aria-pressed')).toBe('true');
    // Clicking it toggles the filter OFF (clears that facet).
    fireEvent.click(chip);
    expect(onChange).toHaveBeenCalledWith({ status: null, model: null, upstream: null });
  });

  it('renders an active chip for a SELECTED upstream that matches NO row in view', () => {
    const { container } = renderBar({
      upstreams: ['vllm-a'],
      filters: { status: null, model: null, upstream: 'idle-provider' },
    });
    const chip = within(container).getByText('idle-provider');
    expect(chip.getAttribute('aria-pressed')).toBe('true');
  });

  it('shows the orphaned active value even when the derived option list is EMPTY (no rows in view)', () => {
    const { container } = renderBar({
      models: [],
      upstreams: [],
      filters: { status: null, model: 'claude-x', upstream: null },
      total: 0,
      shown: 0,
    });
    // The model group renders solely because of the active selection.
    expect(groupChips(container, 'model').map((b) => b.textContent)).toEqual(['claude-x']);
  });

  it('does not duplicate a selected value that is ALSO in the derived options', () => {
    const { container } = renderBar({
      models: ['gpt-4o', 'claude-x'],
      filters: { status: null, model: 'claude-x', upstream: null },
    });
    const labels = groupChips(container, 'model').map((b) => b.textContent);
    expect(labels.filter((l) => l === 'claude-x')).toHaveLength(1);
  });

  it('exposes a `clear` control when any facet is active that resets ALL facets', () => {
    const { container, onChange } = renderBar({
      filters: { status: 'open', model: 'gpt-4o', upstream: 'vllm-a' },
    });
    const clear = within(container).getByTestId('flow-filter-clear');
    fireEvent.click(clear);
    expect(onChange).toHaveBeenCalledWith(EMPTY_FILTERS);
  });

  it('hides the `clear` control when no facet is active', () => {
    const { container } = renderBar({ filters: EMPTY_FILTERS });
    expect(within(container).queryByTestId('flow-filter-clear')).toBeNull();
  });
});
