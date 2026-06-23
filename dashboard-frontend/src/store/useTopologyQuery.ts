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
import { useEffect, useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { getConnection, queryKeys } from '../api/connection';
import { dashboardStore } from './dashboardStore';
import { useDashboard } from './hooks';
import type { ProviderLatency } from '../api/types';

/**
 * Gap 13 — a by-id map of the per-provider latency/error metrics from the LIVE REST `/topology`
 * read. This is the AUTHORITATIVE per-provider source while live: the store's `topologyNodes` get
 * `per_provider` only when last seeded from REST, but a subsequent WS `topology_update` frame
 * REPLACES them with nodes that carry `per_provider` ABSENT (the WS frame does not join the metrics
 * window — gap-12 discovery). Reading the metrics straight off the REST query data here keeps them
 * stable across WS frames, exactly as the spec wants (the per-provider tile reads the REST/snapshot
 * path). Empty while seeking (the query is disabled then; the view reads the frozen snapshot node's
 * own `per_provider` instead, which the `/snapshot` reshape populates).
 */
export type PerProviderById = Record<string, ProviderLatency>;

export interface TopologyQueryResult {
  /** Per-provider metrics keyed by provider id, from the LIVE REST read (absent ⇒ no in-window data). */
  perProviderById: PerProviderById;
}

export function useTopologyQuery(): TopologyQueryResult {
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

  // Gap 13: derive the per-provider map off the LIVE REST data (a node only contributes when its
  // `per_provider` is present — a zero-sample provider is absent, don't-lie-with-zeros). Empty while
  // seeking, so the live REST metrics never bleed into the frozen historical tooltip.
  const perProviderById = useMemo<PerProviderById>(() => {
    const map: PerProviderById = {};
    if (!seeking && data) {
      for (const node of data.nodes) {
        if (node.per_provider) map[node.id] = node.per_provider;
      }
    }
    return map;
  }, [seeking, data]);

  return { perProviderById };
}
