/**
 * The SHARED FlowTable filter (D12). The FlowTable (D10) used to own its filter in component
 * `useState`, which made it unreachable from the other views. D12's Topology (click a provider
 * node) and Sankey (click a flow band) must be able to drive that same filter — "click here →
 * see those flows" — so the filter is hoisted into this tiny vanilla-zustand store (the same
 * `createStore` + `useSyncExternalStore` bridge the dashboard/auth stores use). The FlowTable
 * reads it (it remains the ONLY writer of the chip toggles); Topology/Sankey set it, then
 * `navigate('flows')` so the cross-link lands on the table already filtered.
 *
 * It is deliberately separate from `dashboardStore` (the live WS/seek state): the filter is pure
 * UI selection that must survive view switches and is NOT cleared by a seek/teardown of the live
 * data. Keeping it out of `dashboardStore` also means a filter change never invalidates the live
 * slices' selectors.
 */
import { createStore } from 'zustand/vanilla';
import type { FlowStatus } from '../api/types';
import { EMPTY_FILTERS, type FlowFilters } from '../components/FlowTable/filterTypes';

export interface FlowFilterState {
  filters: FlowFilters;
  /** Replace the whole filter set (the FlowTable's FilterBar onChange). */
  setFilters: (next: FlowFilters) => void;
  /**
   * Toggle a single facet to a value: setting the SAME value again clears it (mirrors the
   * FilterBar chip toggle). Topology/Sankey use the `set*` helpers below, which are thin
   * wrappers over this so a second click on the same node/band un-filters.
   */
  toggle: <K extends keyof FlowFilters>(key: K, value: NonNullable<FlowFilters[K]>) => void;
  /** Cross-link from a topology node: filter to that upstream target (toggles off on repeat). */
  setUpstream: (upstream: string) => void;
  /** Cross-link from a sankey band: filter to that model (requested OR served — toggles off). */
  setModel: (model: string) => void;
  /** Cross-link convenience: filter to a status (toggles off on repeat). */
  setStatus: (status: FlowStatus) => void;
  /** Clear every facet. */
  clear: () => void;
}

export const flowFilterStore = createStore<FlowFilterState>((set) => ({
  filters: EMPTY_FILTERS,
  setFilters: (filters) => set({ filters }),
  toggle: (key, value) =>
    set((s) => ({
      // Re-selecting the active value clears the facet (chip-toggle semantics), so a second
      // click on the same node/band/chip removes the filter rather than re-applying it.
      filters: { ...s.filters, [key]: s.filters[key] === value ? null : value },
    })),
  setUpstream: (upstream) =>
    set((s) => ({ filters: { ...s.filters, upstream: s.filters.upstream === upstream ? null : upstream } })),
  setModel: (model) =>
    set((s) => ({ filters: { ...s.filters, model: s.filters.model === model ? null : model } })),
  setStatus: (status) =>
    set((s) => ({ filters: { ...s.filters, status: s.filters.status === status ? null : status } })),
  clear: () => set({ filters: EMPTY_FILTERS }),
}));

export type FlowFilterStore = typeof flowFilterStore;
