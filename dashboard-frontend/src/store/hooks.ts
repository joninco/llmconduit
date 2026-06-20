/**
 * React 18 bridges for the vanilla zustand stores. We use the store's own
 * `subscribe`/`getState` with React's `useSyncExternalStore` so concurrent rendering
 * stays tear-free (D9 §"useSyncExternalStore bridges zustand → React 18 concurrent").
 */
import { useSyncExternalStore } from 'react';
import { dashboardStore, type DashboardState } from './dashboardStore';
import { authStore, type AuthState } from './authStore';
import { flowFilterStore, type FlowFilterState } from './flowFilterStore';

/** Subscribe to a slice of the dashboard store. */
export function useDashboard<T>(selector: (s: DashboardState) => T): T {
  return useSyncExternalStore(
    dashboardStore.subscribe,
    () => selector(dashboardStore.getState()),
    () => selector(dashboardStore.getState()),
  );
}

/** Subscribe to a slice of the auth store. */
export function useAuth<T>(selector: (s: AuthState) => T): T {
  return useSyncExternalStore(
    authStore.subscribe,
    () => selector(authStore.getState()),
    () => selector(authStore.getState()),
  );
}

/** Subscribe to a slice of the SHARED FlowTable filter store (D12 cross-link). */
export function useFlowFilter<T>(selector: (s: FlowFilterState) => T): T {
  return useSyncExternalStore(
    flowFilterStore.subscribe,
    () => selector(flowFilterStore.getState()),
    () => selector(flowFilterStore.getState()),
  );
}
