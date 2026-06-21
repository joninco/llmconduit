/**
 * Joins the live monitor ring (`DebugWsMessage[]`, keyed by `response_id`) to ONE flow.
 *
 * A flow row is keyed by `api_call_id`, but the Monitor stream (segment_append / event_append /
 * request_upsert / request_status — the real `src/monitor.rs` arms) is keyed by `response_id`
 * (the engine id). The two coexist on a flow (types.ts): the inspector therefore selects the
 * flow's `response_id` and filters the ring to it, splitting into the streamed deltas (segments),
 * the timeline (events), and the latest request status. Pure + memo-friendly: same inputs ⇒ same
 * arrays-by-content so the panels don't churn.
 */
import type { DebugSegment, DebugTimelineEvent, DebugRequest, DebugRequestStatus, DebugWsMessage } from '../../api/types';

export interface MonitorJoin {
  segments: DebugSegment[];
  /**
   * The per-segment MonitorHub `monitor_seq`, sliced in LOCKSTEP with `segments` (same length/order).
   * It is the cross-source merge cursor the deltas bridge needs (finding 2): the seq the socket
   * stamped onto the `segment_append` frame that delivered each segment. `null` when no seq was
   * provided (`opts.seqs` omitted), which makes the merge append the live run verbatim.
   */
  segmentSeqs: (number | null)[];
  events: DebugTimelineEvent[];
  /** Latest request_upsert for this response (stats/model), if any. */
  request: DebugRequest | null;
  /** Latest status (from request_upsert or request_status), if any. */
  status: DebugRequestStatus | null;
  /** Latest error string seen on this response, if any. */
  error: string | null;
}

const EMPTY: MonitorJoin = { segments: [], segmentSeqs: [], events: [], request: null, status: null, error: null };

/**
 * Filters `monitor` to `responseId` and folds it into the per-flow view. Order is preserved
 * (the ring is append-order = arrival order = chronological). Returns a stable empty result when
 * `responseId` is null (a flow not yet linked to an engine response) so callers can render an
 * empty deltas/timeline rather than the whole ring.
 *
 * SEEK BOUND (finding 1): pass `opts.seqs` (the store's per-message `monitorSeqs`, sliced in
 * lockstep with `monitor`) + `opts.maxSeq` (the frozen `monitor_seq` cut). Any message whose
 * arrival seq is `> maxSeq` — i.e. it streamed AFTER the seeked instant — is excluded, so a
 * historical cut's deltas/timeline/status never leak post-cut frames. Omitted ⇒ no bound (LIVE).
 */
export function joinMonitor(
  monitor: DebugWsMessage[],
  responseId: string | null | undefined,
  opts?: { seqs?: number[]; maxSeq?: number | null },
): MonitorJoin {
  if (!responseId) return EMPTY;
  const segments: DebugSegment[] = [];
  // Per-segment `monitor_seq`, pushed in lockstep with `segments` (the merge cursor — finding 2).
  // `null` for every segment when no `opts.seqs` was supplied (the merge then appends verbatim).
  const segmentSeqs: (number | null)[] = [];
  const events: DebugTimelineEvent[] = [];
  let request: DebugRequest | null = null;
  let status: DebugRequestStatus | null = null;
  let error: string | null = null;

  const seqs = opts?.seqs;
  const maxSeq = opts?.maxSeq ?? null;
  for (let i = 0; i < monitor.length; i += 1) {
    const msg = monitor[i]!;
    // Drop anything past the frozen cut (only when a bound + its lockstep seq are provided).
    if (maxSeq !== null && seqs && (seqs[i] ?? 0) > maxSeq) continue;
    switch (msg.type) {
      case 'request_upsert':
        if (msg.request.response_id === responseId) {
          request = msg.request;
          status = msg.request.status;
          if (msg.request.error) error = msg.request.error;
        }
        break;
      case 'segment_append':
        if (msg.response_id === responseId) {
          segments.push(msg.segment);
          // Carry this segment's arrival seq (the merge watermark). No `seqs` ⇒ null (append-all).
          segmentSeqs.push(seqs ? seqs[i] ?? null : null);
        }
        break;
      case 'event_append':
        if (msg.response_id === responseId) events.push(msg.event);
        break;
      case 'request_status':
        if (msg.response_id === responseId) {
          status = msg.status;
          if (msg.error) error = msg.error;
        }
        break;
      // hello / usage / request_remove / snapshot_done carry no per-flow delta content (the flow's
      // authoritative token usage lives on the flow row, not this monitor join).
      default:
        break;
    }
  }

  return { segments, segmentSeqs, events, request, status, error };
}
