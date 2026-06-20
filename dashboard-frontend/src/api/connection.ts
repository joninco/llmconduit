/**
 * Wires the REST client + DashboardSocket to the stores, selecting mock vs real
 * transport from the env flag. This is the single composition root the app boots from;
 * views consume the stores (live state) + a TanStack Query client (REST cache) that this
 * module configures with the mock/real client.
 *
 * WS-driven invalidation (finding 10): after an ACCEPTED (post-dedup) frame, the socket
 * fires `onFrameApplied(domain)`; we map the domain → the affected query keys and
 * invalidate them so the REST cache refetches the authoritative shape.
 */
import { QueryClient } from '@tanstack/react-query';
import { DashboardClient, readCsrfCookie } from './client';
import { DashboardSocket } from './ws';
import { mockFetch, mockWsFactory } from './mock';
import { isMockEnabled, readBootstrap } from '../config/env';
import { authStore } from '../store/authStore';
import { dashboardStore } from '../store/dashboardStore';
import type { Domain } from './types';

/** Stable query keys; the WS invalidation + the views both reference these. */
export const queryKeys = {
  flows: ['flows'] as const,
  flowDetail: (id: string) => ['flows', id] as const,
  metrics: ['metrics'] as const,
  topology: ['topology'] as const,
  catalog: ['catalog'] as const,
} as const;

export interface Connection {
  client: DashboardClient;
  socket: DashboardSocket;
  queryClient: QueryClient;
  mock: boolean;
}

let singleton: Connection | null = null;

/**
 * Resolves the double-submit CSRF token at CALL time (finding 2): a token issued during a
 * fresh login lands in the `llmconduit_csrf` cookie, so we prefer the live cookie and fall
 * back to the auth store (seeded from the SPA bootstrap). This way a kill after login
 * always carries the current token.
 */
function resolveCsrfToken(): string | null {
  return readCsrfCookie() ?? authStore.getState().csrfToken;
}

/** Builds (once) the client+socket+query-client, seeding auth state from the bootstrap. */
export function getConnection(): Connection {
  if (singleton) return singleton;

  const mock = isMockEnabled();
  const boot = readBootstrap();

  // Seed auth store from the bootstrap (D7 double-submit CSRF + auth state).
  authStore.getState().setAuthenticated(boot.authenticated);
  authStore.getState().setCsrfToken(boot.csrf_token);
  authStore.getState().setMutationsEnabled(boot.mutations_enabled);

  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        // WS frames drive invalidation; keep REST reads cheap + non-chatty.
        staleTime: 5_000,
        refetchOnWindowFocus: false,
        retry: 1,
      },
    },
  });

  // Finding 1: BOTH the logout control and any 401/unauthorized handler route through the
  // ONE centralized teardown so the sensitive REST cache + live store never survive a
  // session boundary.
  const onUnauthorized = () => teardownSession();

  const client = new DashboardClient({
    fetchImpl: mock ? mockFetch : undefined,
    // Dynamic: cookie-first, bootstrap fallback — read fresh on every kill.
    getCsrfToken: resolveCsrfToken,
    onUnauthorized,
  });

  const socket = new DashboardSocket({
    factory: mock ? mockWsFactory : undefined,
    onUnauthorized,
    onFrameApplied: (domain) => invalidateForDomain(queryClient, domain),
    // Finding 7: after a transient WS drop, probe a protected endpoint — only a 401
    // bounces to login; otherwise the socket reconnects.
    probeAuth: () => client.probeAuth(),
  });

  singleton = { client, socket, queryClient, mock };
  return singleton;
}

/**
 * Centralized session teardown (finding 1). Disconnects the WS, CLEARS the TanStack Query
 * cache (which holds bodies/headers/usage), and resets BOTH zustand stores to their
 * initial state — so no session-scoped data (cached flow bodies, live frames, CSRF token,
 * mutation flag) survives a logout or a 401. Idempotent + safe before `getConnection()`.
 */
export function teardownSession(): void {
  if (singleton) {
    singleton.socket.disconnect();
    singleton.queryClient.clear();
  }
  dashboardStore.getState().reset();
  authStore.getState().reset();
}

/** Maps an accepted WS domain to the REST queries it invalidates. */
function invalidateForDomain(queryClient: QueryClient, domain: Domain): void {
  switch (domain) {
    case 'flow':
      // A flow frame changes the list AND whatever detail is open.
      void queryClient.invalidateQueries({ queryKey: queryKeys.flows });
      return;
    case 'metrics':
      void queryClient.invalidateQueries({ queryKey: queryKeys.metrics });
      return;
    case 'topology':
      void queryClient.invalidateQueries({ queryKey: queryKeys.topology });
      return;
    case 'monitor':
      // Monitor deltas feed the live store directly; no REST query mirrors them.
      return;
  }
}

/** Test/HMR reset of the singleton. */
export function resetConnection(): void {
  singleton?.socket.disconnect();
  singleton = null;
}
