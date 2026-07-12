import { useSyncExternalStore } from 'react';

export type FlowSort = 'started' | 'id' | 'client' | 'endpoint' | 'model' | 'upstream' | 'status' | 'tokens' | 'cost' | 'latency';
export type SortDirection = 'asc' | 'desc';
export interface FlowViewState { q: string; sort: FlowSort; direction: SortDirection }

const SORTS: FlowSort[] = ['started', 'id', 'client', 'endpoint', 'model', 'upstream', 'status', 'tokens', 'cost', 'latency'];

function parts(): { path: string; query: URLSearchParams } {
  const raw = window.location.hash.replace(/^#\/?/, '');
  const split = raw.indexOf('?');
  return { path: split < 0 ? raw : raw.slice(0, split), query: new URLSearchParams(split < 0 ? '' : raw.slice(split + 1)) };
}

export function readFlowViewState(): FlowViewState {
  const query = parts().query;
  const candidate = query.get('sort') as FlowSort | null;
  return { q: query.get('q') ?? '', sort: candidate && SORTS.includes(candidate) ? candidate : 'started', direction: query.get('direction') === 'asc' ? 'asc' : 'desc' };
}

const snapshot = () => JSON.stringify(readFlowViewState());
const subscribe = (callback: () => void) => {
  window.addEventListener('hashchange', callback);
  return () => window.removeEventListener('hashchange', callback);
};

export function useFlowViewState(): FlowViewState {
  return JSON.parse(useSyncExternalStore(subscribe, snapshot, snapshot)) as FlowViewState;
}

export function updateFlowViewState(patch: Partial<FlowViewState>): void {
  const { path, query } = parts();
  const next = { ...readFlowViewState(), ...patch };
  if (next.q.trim()) query.set('q', next.q.trim()); else query.delete('q');
  if (next.sort !== 'started') query.set('sort', next.sort); else query.delete('sort');
  if (next.direction !== 'desc') query.set('direction', next.direction); else query.delete('direction');
  const hash = `#/${path || 'flows'}${query.size ? `?${query}` : ''}`;
  window.history.replaceState(null, '', hash);
  window.dispatchEvent(new HashChangeEvent('hashchange'));
}
