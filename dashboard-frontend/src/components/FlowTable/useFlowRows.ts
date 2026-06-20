/**
 * Merges the two sources of flow rows into ONE filtered, newest-on-top list for the table:
 *   - the LIVE WS store (`flows` Map keyed by api_call_id + `flowOrder`, newest-prepended) —
 *     the authoritative live state the socket feeds (snapshot + flow_status + usage frames);
 *   - the `/flows` TanStack query — the REST list, invalidated on accepted `flow` frames
 *     (connection.ts finding 10). It seeds rows the store has not seen and stays the source of
 *     server-only fields (terminal_reason, cost roll-up) until a live frame supersedes them.
 *
 * The store wins on conflict (it carries the freshest status/usage). `flowOrder` defines
 * newest-on-top; query-only rows are appended after the live ones, sorted by `started_ms` desc
 * so the union stays newest-first. Filtering + the distinct model/upstream option lists are
 * derived here so the table and filter bar share one computation.
 */
import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import type { FlowSummary } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { getConnection, queryKeys } from '../../api/connection';
import type { FlowFilters } from './filterTypes';
import { isFailover } from './flowModel';

export interface FlowRowsResult {
  /** Filtered rows, newest-on-top (the array the virtualizer renders). */
  rows: FlowSummary[];
  /** Total rows BEFORE filtering (for the "shown / total" readout). */
  total: number;
  /** Distinct model values present (requested or served) for the filter chips. */
  models: string[];
  /** Distinct upstream targets present for the filter chips. */
  upstreams: string[];
}

/** Union the live store rows (authoritative) with REST-only rows, newest-on-top. */
function mergeRows(
  order: string[],
  flows: Map<string, FlowSummary>,
  queryFlows: FlowSummary[],
): FlowSummary[] {
  const seen = new Set<string>();
  const merged: FlowSummary[] = [];
  // Live rows first, in store order (newest-prepended ⇒ already newest-first).
  for (const id of order) {
    const f = flows.get(id);
    if (f) {
      merged.push(f);
      seen.add(id);
    }
  }
  // REST-only rows the store has not seen yet, newest (by started_ms) first.
  const extras = queryFlows.filter((f) => !seen.has(f.api_call_id)).sort((a, b) => b.started_ms - a.started_ms);
  merged.push(...extras);
  return merged;
}

function applyFilters(rows: FlowSummary[], f: FlowFilters): FlowSummary[] {
  return rows.filter((row) => {
    if (f.status && row.status !== f.status) return false;
    if (f.model && row.model_requested !== f.model && row.model_served !== f.model) return false;
    if (f.upstream && row.upstream_target !== f.upstream) return false;
    return true;
  });
}

function distinct(rows: FlowSummary[], pick: (r: FlowSummary) => (string | null | undefined)[]): string[] {
  const set = new Set<string>();
  for (const r of rows) for (const v of pick(r)) if (v) set.add(v);
  return [...set].sort();
}

export function useFlowRows(filters: FlowFilters): FlowRowsResult {
  const order = useDashboard((s) => s.flowOrder);
  const flows = useDashboard((s) => s.flows);
  const { client, mock } = getConnection();

  // The REST list. WS `flow` frames invalidate `queryKeys.flows` (connection.ts), so this
  // refetches when the server list changes. In tests without a server it simply stays empty.
  const query = useQuery({
    queryKey: queryKeys.flows,
    queryFn: () => client.flows(),
    // Under the mock the live store is the primary driver; keep the REST poll quiet.
    enabled: mock,
  });
  const queryData = query.data;

  const merged = useMemo(
    () => mergeRows(order, flows, queryData?.flows ?? []),
    [order, flows, queryData],
  );
  const rows = useMemo(() => applyFilters(merged, filters), [merged, filters]);
  const models = useMemo(() => distinct(merged, (r) => [r.model_requested, r.model_served]), [merged]);
  const upstreams = useMemo(() => distinct(merged, (r) => [r.upstream_target]), [merged]);

  return { rows, total: merged.length, models, upstreams };
}

/** Re-export so the table can tag failover rows without re-importing the model module. */
export { isFailover };
