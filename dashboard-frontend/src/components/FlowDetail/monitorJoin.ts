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
  events: DebugTimelineEvent[];
  /** Latest request_upsert for this response (stats/model), if any. */
  request: DebugRequest | null;
  /** Latest status (from request_upsert or request_status), if any. */
  status: DebugRequestStatus | null;
  /** Latest error string seen on this response, if any. */
  error: string | null;
}

const EMPTY: MonitorJoin = { segments: [], events: [], request: null, status: null, error: null };

/**
 * Filters `monitor` to `responseId` and folds it into the per-flow view. Order is preserved
 * (the ring is append-order = arrival order = chronological). Returns a stable empty result when
 * `responseId` is null (a flow not yet linked to an engine response) so callers can render an
 * empty deltas/timeline rather than the whole ring.
 */
export function joinMonitor(monitor: DebugWsMessage[], responseId: string | null | undefined): MonitorJoin {
  if (!responseId) return EMPTY;
  const segments: DebugSegment[] = [];
  const events: DebugTimelineEvent[] = [];
  let request: DebugRequest | null = null;
  let status: DebugRequestStatus | null = null;
  let error: string | null = null;

  for (const msg of monitor) {
    switch (msg.type) {
      case 'request_upsert':
        if (msg.request.response_id === responseId) {
          request = msg.request;
          status = msg.request.status;
          if (msg.request.error) error = msg.request.error;
        }
        break;
      case 'segment_append':
        if (msg.response_id === responseId) segments.push(msg.segment);
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
      // hello / request_remove / snapshot_done carry no per-flow delta content.
      default:
        break;
    }
  }

  return { segments, events, request, status, error };
}
