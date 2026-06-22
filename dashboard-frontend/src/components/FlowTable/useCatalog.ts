/**
 * `useCatalog` (gap 09) — reads the per-model catalog (`GET /dashboard/api/catalog`, the bare
 * gap-06 array) and exposes a `model id → context_limit` lookup for the context-utilization gauge
 * + the aggregate context-pressure stat.
 *
 * The catalog is a static-ish read (no cursor, no WS domain), so it is a plain TanStack query keyed
 * by `queryKeys.catalog` — the SAME pattern the strip uses for `/metrics`. The mock answers it; the
 * real backend serves it. While the query is loading/errored the map is simply empty, so every
 * gauge reads `—` (unavailable) rather than a fabricated utilization — the don't-lie-with-zeros
 * posture extends to "catalog not yet loaded".
 *
 * Honesty preserved from gap 06: a model whose upstream advertises NO window serializes
 * `context_limit: null` — we keep it `null` in the map (distinct from a real integer), and the
 * derivation treats `null` as UNKNOWN capacity ⇒ unavailable utilization (never `0%`/`100%`).
 */
import { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { getConnection, queryKeys } from '../../api/connection';
import type { ContextLimitMap } from './contextUtilization';

/**
 * Fetch the catalog and fold it into a `ContextLimitMap`. Returns an empty map until the read
 * resolves (so gauges render `—`, never a guessed limit). A duplicate model id keeps the LAST
 * entry — the catalog is expected to be unique by id, but this is deterministic if not.
 */
export function useCatalog(): ContextLimitMap {
  const { client } = getConnection();
  const query = useQuery({
    queryKey: queryKeys.catalog,
    queryFn: () => client.catalog(),
  });

  return useMemo(() => {
    const map: ContextLimitMap = {};
    for (const entry of query.data ?? []) {
      // Preserve the gap-06 tri-state: a real integer stays the integer; `null`/absent becomes
      // `null` (UNKNOWN capacity). Never coerce a missing window to `0`.
      map[entry.id] = entry.context_limit ?? null;
    }
    return map;
  }, [query.data]);
}
