/**
 * Auth/session state (zustand vanilla, bridged via useSyncExternalStore). The shell
 * reads `authenticated` to decide login-shell vs dashboard. On any 401 / logout the app
 * calls `teardownSession()` (connection.ts), which invokes `reset()` here (full clear) —
 * so no session-scoped secret survives. `bounceToLogin()` remains as a narrow
 * "auth flag only" helper for callers that don't need the full teardown.
 */
import { createStore } from 'zustand/vanilla';

export interface AuthState {
  authenticated: boolean;
  /** Double-submit CSRF token from the bootstrap/cookie (D7). */
  csrfToken: string | null;
  mutationsEnabled: boolean;

  setAuthenticated: (v: boolean) => void;
  setCsrfToken: (t: string | null) => void;
  setMutationsEnabled: (v: boolean) => void;
  /** Any 401 → drop back to the login shell. */
  bounceToLogin: () => void;
  /**
   * Full reset to the initial UNAUTHENTICATED state — clears the CSRF token + mutation
   * flag too (not just `authenticated`). Called by `teardownSession()` on logout / 401 so
   * no session-scoped secret survives across sessions.
   */
  reset: () => void;
}

export const authStore = createStore<AuthState>((set) => ({
  authenticated: false,
  csrfToken: null,
  mutationsEnabled: false,

  setAuthenticated: (authenticated) => set({ authenticated }),
  setCsrfToken: (csrfToken) => set({ csrfToken }),
  setMutationsEnabled: (mutationsEnabled) => set({ mutationsEnabled }),
  bounceToLogin: () => set({ authenticated: false }),
  reset: () => set({ authenticated: false, csrfToken: null, mutationsEnabled: false }),
}));

export type AuthStore = typeof authStore;
