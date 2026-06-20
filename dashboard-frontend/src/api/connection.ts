/**
 * Wires the REST client + DashboardSocket to the stores, selecting mock vs real
 * transport from the env flag. This is the single composition root the app boots from;
 * views consume the stores (live state) + a TanStack Query client (REST cache) that this
 * module configures with the mock/real client.
 */
import { QueryClient } from '@tanstack/react-query';
import { DashboardClient } from './client';
import { DashboardSocket } from './ws';
import { mockFetch, mockWsFactory } from './mock';
import { isMockEnabled, readBootstrap } from '../config/env';
import { authStore } from '../store/authStore';

export interface Connection {
  client: DashboardClient;
  socket: DashboardSocket;
  queryClient: QueryClient;
  mock: boolean;
}

let singleton: Connection | null = null;

/** Builds (once) the client+socket+query-client, seeding auth state from the bootstrap. */
export function getConnection(): Connection {
  if (singleton) return singleton;

  const mock = isMockEnabled();
  const boot = readBootstrap();

  // Seed auth store from the bootstrap (D7 double-submit CSRF + auth state).
  authStore.getState().setAuthenticated(boot.authenticated);
  authStore.getState().setCsrfToken(boot.csrf_token);
  authStore.getState().setMutationsEnabled(boot.mutations_enabled);

  const onUnauthorized = () => authStore.getState().bounceToLogin();

  const client = new DashboardClient({
    fetchImpl: mock ? mockFetch : undefined,
    getCsrfToken: () => authStore.getState().csrfToken,
    onUnauthorized,
  });

  const socket = new DashboardSocket({
    factory: mock ? mockWsFactory : undefined,
    onUnauthorized,
  });

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

  singleton = { client, socket, queryClient, mock };
  return singleton;
}

/** Test/HMR reset of the singleton. */
export function resetConnection(): void {
  singleton?.socket.disconnect();
  singleton = null;
}
