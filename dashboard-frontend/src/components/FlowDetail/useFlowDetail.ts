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
    onMutate: (id: string): { prev: FlowSummary | undefined; epoch: string } => {
      setKillState({ phase: 'killing' });
      // Bind this mutation to a LIVE-state epoch: the connection state at dispatch. If the app has
      // since left live (entered seek, or teardown) by the time the request resolves, the success/
      // rollback callbacks IGNORE it — they must never mutate the frozen snapshot store (finding 2).
      const epoch = liveEpoch();
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
      return { prev, epoch };
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
      // EPOCH GUARD (finding 2): if the app entered seek (or was torn down) after dispatch, the
      // store now holds a FROZEN snapshot — re-inserting the optimistic row's prior value would
      // leak live/future data into the frozen cut. Skip the rollback; only surface the error.
      if (ctx && ctx.epoch === liveEpoch() && ctx.prev) {
        // Non-auth failure (e.g. 403) in the SAME live epoch: undo the optimistic flip.
        dashboardStore.getState().upsertFlow(ctx.prev);
      }
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
 * A coarse "are we LIVE" epoch for binding a kill mutation to the state at dispatch (finding 2).
 * `seeking` and the post-teardown `idle`/`closed`/`error` states are all NON-live; only `live`
 * (and the transient `connecting`) own the mutable store. The epoch is just the connection state
 * string — if it differs at resolve time, the optimistic rollback is skipped so it can't write
 * into a frozen snapshot or a torn-down store.
 */
function liveEpoch(): string {
  return dashboardStore.getState().connection;
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
