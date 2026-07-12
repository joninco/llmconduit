/**
 * Merges the two sources of flow rows into ONE filtered, newest-on-top list for the table:
 *   - the LIVE WS store (`flows` Map keyed by api_call_id + `flowOrder`, newest-prepended) —
 *     the authoritative live state the socket feeds (snapshot + complete flow mutations);
 *   - the `/flows` TanStack query — the coordinated REST list. It seeds rows the store has not seen
 *     and may carry a newer revision after a reconnect or missed live frame.
 *
 * The store wins on conflict (it carries the freshest status/usage). `flowOrder` defines the
 * live-row identity, but the MERGED union is sorted GLOBALLY by `started_ms` descending so a
 * newer REST-only row can never sort below an older live row (finding 4) — newest-on-top holds
 * regardless of source. Filtering + the distinct model/upstream option lists are derived here so
 * the table and filter bar share one computation.
 */
import { useEffect, useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import type { FlowSummary } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { getConnection, queryKeys } from '../../api/connection';
import type { FlowFilters } from './filterTypes';
import { flowMatchesSearch } from './flowSearch';

export interface FlowRowsResult {
  /** Filtered rows, newest-on-top (the array the virtualizer renders). */
  rows: FlowSummary[];
  /** Total rows BEFORE filtering (for the "shown / total" readout). */
  total: number;
  /** Distinct model values present (requested or served) for the filter chips. */
  models: string[];
  /** Distinct upstream targets present for the filter chips. */
  upstreams: string[];
  /** Gap 15 — distinct `client_label`s present, for the per-client filter chips. */
  clients: string[];
  /** REST-list state, kept separate from an honest empty/filtered-empty result. */
  loadState: 'loading' | 'ready' | 'error';
  /** Retry a failed/stale REST list without disturbing authoritative live rows. */
  retry: () => void;
}

/** Union the live store rows (authoritative) with REST-only rows, newest-on-top. */
function mergeRows(
  order: string[],
  flows: Map<string, FlowSummary>,
  queryFlows: FlowSummary[],
): FlowSummary[] {
  // Both sources now carry the same complete revisioned FlowRow. Index REST by id and select one
  // whole revision on conflict; field-by-field backfill could combine values that never coexisted.
  const restById = new Map(queryFlows.map((f) => [f.api_call_id, f]));
  const seen = new Set<string>();
  const merged: FlowSummary[] = [];
  // Live rows first, in store order. A strictly newer REST revision wins as a complete row;
  // equal revisions prefer the live object for referential stability.
  for (const id of order) {
    const f = flows.get(id);
    if (f) {
      merged.push(mergeLiveWithRest(f, restById.get(id)));
      seen.add(id);
    }
  }
  // REST-only rows the store has not seen yet.
  for (const f of queryFlows) {
    if (!seen.has(f.api_call_id)) merged.push(f);
  }
  // Sort the COMBINED union GLOBALLY by `started_ms` descending so newest-on-top holds across
  // BOTH sources — a newer REST-only row must not sort below an older live row just because the
  // live rows were emitted first (finding 4). Stable tiebreak keeps deterministic ordering for
  // equal timestamps. (`flowOrder` already tracks live newest-prepended, so for an all-live list
  // this preserves the existing order.)
  merged.sort((a, b) => b.started_ms - a.started_ms);
  return merged;
}

/**
 * Reconcile two complete authoritative rows by optimistic-concurrency revision. Selecting the
 * entire newer row keeps usage/cost, status/terminal metadata, attribution, phases, and attempts
 * from one real FlowStore state. Equal revisions prefer live because the socket already installed
 * that exact version and retaining its identity avoids virtualizer churn.
 */
function mergeLiveWithRest(live: FlowSummary, rest: FlowSummary | undefined): FlowSummary {
  return rest && rest.revision > live.revision ? rest : live;
}

function applyFilters(rows: FlowSummary[], f: FlowFilters, searchQuery: string): FlowSummary[] {
  return rows.filter((row) => {
    if (f.status && row.status !== f.status) return false;
    if (f.model && row.model_requested !== f.model && row.model_served !== f.model) return false;
    // Provider-health drilldowns include FAILED primaries, which may differ from the provider that
    // ultimately served the flow. Match either the served target or any recorded attempt so a
    // global provider-attempt row never links to an apparently empty Flows view.
    if (
      f.upstream
      && row.upstream_target !== f.upstream
      && !row.attempts?.some((attempt) => attempt.provider === f.upstream)
    ) return false;
    // Gap 15: the per-client facet matches the row's `client_label` exactly. An unattributed row
    // (no label) never matches a client filter (it can't be claimed by a client).
    if (f.client && row.client_label !== f.client) return false;
    if (!flowMatchesSearch(row, searchQuery)) return false;
    return true;
  });
}

function distinct(rows: FlowSummary[], pick: (r: FlowSummary) => (string | null | undefined)[]): string[] {
  const set = new Set<string>();
  for (const r of rows) for (const v of pick(r)) if (v) set.add(v);
  return [...set].sort();
}

/**
 * Distinct `client_label`s ordered by DESCENDING flow volume (gap 15 review MEDIUM). Client attribution
 * is HIGH-CARDINALITY (unlike the bounded model/upstream sets) — thousands of distinct keys would render
 * thousands of chips and wrap the filter bar unusable. The FilterBar caps to the top-N by volume off
 * THIS order, so the busiest clients are the offered chips; first-seen order breaks ties for stability.
 */
function clientsByVolume(rows: FlowSummary[]): string[] {
  const counts = new Map<string, number>();
  for (const r of rows) {
    const c = r.client_label;
    if (c) counts.set(c, (counts.get(c) ?? 0) + 1);
  }
  // Descending count; insertion order (first-seen) is the stable tiebreak (Map preserves it).
  return [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([label]) => label);
}

export function useFlowRows(filters: FlowFilters, searchQuery = ''): FlowRowsResult {
  const order = useDashboard((s) => s.flowOrder);
  const flows = useDashboard((s) => s.flows);
  const reconcileFlowRows = useDashboard((s) => s.reconcileFlowRows);
  // Time-travel: while seeking (D11 paused on a historical cut), the store holds the FROZEN
  // snapshot summaries. Merging the live `/flows` REST list (or live WS rows) here would leak
  // flows/state from AFTER the seeked timestamp into the frozen view, so we render the snapshot
  // rows ALONE while seeking and resume the live merge on LIVE (HIGH finding 1).
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const { client } = getConnection();

  // The REST list seeds rows the live store has not seen and reconciles a missed event by revision.
  // Complete live mutations patch the list directly, so progress does not refetch this query.
  // Enabled for BOTH the real backend (where it is authoritative) and the mock (its `mockFetch`
  // answers `/flows`). Component tests that drive the store directly seed `resetWorld()` with a
  // real bootstrap and no live server, so the fetch simply fails/stays empty without churn.
  // DISABLED while seeking: the REST list is live (post-seek) data that must not bleed into the
  // frozen snapshot (finding 1).
  const query = useQuery({
    queryKey: queryKeys.flows,
    queryFn: () => client.flows(),
    enabled: !seeking,
    // FlowStore count/TTL/quota evictions have no row payload to broadcast. A small, bounded
    // unfiltered reconciliation closes that gap even on routes that never remount (ScopeBar keeps
    // this hook mounted globally), while WS mutations remain the low-latency progress path.
    refetchInterval: seeking ? false : 5_000,
    refetchIntervalInBackground: false,
  });
  // Ignore any cached REST result while seeking so the frozen snapshot stands alone.
  const queryData = seeking ? undefined : query.data;

  useEffect(() => {
    if (queryData) reconcileFlowRows(queryData.flows, queryData.flow_seq);
  }, [queryData, reconcileFlowRows]);

  const merged = useMemo(
    () => mergeRows(order, flows, queryData?.flows ?? []),
    [order, flows, queryData],
  );
  const rows = useMemo(() => applyFilters(merged, filters, searchQuery), [merged, filters, searchQuery]);
  const models = useMemo(() => distinct(merged, (r) => [r.model_requested, r.model_served]), [merged]);
  const upstreams = useMemo(() => distinct(merged, (r) => [r.upstream_target]), [merged]);
  // Gap 15: the distinct `client_label`s for the per-client filter, ordered by DESCENDING volume so the
  // FilterBar can cap to the top-N busiest (high-cardinality defense — review MEDIUM). Unattributed rows
  // have no label ⇒ contribute nothing (an absent attribution is never a filterable client).
  const clients = useMemo(() => clientsByVolume(merged), [merged]);

  const loadState = seeking ? 'ready' : query.isError ? 'error' : query.isPending ? 'loading' : 'ready';
  return {
    rows,
    total: merged.length,
    models,
    upstreams,
    clients,
    loadState,
    retry: () => { void query.refetch(); },
  };
}
