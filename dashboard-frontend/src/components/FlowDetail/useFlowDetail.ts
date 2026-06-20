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
import type { FlowDetail as FlowDetailDto, FlowStatusPayload, FlowSummary } from '../../api/types';
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

/**
 * What the inspector exposes for ONE flow. The split below is the SEEK CONTRACT (finding 1):
 *  - `detail` (the live `/flows/:id`) supplies ONLY the three bodies, and ONLY for a flow present
 *    in the frozen cut while seeking (an out-of-cut selection gets no detail at all).
 *  - `frozenDetail` is `detail` LIVE, but `null` while seeking — so headers/deltas/timeline/status
 *    derive from the frozen snapshot (the store row) + the cut-bounded monitor join, never the
 *    live REST fetch. This keeps every non-body surface coherent with the seeked instant.
 */
export interface FlowDetailView {
  /** The live `/flows/:id` payload — bodies only while seeking (gated to an in-cut flow). */
  detail: FlowDetailDto | null;
  detailQuery: ReturnType<typeof useQuery<FlowDetailDto>>;
  /** `detail` while LIVE; `null` while seeking (non-body surfaces must not read live REST). */
  frozenDetail: FlowDetailDto | null;
  liveFlow: FlowSummary | null;
  status: FlowSummary['status'] | null;
  seeking: boolean;
  /** True when the selected flow is present in the frozen snapshot cut (store rows). */
  inCut: boolean;
  /** The frozen `monitor_seq` cut while seeking (else null) — bounds the monitor join. */
  seekMonitorSeq: number | null;
  /** The frozen cut wall-clock (snapshot `at_ms`) while seeking (else null) — bounds elapsed. */
  seekAtMs: number | null;
  mutationsEnabled: boolean;
  kill: (id: string) => void;
  killState: KillState;
}

export function useFlowDetail(apiCallId: string | null): FlowDetailView {
  const { client } = getConnection();
  const queryClient = useQueryClient();
  const connection = useDashboard((s) => s.connection);
  const mutationsEnabled = useAuth((s) => s.mutationsEnabled);
  const seekMonitorSeq = useDashboard((s) => s.seekMonitorSeq);
  const seekAtMs = useDashboard((s) => s.seekAtMs);
  const seeking = connection === 'seeking';

  // The live store row (authoritative status/usage). While seeking this IS the frozen snapshot row.
  const liveFlow = useDashboard((s) => (apiCallId ? s.flows.get(apiCallId) ?? null : null));
  // A selection is "in the cut" when the frozen snapshot rows contain it. While seeking, a flow
  // NOT in the cut must not fetch any detail (no live body, no live anything) — finding 1.
  const inCut = liveFlow !== null;

  // The detail fetch is DISABLED while seeking for an out-of-cut selection: no live `/flows/:id`
  // leaks post-cut data for a flow the snapshot never held. For an in-cut flow it still loads, but
  // only the BODIES are consumed during seek (see `frozenDetail`).
  const detailEnabled = !!apiCallId && (!seeking || inCut);
  const detailQuery = useQuery({
    queryKey: apiCallId ? queryKeys.flowDetail(apiCallId) : ['flows', '__none__'],
    queryFn: () => client.flowDetail(apiCallId as string),
    enabled: detailEnabled,
  });
  const detail = detailEnabled ? detailQuery.data ?? null : null;
  // Non-body surfaces (headers/deltas/timeline/status/usage/cost/elapsed) must come from the FROZEN
  // cut while seeking, so the live REST detail is withheld from them (finding 1).
  const frozenDetail = seeking ? null : detail;

  const [killState, setKillState] = useState<KillState>({ phase: 'idle' });

  const killMutation = useMutation({
    mutationFn: (id: string) => client.kill(id),
    onMutate: (id: string): { prev: FlowSummary | undefined; gen: number } => {
      setKillState({ phase: 'killing' });
      // Bind this mutation to the MONOTONIC connection-transition generation at dispatch (finding 1).
      // If ANY boundary is crossed before the request resolves (live→seek, seek→live, teardown, or a
      // fresh snapshot), the generation advances and BOTH callbacks bail before any store write — so
      // a late kill can never re-insert a stale optimistic row into a frozen cut, a re-established
      // live store, or a torn-down store. The connection STRING was insufficient: it is reusable
      // (`live→seek→live` returns to `'live'`), so a round-trip would have falsely matched.
      const gen = currentGen();
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
      return { prev, gen };
    },
    onError: (err: unknown, _id, ctx) => {
      // GENERATION GUARD (finding 1) — checked FIRST, before any store mutation. If the app crossed
      // a connection boundary after dispatch (entered seek, returned to live, was torn down, or took
      // a fresh snapshot), the store the optimistic row belonged to is gone; re-inserting prev would
      // leak stale live/future data across the boundary. Surface the error but make NO store write.
      if (!ctx || ctx.gen !== currentGen()) {
        if (err instanceof UnauthorizedError) setKillState({ phase: 'error', message: 'session expired' });
        else if (isForbidden(err)) setKillState({ phase: 'forbidden' });
        else setKillState({ phase: 'error', message: err instanceof Error ? err.message : 'kill failed' });
        return;
      }
      if (err instanceof UnauthorizedError) {
        // A 401 routed through centralized teardown (connection.ts), which CLEARS the live store —
        // but teardown bumps the generation, so the guard above already caught it. This remains as a
        // belt-and-braces guard: never roll back on a 401; reflect a generic expired-session error.
        setKillState({ phase: 'error', message: 'session expired' });
        return;
      }
      // Same generation (still the live store this row belongs to): undo the optimistic flip.
      if (ctx.prev) dashboardStore.getState().upsertFlow(ctx.prev);
      if (isForbidden(err)) {
        // 403 = mutations disabled / bad CSRF (D7). Distinct UI from a generic failure.
        setKillState({ phase: 'forbidden' });
      } else {
        setKillState({ phase: 'error', message: err instanceof Error ? err.message : 'kill failed' });
      }
    },
    onSuccess: (_res, id, ctx) => {
      setKillState({ phase: 'killed' });
      // GENERATION GUARD (finding 1): if a boundary was crossed after dispatch, the optimistic
      // `cancelled` row already living in the (now foreign) store must NOT be re-asserted, and the
      // detail query for a flow absent from the current cut must not be invalidated/refetched into
      // it. Reflect the killed state for the user but make NO store/query effect.
      if (!ctx || ctx.gen !== currentGen()) return;
      void queryClient.invalidateQueries({ queryKey: queryKeys.flowDetail(id) });
    },
  });

  const kill = useCallback(
    (id: string) => {
      // No mutations against a frozen cut: while seeking (D11 paused) the store holds the
      // historical snapshot, and the optimistic `patchFlowStatus` would mutate it (finding 2).
      // The kill button is already disabled while seeking; this guards a programmatic call too.
      if (seeking) return;
      if (!mutationsEnabled) {
        setKillState({ phase: 'forbidden' });
        return;
      }
      killMutation.mutate(id);
    },
    [killMutation, mutationsEnabled, seeking],
  );

  return {
    detail,
    detailQuery,
    frozenDetail,
    liveFlow,
    // Effective status for the header: live/frozen store row wins, else the frozen detail. While
    // seeking `frozenDetail` is null, so an in-cut flow reads its status from the frozen row only.
    status: liveFlow?.status ?? frozenDetail?.status ?? null,
    seeking,
    inCut,
    seekMonitorSeq,
    seekAtMs,
    mutationsEnabled,
    kill,
    killState,
  };
}

/**
 * The store's MONOTONIC connection-transition generation, for binding a kill mutation to the exact
 * connection epoch at dispatch (finding 1). It advances on EVERY boundary crossing (live↔seek↔
 * teardown↔fresh-snapshot) and never repeats, so — unlike the reusable connection STRING — a
 * `live→seek→live` round-trip yields a DIFFERENT generation. If it differs at resolve time, both
 * mutation callbacks bail before any store write, so a stale optimistic row can never be re-inserted
 * into a frozen snapshot, a re-established live store, or a torn-down store.
 */
function currentGen(): number {
  return dashboardStore.getState().connEpoch;
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
