/**
 * Runtime environment + the mock-vs-real selector.
 *
 * Mock is enabled when:
 *  - the Vite dev server is running (`import.meta.env.DEV`) and no real bootstrap is
 *    present, OR
 *  - `?mock=1` is in the URL (force on), unless `?mock=0` forces it off.
 *
 * The Rust shell embeds a real bootstrap (`window.__LLMCONDUIT_DASHBOARD__`) with the
 * CSRF token + auth state (D7). When that object is present we run against the real
 * backend; otherwise we fall back to the in-browser mock so views render in `npm run dev`.
 */
import type { DashboardBootstrap } from '../api/types';
import { mockBootstrapCsrf } from '../api/mock';

declare global {
  interface Window {
    __LLMCONDUIT_DASHBOARD__?: DashboardBootstrap;
  }
}

function urlFlag(name: string): string | null {
  if (typeof window === 'undefined') return null;
  return new URLSearchParams(window.location.search).get(name);
}

export function isMockEnabled(): boolean {
  const forced = urlFlag('mock');
  if (forced === '1') return true;
  if (forced === '0') return false;
  const hasRealBootstrap = typeof window !== 'undefined' && !!window.__LLMCONDUIT_DASHBOARD__;
  if (hasRealBootstrap) return false;
  // Vite injects `import.meta.env.DEV`; default to mock in dev.
  return import.meta.env.DEV;
}

/**
 * Reads the SPA bootstrap. With a real Rust shell this is `window.__LLMCONDUIT_DASHBOARD__`.
 * Under the mock we synthesize an UNAUTHENTICATED bootstrap (the login shell renders, and
 * a successful mock login flips auth) carrying the mock CSRF token + mutations enabled.
 */
export function readBootstrap(): DashboardBootstrap {
  if (typeof window !== 'undefined' && window.__LLMCONDUIT_DASHBOARD__) {
    return window.__LLMCONDUIT_DASHBOARD__;
  }
  return {
    authenticated: false,
    csrf_token: isMockEnabled() ? mockBootstrapCsrf : null,
    mutations_enabled: isMockEnabled(),
  };
}
