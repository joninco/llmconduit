/**
 * Data + actions for the inspector of ONE flow (`api_call_id`).
 *
 *  - `detailQuery`: `GET /flows/:id` (TanStack), keyed by `queryKeys.flowDetail(id)` — the three
 *    captured bodies + headers + replayed deltas. Invalidated by accepted `flow` frames
 *    (connection.ts). The body fields are absent when EVICTED (D5 body-free tradeoff), which the
 *    panes render as "body evicted".
 *  - kill: `POST /flows/:id/kill` (CSRF via the client). OPTIMISTIC — the row flips to
 *    `cancelled` immediately in the live store; a 403 (mutations off / bad CSRF) ROLLS BACK and
 *    surfaces the error. Gated by `mutations_enabled` from the auth bootstrap.
 *  - seek awareness: when the store connection is `seeking` (D11 time-travel paused), the
 *    inspector shows the SNAPSHOT summary and treats a missing body as evicted rather than
 *    loading (the live body may be gone for a historical cut).
 */
import { useCallback, useState } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type { FlowStatusPayload, FlowSummary } from '../../api/types';
import { getConnection, queryKeys } from '../../api/connection';
import { UnauthorizedError } from '../../api/client';
import { useAuth, useDashboard } from '../../store/hooks';
import { dashboardStore } from '../../store/dashboardStore';

export type KillState =
  | { phase: 'idle' }
  | { phase: 'killing' }
  | { phase: 'killed' }
  | { phase: 'forbidden' }
  | { phase: 'error'; message: string };

export function useFlowDetail(apiCallId: string | null) {
  const { client } = getConnection();
  const queryClient = useQueryClient();
  const connection = useDashboard((s) => s.connection);
  const mutationsEnabled = useAuth((s) => s.mutationsEnabled);
  const seeking = connection === 'seeking';

  // The live store row (authoritative status/usage); falls back to the query detail's summary.
  const liveFlow = useDashboard((s) => (apiCallId ? s.flows.get(apiCallId) ?? null : null));

  const detailQuery = useQuery({
    queryKey: apiCallId ? queryKeys.flowDetail(apiCallId) : ['flows', '__none__'],
    queryFn: () => client.flowDetail(apiCallId as string),
    enabled: !!apiCallId,
  });

  const [killState, setKillState] = useState<KillState>({ phase: 'idle' });

  const killMutation = useMutation({
    mutationFn: (id: string) => client.kill(id),
    onMutate: (id: string): { prev: FlowSummary | undefined } => {
      setKillState({ phase: 'killing' });
      // Optimistic: flip the live row to `cancelled` right away (D10 "optimistic state update").
      const prev = dashboardStore.getState().flows.get(id);
      if (prev) {
        const patch: FlowStatusPayload = {
          type: 'flow_status',
          api_call_id: id,
          response_id: prev.response_id ?? null,
          status: 'cancelled',
          model_requested: prev.model_requested ?? null,
          model_served: prev.model_served ?? null,
          upstream_target: prev.upstream_target ?? null,
          usage: prev.usage ?? null,
          started_ms: prev.started_ms,
          elapsed_ms: prev.elapsed_ms ?? null,
        };
        dashboardStore.getState().patchFlowStatus(patch);
      }
      return { prev };
    },
    onError: (err: unknown, _id, ctx) => {
      if (err instanceof UnauthorizedError) {
        // A 401 already routed through centralized teardown (connection.ts), which CLEARED the
        // live store. Rolling back here would re-insert the optimistic row's prior value into the
        // now-cleared store — leaking session data past the auth boundary. So on 401 we do NOT
        // roll back; teardown wins. Reflect a generic expired-session error.
        setKillState({ phase: 'error', message: 'session expired' });
        return;
      }
      // Non-auth failure (e.g. 403): the store was NOT torn down, so undo the optimistic flip.
      if (ctx?.prev) dashboardStore.getState().upsertFlow(ctx.prev);
      if (isForbidden(err)) {
        // 403 = mutations disabled / bad CSRF (D7). Distinct UI from a generic failure.
        setKillState({ phase: 'forbidden' });
      } else {
        setKillState({ phase: 'error', message: err instanceof Error ? err.message : 'kill failed' });
      }
    },
    onSuccess: (_res, id) => {
      setKillState({ phase: 'killed' });
      void queryClient.invalidateQueries({ queryKey: queryKeys.flowDetail(id) });
    },
  });

  const kill = useCallback(
    (id: string) => {
      if (!mutationsEnabled) {
        setKillState({ phase: 'forbidden' });
        return;
      }
      killMutation.mutate(id);
    },
    [killMutation, mutationsEnabled],
  );

  return {
    detail: detailQuery.data ?? null,
    detailQuery,
    liveFlow,
    /** Effective status for the header: live store row wins, else the fetched detail. */
    status: liveFlow?.status ?? detailQuery.data?.status ?? null,
    seeking,
    mutationsEnabled,
    kill,
    killState,
  };
}

/**
 * Detects a 403 from the kill route (mutations disabled / bad CSRF). The D9 client throws a
 * generic `Error` for non-401 failures formatted as `POST /path failed: 403`; we match the
 * trailing status so the UI can show the distinct "mutations off" state rather than a generic
 * error. (A 401 is handled separately — the client throws `UnauthorizedError` + runs teardown.)
 */
function isForbidden(err: unknown): boolean {
  return err instanceof Error && /\bfailed:\s*403\b/.test(err.message);
}
