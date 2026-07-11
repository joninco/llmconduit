/**
 * Wires the REST client + DashboardSocket to the stores, selecting mock vs real
 * transport from the env flag. This is the single composition root the app boots from;
 * views consume the stores (live state) + a TanStack Query client (REST cache) that this
 * module configures with the mock/real client.
 *
 * WS-driven invalidation: accepted complete flow rows patch the list cache in-place through the
 * store. Only terminal mutations invalidate that flow's body-bearing detail read; progress/usage
 * never fan out across the list/detail family.
 */
import { QueryClient } from '@tanstack/react-query';
import { DashboardClient, readCsrfCookie } from './client';
import { DashboardSocket } from './ws';
import { mockFetch, mockWsFactory } from './mock';
import { isMockEnabled, readBootstrap } from '../config/env';
import { authStore } from '../store/authStore';
import { dashboardStore } from '../store/dashboardStore';
import { flowFilterStore } from '../store/flowFilterStore';
import { resetHashScope } from '../router/useHashRoute';
import type { DashboardBootstrap, DashboardFrame, OverviewQuery } from './types';
import { DashboardSchemaMismatchError, DASHBOARD_SCHEMA_VERSION } from './schemaVersion';
import { DashboardContractError } from './validation';

/** Stable query keys; the WS invalidation + the views both reference these. */
export const queryKeys = {
  flows: ['flows'] as const,
  flowDetail: (id: string) => ['flows', id] as const,
  metrics: ['metrics'] as const,
  overviewRoot: ['overview'] as const,
  overview: (query: OverviewQuery) => [
    'overview',
    query.window,
    query.at ?? null,
    query.status ?? null,
    query.model ?? null,
    query.upstream ?? null,
    query.client ?? null,
  ] as const,
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
  let boot: DashboardBootstrap;
  try {
    boot = readBootstrap();
  } catch (error) {
    // Bootstrap validation runs before a REST client exists. Route persistent version/root-contract
    // failures into the same explicit fatal shell used by asynchronous API reads.
    surfaceDashboardFatal(error);
    boot = {
      authenticated: false,
      csrf_token: null,
      mutations_enabled: false,
      schema_version: DASHBOARD_SCHEMA_VERSION,
    };
  }

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
    onFatal: surfaceDashboardFatal,
  });

  const socket = new DashboardSocket({
    factory: mock ? mockWsFactory : undefined,
    onUnauthorized,
    onFrameApplied: (frame) => invalidateForFrame(queryClient, frame),
    // Finding 7: after a transient WS drop, probe a protected endpoint — only a 401
    // bounces to login; otherwise the socket reconnects.
    probeAuth: () => client.probeAuth(),
  });

  // Start the APP-LIFETIME Sankey fold engine from the composition root (D12 R3): it subscribes to
  // the store directly and folds each flow's usage GROWTH into a timestamped delta as frames arrive,
  // stamping every delta at its REAL arrival time regardless of which view is mounted. The SankeyView
  // only READS the maintained ring — so usage that grew while the Sankey was unmounted is recorded at
  // its real arrival time and correctly ages out of the 30 s window, never lumped in at remount.

  singleton = { client, socket, queryClient, mock };
  return singleton;
}

/** Low-level API modules report fatal roots through this composition-root callback (no store cycle). */
function surfaceDashboardFatal(error: unknown): void {
  if (
    error instanceof DashboardContractError
    || (error instanceof DashboardSchemaMismatchError && !error.reloadRequested)
  ) {
    dashboardStore.getState().setConnection('error');
    dashboardStore.getState().setFatalError(error.message);
  }
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
  flowFilterStore.getState().hydrate({ status: null, model: null, upstream: null, client: null });
  resetHashScope();
  authStore.getState().reset();
}

/** Maps an accepted WS frame to the smallest REST query set that can actually be stale. */
function invalidateForFrame(queryClient: QueryClient, frame: DashboardFrame): void {
  switch (frame.domain) {
    case 'flow':
      // Full live rows already patched the list store. Only a terminal transition can finish body,
      // header, delta, or error capture, so refresh exactly that flow's detail query. A Set avoids
      // duplicate invalidations if a future batched frame repeats the same terminal row.
      for (const id of new Set(
        frame.batch.flatMap((payload) =>
          payload.type === 'flow_status' && payload.phase === 'terminal'
            ? [payload.api_call_id]
            : []),
      )) {
        void queryClient.invalidateQueries({ queryKey: queryKeys.flowDetail(id), exact: true });
      }
      return;
    case 'metrics':
      void queryClient.invalidateQueries({ queryKey: queryKeys.metrics });
      // The process-level metrics publisher owns Overview cuts too. Refresh every mounted
      // Overview scope from the same presentation tick; historical keys retain their explicit
      // `at`, so even an incidental refetch can only return that retained cut, never live data.
      void queryClient.invalidateQueries({ queryKey: queryKeys.overviewRoot });
      // Gap 13: the REST `/topology` node's per_provider tile (p50/p95/p99 + error rate) is JOINED
      // from the SAME m1 metrics window (Rust `from_health_with_metrics`), but the live WS
      // `topology_update` frame carries `per_provider` ABSENT — so without this the per-provider
      // metrics would stay STALE until an UNRELATED topology change happened to refetch `/topology`
      // (e.g. old values lingering after a provider drops to zero in-window samples). A metrics frame
      // moves that window, so it must ALSO refresh the topology query that joins it.
      void queryClient.invalidateQueries({ queryKey: queryKeys.topology });
      return;
    case 'topology':
      void queryClient.invalidateQueries({ queryKey: queryKeys.topology });
      return;
    case 'monitor':
      // Monitor deltas feed the live store directly; no REST query mirrors them.
      return;
  }
}

/** Test/HMR reset of the singleton (also stops the app-lifetime Sankey fold engine it started). */
export function resetConnection(): void {
  singleton?.socket.disconnect();
  singleton = null;
  // Pair the engine teardown with the singleton reset so a fresh `getConnection()` re-starts it
  // (HMR) and tests don't leak a running fold subscription / accumulated ring across cases.
}
