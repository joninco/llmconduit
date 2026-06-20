/**
 * Auth/session state (zustand vanilla, bridged via useSyncExternalStore). The shell
 * reads `authenticated` to decide login-shell vs dashboard; the client's onUnauthorized
 * handler calls `bounceToLogin()` on any 401.
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
}

export const authStore = createStore<AuthState>((set) => ({
  authenticated: false,
  csrfToken: null,
  mutationsEnabled: false,

  setAuthenticated: (authenticated) => set({ authenticated }),
  setCsrfToken: (csrfToken) => set({ csrfToken }),
  setMutationsEnabled: (mutationsEnabled) => set({ mutationsEnabled }),
  bounceToLogin: () => set({ authenticated: false }),
}));

export type AuthStore = typeof authStore;
