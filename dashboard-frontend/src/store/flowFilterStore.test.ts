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

  it('setUpstream SETS the target deterministically (a click filters; no toggle-off) — finding 10', () => {
    flowFilterStore.getState().setUpstream('vllm-a');
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
    // A cross-link is "click → SEE those flows": a repeat re-SETS the same value, never clears it.
    flowFilterStore.getState().setUpstream('vllm-a');
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-a');
    // A different target replaces it.
    flowFilterStore.getState().setUpstream('vllm-b');
    expect(flowFilterStore.getState().filters.upstream).toBe('vllm-b');
  });

  it('setModel / setUpstream set their facets independently', () => {
    flowFilterStore.getState().setModel('gpt-4o');
    flowFilterStore.getState().setUpstream('vllm-a');
    expect(flowFilterStore.getState().filters).toEqual({ status: null, model: 'gpt-4o', upstream: 'vllm-a' });
  });

  it('setFilters replaces the whole set; clear resets', () => {
    flowFilterStore.getState().setFilters({ status: 'open', model: 'm', upstream: 'u' });
    expect(flowFilterStore.getState().filters).toEqual({ status: 'open', model: 'm', upstream: 'u' });
    flowFilterStore.getState().clear();
    expect(flowFilterStore.getState().filters).toEqual(EMPTY_FILTERS);
  });
});
