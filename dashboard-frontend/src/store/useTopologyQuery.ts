/**
 * `useTopologyQuery` — the shared LIVE-only `/topology` REST read (D13, finding 5). Until this
 * existed, `/topology` was NEVER fetched: the WS `topology_update` invalidation in `connection.ts`
 * had no observer, so the REST topology + the price table were unused (the Sankey had no prices and
 * the topology relied solely on WS frames after connect). This hook is the observer.
 *
 * What it seeds: nodes/edges (the radial map) AND the price table (the Sankey cost colors / `$`/min
 * lane costs) — the WS `topology_update` frames carry ONLY nodes/edges, so the price table comes
 * from here. `metric`/`topology` WS frames invalidate `queryKeys.topology` (connection.ts), so the
 * query refetches the authoritative shape and re-seeds.
 *
 * LIVE-only (the seek-coherence invariant): the query is DISABLED while seeking and the seed is
 * gated on `!seeking`, so a frozen seek cut (`applySeekCut` installed nodes/edges/prices as-of the
 * seeked instant) is NEVER overwritten by live REST data. On resume the query re-enables and re-seeds
 * the current live topology. Both TopologyView and SankeyView mount this (idempotent — one shared
 * query key), so the price table is primed for the Sankey regardless of which view is open first.
 */
import { useEffect } from 'react';
import { useQuery } from '@tanstack/react-query';
import { getConnection, queryKeys } from '../api/connection';
import { dashboardStore } from './dashboardStore';
import { useDashboard } from './hooks';

export function useTopologyQuery(): void {
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const { client } = getConnection();

  const query = useQuery({
    queryKey: queryKeys.topology,
    queryFn: () => client.topology(),
    // Disabled while seeking: the REST read is live (post-seek) data that must not bleed into the
    // frozen snapshot. Re-enables on resume.
    enabled: !seeking,
  });

  // Seed the store from the LIVE result only. Guard on `!seeking` so a result that resolves just as
  // a seek begins never overwrites the frozen cut. `data` identity changes on each refetch, so the
  // effect re-seeds when the authoritative topology moves.
  const data = query.data;
  useEffect(() => {
    if (!seeking && data) dashboardStore.getState().seedTopology(data);
  }, [seeking, data]);
}
