/**
 * Minimal hash router. The views live at `#/flows`, `#/topology`, `#/sankey`, `#/theater` (D9),
 * and `#/overview` (gap 16 — the control-room overview that COMPOSES the other surfaces). No router
 * dependency — a `hashchange` listener bridged into React via useSyncExternalStore keeps it tear-free.
 *
 * A route may carry ONE optional detail segment — `#/flows/<api_call_id>` deep-links the flow
 * drill-down. `parseHash` keys the route off the FIRST segment only, so an unknown/extra segment
 * never breaks view resolution (an unrecognized route still falls back to `flows`).
 */
import { useSyncExternalStore } from 'react';

export type RouteName = 'flows' | 'topology' | 'sankey' | 'theater' | 'overview';

export const ROUTES: RouteName[] = ['flows', 'topology', 'sankey', 'theater', 'overview'];

const DEFAULT_ROUTE: RouteName = 'flows';

function rawHash(): string {
  return (typeof window !== 'undefined' ? window.location.hash : '').replace(/^#\/?/, '');
}

function parseHash(): RouteName {
  const name = rawHash().split('/')[0] as RouteName;
  return ROUTES.includes(name) ? name : DEFAULT_ROUTE;
}

/** The detail segment after the route (`#/flows/<id>` → `<id>`), or null when absent/blank. */
function parseDetail(): string | null {
  const segments = rawHash().split('/');
  if (!ROUTES.includes(segments[0] as RouteName)) return null;
  const detail = segments.slice(1).join('/');
  return detail ? decodeURIComponent(detail) : null;
}

function subscribe(cb: () => void): () => void {
  window.addEventListener('hashchange', cb);
  return () => window.removeEventListener('hashchange', cb);
}

export function useHashRoute(): RouteName {
  return useSyncExternalStore(subscribe, parseHash, () => DEFAULT_ROUTE);
}

/** The current route's detail segment (deep-linked drill-down id), tear-free like the route. */
export function useHashDetail(): string | null {
  return useSyncExternalStore(subscribe, parseDetail, () => null);
}

/** Imperatively navigate (used by the nav tabs + the flow drill-down). */
export function navigate(route: RouteName, detail?: string | null): void {
  const next = detail ? `#/${route}/${encodeURIComponent(detail)}` : `#/${route}`;
  if (window.location.hash === next) return;
  window.location.hash = next;
  // Browsers fire `hashchange` asynchronously (jsdom too) — dispatch synchronously so the
  // subscribed views re-render in the same tick as the interaction (the async native event
  // then re-reads the identical snapshot, a harmless no-op).
  window.dispatchEvent(new HashChangeEvent('hashchange'));
}
