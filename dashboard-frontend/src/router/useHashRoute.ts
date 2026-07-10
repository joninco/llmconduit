/** Hash routing plus the dashboard's shareable window/filter scope. */
import { useSyncExternalStore } from 'react';
import type { FlowStatus } from '../api/types';

export type RouteName = 'overview' | 'flows' | 'topology' | 'sankey' | 'theater';
export type DashboardWindow = 'm1' | 'm5' | 'h1';

export const ROUTES: RouteName[] = ['overview', 'flows', 'topology', 'sankey', 'theater'];
export const DEFAULT_ROUTE: RouteName = 'overview';

export interface HashScope {
  window: DashboardWindow;
  status: FlowStatus | null;
  model: string | null;
  upstream: string | null;
  client: string | null;
}

export const DEFAULT_SCOPE: HashScope = {
  window: 'm1',
  status: null,
  model: null,
  upstream: null,
  client: null,
};

function rawHash(): string {
  return (typeof window !== 'undefined' ? window.location.hash : '').replace(/^#\/?/, '');
}

function hashParts(): { path: string; query: URLSearchParams } {
  const raw = rawHash();
  const split = raw.indexOf('?');
  return {
    path: split < 0 ? raw : raw.slice(0, split),
    query: new URLSearchParams(split < 0 ? '' : raw.slice(split + 1)),
  };
}

function parseHash(): RouteName {
  const name = hashParts().path.split('/')[0] as RouteName;
  return ROUTES.includes(name) ? name : DEFAULT_ROUTE;
}

/** `#/flows/<id>?…` detail, resilient to malformed percent escapes. */
function parseDetail(): string | null {
  const segments = hashParts().path.split('/');
  if (!ROUTES.includes(segments[0] as RouteName)) return null;
  const encoded = segments.slice(1).join('/');
  if (!encoded) return null;
  try {
    return decodeURIComponent(encoded);
  } catch {
    return null;
  }
}

export function readHashScope(): HashScope {
  const query = hashParts().query;
  const window = query.get('window');
  const status = query.get('status');
  return {
    window: window === 'm5' || window === 'h1' ? window : 'm1',
    status:
      status === 'open' || status === 'completed' || status === 'failed' || status === 'cancelled'
        ? status
        : null,
    model: clean(query.get('model')),
    upstream: clean(query.get('upstream')),
    client: clean(query.get('client')),
  };
}

function clean(value: string | null): string | null {
  const trimmed = value?.trim();
  return trimmed ? trimmed : null;
}

function subscribe(callback: () => void): () => void {
  window.addEventListener('hashchange', callback);
  return () => window.removeEventListener('hashchange', callback);
}

export function useHashRoute(): RouteName {
  return useSyncExternalStore(subscribe, parseHash, () => DEFAULT_ROUTE);
}

export function useHashDetail(): string | null {
  return useSyncExternalStore(subscribe, parseDetail, () => null);
}

export function useHashScope(): HashScope {
  // A stable serialized snapshot prevents useSyncExternalStore's object identity
  // rule from triggering an infinite render loop.
  const serialized = useSyncExternalStore(subscribe, scopeSnapshot, scopeSnapshot);
  return JSON.parse(serialized) as HashScope;
}

function scopeSnapshot(): string {
  return JSON.stringify(readHashScope());
}

/** Navigate while preserving the current shared scope by default. */
export function navigate(route: RouteName, detail?: string | null, scope = readHashScope()): void {
  const path = detail ? `${route}/${encodeURIComponent(detail)}` : route;
  writeHash(path, scope, false);
}

/** Merge scope fields into the current hash without adding browser history noise. */
export function updateHashScope(patch: Partial<HashScope>): void {
  const { path } = hashParts();
  writeHash(path || DEFAULT_ROUTE, { ...readHashScope(), ...patch }, true);
}

/** Clear all filters while retaining the selected window. */
export function clearHashFilters(): void {
  updateHashScope({ status: null, model: null, upstream: null, client: null });
}

/** Session teardown: no prior-user scope survives. */
export function resetHashScope(): void {
  writeHash(DEFAULT_ROUTE, DEFAULT_SCOPE, true);
}

function writeHash(path: string, scope: HashScope, replace: boolean): void {
  if (typeof window === 'undefined') return;
  const query = new URLSearchParams();
  if (scope.window !== DEFAULT_SCOPE.window) query.set('window', scope.window);
  if (scope.status) query.set('status', scope.status);
  if (scope.model) query.set('model', scope.model);
  if (scope.upstream) query.set('upstream', scope.upstream);
  if (scope.client) query.set('client', scope.client);
  const next = `#/${path}${query.size > 0 ? `?${query}` : ''}`;
  if (window.location.hash === next) return;
  if (replace) window.history.replaceState(null, '', next);
  else window.location.hash = next;
  window.dispatchEvent(new HashChangeEvent('hashchange'));
}
