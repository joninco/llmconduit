import { describe, it, expect, beforeEach } from 'vitest';
import { flowFilterStore } from './flowFilterStore';
import { EMPTY_FILTERS } from '../components/FlowTable/filterTypes';

describe('flowFilterStore — the shared FlowTable filter (D12 cross-link)', () => {
  beforeEach(() => {
    flowFilterStore.getState().clear();
  });

  it('starts empty', () => {
    expect(flowFilterStore.getState().filters).toEqual(EMPTY_FILTERS);
  });

  it('setUpstream filters to a target and toggles it off on a repeat', () => {
    flowFilterStore.getState().setUpstream('vllm-a');
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
    // Re-selecting the same upstream clears it (chip-toggle semantics).
    flowFilterStore.getState().setUpstream('vllm-a');
    expect(flowFilterStore.getState().filters.upstream).toBeNull();
  });

  it('setModel / setStatus set their facets independently', () => {
    flowFilterStore.getState().setModel('gpt-4o');
    flowFilterStore.getState().setStatus('failed');
    expect(flowFilterStore.getState().filters).toEqual({ status: 'failed', model: 'gpt-4o', upstream: null });
  });

  it('setFilters replaces the whole set; clear resets', () => {
    flowFilterStore.getState().setFilters({ status: 'open', model: 'm', upstream: 'u' });
    expect(flowFilterStore.getState().filters).toEqual({ status: 'open', model: 'm', upstream: 'u' });
    flowFilterStore.getState().clear();
    expect(flowFilterStore.getState().filters).toEqual(EMPTY_FILTERS);
  });
});
