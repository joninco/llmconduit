/**
 * React 18 bridges for the vanilla zustand stores. We use the store's own
 * `subscribe`/`getState` with React's `useSyncExternalStore` so concurrent rendering
 * stays tear-free (D9 §"useSyncExternalStore bridges zustand → React 18 concurrent").
 */
import { useSyncExternalStore } from 'react';
import { dashboardStore, type DashboardState } from './dashboardStore';
import { authStore, type AuthState } from './authStore';

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
