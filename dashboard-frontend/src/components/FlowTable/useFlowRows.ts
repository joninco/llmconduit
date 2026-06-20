/**
 * Merges the two sources of flow rows into ONE filtered, newest-on-top list for the table:
 *   - the LIVE WS store (`flows` Map keyed by api_call_id + `flowOrder`, newest-prepended) —
 *     the authoritative live state the socket feeds (snapshot + flow_status + usage frames);
 *   - the `/flows` TanStack query — the REST list, invalidated on accepted `flow` frames
 *     (connection.ts finding 10). It seeds rows the store has not seen and stays the source of
 *     server-only fields (terminal_reason, cost roll-up) until a live frame supersedes them.
 *
 * The store wins on conflict (it carries the freshest status/usage). `flowOrder` defines the
 * live-row identity, but the MERGED union is sorted GLOBALLY by `started_ms` descending so a
 * newer REST-only row can never sort below an older live row (finding 4) — newest-on-top holds
 * regardless of source. Filtering + the distinct model/upstream option lists are derived here so
 * the table and filter bar share one computation.
 */
import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import type { FlowSummary } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { getConnection, queryKeys } from '../../api/connection';
import type { FlowFilters } from './filterTypes';

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
  // Index the REST list by api_call_id so a live row can re-adopt the server-only roll-up
  // fields the WS frame does not carry (finding 5).
  const restById = new Map(queryFlows.map((f) => [f.api_call_id, f]));
  const seen = new Set<string>();
  const merged: FlowSummary[] = [];
  // Live rows first, in store order. The live row's status/usage WIN, but `cost`/`terminal_reason`
  // fall back to the REST roll-up when the live frame lacks them — so a live update never blanks
  // the server's cost/terminal_reason.
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
 * Live row wins on status/usage/timing (the socket is freshest), but RETAINS the REST roll-up
 * fields (`cost`, `terminal_reason`) when the live row's are absent (null/undefined). The
 * `flow_status` store patch defaults these to null until a frame carries them, so without this a
 * live update would drop the server's cost/terminal_reason (finding 5). Returns the live row
 * unchanged when there is no REST counterpart or nothing to backfill.
 */
function mergeLiveWithRest(live: FlowSummary, rest: FlowSummary | undefined): FlowSummary {
  if (!rest) return live;
  const cost = live.cost ?? rest.cost ?? null;
  const terminalReason = live.terminal_reason ?? rest.terminal_reason ?? null;
  if (cost === (live.cost ?? null) && terminalReason === (live.terminal_reason ?? null)) {
    return live; // nothing to backfill — avoid a needless object churn
  }
  return { ...live, cost, terminal_reason: terminalReason };
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
  // Time-travel: while seeking (D11 paused on a historical cut), the store holds the FROZEN
  // snapshot summaries. Merging the live `/flows` REST list (or live WS rows) here would leak
  // flows/state from AFTER the seeked timestamp into the frozen view, so we render the snapshot
  // rows ALONE while seeking and resume the live merge on LIVE (HIGH finding 1).
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const { client } = getConnection();

  // The REST list — the PRODUCTION data source for the table. It seeds rows the live store has
  // not seen and carries the server-only roll-up fields (cost/terminal_reason). WS `flow` frames
  // invalidate `queryKeys.flows` (connection.ts), so it refetches when the server list changes.
  // Enabled for BOTH the real backend (where it is authoritative) and the mock (its `mockFetch`
  // answers `/flows`). Component tests that drive the store directly seed `resetWorld()` with a
  // real bootstrap and no live server, so the fetch simply fails/stays empty without churn.
  // DISABLED while seeking: the REST list is live (post-seek) data that must not bleed into the
  // frozen snapshot (finding 1).
  const query = useQuery({
    queryKey: queryKeys.flows,
    queryFn: () => client.flows(),
    enabled: !seeking,
  });
  // Ignore any cached REST result while seeking so the frozen snapshot stands alone.
  const queryData = seeking ? undefined : query.data;

  const merged = useMemo(
    () => mergeRows(order, flows, queryData?.flows ?? []),
    [order, flows, queryData],
  );
  const rows = useMemo(() => applyFilters(merged, filters), [merged, filters]);
  const models = useMemo(() => distinct(merged, (r) => [r.model_requested, r.model_served]), [merged]);
  const upstreams = useMemo(() => distinct(merged, (r) => [r.upstream_target]), [merged]);

  return { rows, total: merged.length, models, upstreams };
}
