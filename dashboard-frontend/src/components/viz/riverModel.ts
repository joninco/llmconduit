/**
 * Pure stream-theater model (D12), kept out of the .tsx so it is unit-testable and the component
 * exports only the component (react-refresh). Folds the live monitor ring (`DebugWsMessage[]`,
 * D3/monitor `segment_append`/`request_upsert`/`request_status`) into one "river" per stream
 * (`response_id`), each carrying its concatenated output/reasoning/tool text + a derived tokens/s.
 *
 * tokens/s is APPROXIMATE (the monitor deltas are text, not token counts): we estimate tokens as
 * chars/4 and divide the delta since each river's first segment by the elapsed wall time. This is
 * the standard "≈ tokens" heuristic the theater meter shows; the authoritative token totals live
 * on the flow rows. A river with a completed/failed status stops accumulating tok/s (frozen final).
 */
import { assertNever, type DebugWsMessage, type DebugRequestStatus } from '../../api/types';

export interface River {
  /** The stream id (`response_id`). */
  id: string;
  model: string | null;
  status: DebugRequestStatus;
  /** Concatenated `output` deltas (the bright mono body). */
  output: string;
  /** Concatenated `reasoning` deltas (dim, collapsible). */
  reasoning: string;
  /** Tool-call segment texts, in order (rendered as cards). */
  tools: string[];
  /** First/last segment timestamps (ms) seen for this river — the tok/s window. */
  firstMs: number | null;
  lastMs: number | null;
  /** Approx tokens emitted (chars/4 across output+reasoning) — the meter numerator. */
  approxTokens: number;
  /** Approx tokens/sec over the river's lifetime (0 until ≥2 timestamps). */
  tokensPerSec: number;
}

const CHARS_PER_TOKEN = 4;

/** Approx token count for a text delta (chars/4, floored at 0). */
function approxTokensFor(text: string): number {
  return text.length / CHARS_PER_TOKEN;
}

/**
 * Fold the monitor ring into rivers keyed by `response_id`. `request_upsert` seeds a river (model
 * + status); `segment_append` appends to the matching channel and advances the tok/s window;
 * `request_status` updates the terminal status; `request_remove` drops the river. Order is the
 * ring order (arrival), so the newest activity is last. Returns rivers in first-seen order.
 */
export function buildRivers(monitor: DebugWsMessage[]): River[] {
  const rivers = new Map<string, River>();
  const order: string[] = [];

  const ensure = (id: string): River => {
    let r = rivers.get(id);
    if (!r) {
      r = {
        id, model: null, status: 'running', output: '', reasoning: '', tools: [],
        firstMs: null, lastMs: null, approxTokens: 0, tokensPerSec: 0,
      };
      rivers.set(id, r);
      order.push(id);
    }
    return r;
  };

  for (const msg of monitor) {
    switch (msg.type) {
      case 'request_upsert': {
        const r = ensure(msg.request.response_id);
        r.model = msg.request.model;
        r.status = msg.request.status;
        break;
      }
      case 'segment_append': {
        const r = ensure(msg.response_id);
        const t = msg.segment.timestamp_ms;
        if (r.firstMs == null) r.firstMs = t;
        r.lastMs = t;
        if (msg.segment.kind === 'output') {
          r.output += msg.segment.text;
          r.approxTokens += approxTokensFor(msg.segment.text);
        } else if (msg.segment.kind === 'reasoning') {
          r.reasoning += msg.segment.text;
          r.approxTokens += approxTokensFor(msg.segment.text);
        } else {
          r.tools.push(msg.segment.text);
        }
        break;
      }
      case 'request_status': {
        const r = ensure(msg.response_id);
        r.status = msg.status;
        break;
      }
      case 'request_remove': {
        if (rivers.delete(msg.response_id)) {
          const i = order.indexOf(msg.response_id);
          if (i >= 0) order.splice(i, 1);
        }
        break;
      }
      // These arms carry no river body — intentionally ignored, but enumerated so a NEW protocol
      // arm (added to the `DebugWsMessage` union) is a COMPILE error here, not a silent drop
      // (finding 11). `hello` is the handshake; `event_append` feeds the inspector timeline (not the
      // theater); `snapshot_done` marks end-of-replay.
      case 'hello':
      case 'event_append':
      case 'snapshot_done':
        break;
      default:
        assertNever(msg);
    }
  }

  // Derive tok/s once per river from its accumulated window. A still-running river with a single
  // timestamp has no measurable rate yet (0); a completed river keeps its final rate.
  for (const r of rivers.values()) {
    if (r.firstMs != null && r.lastMs != null && r.lastMs > r.firstMs) {
      r.tokensPerSec = r.approxTokens / ((r.lastMs - r.firstMs) / 1000);
    }
  }
  return order.map((id) => rivers.get(id)!);
}

/** Grid template for N rivers: 1 → big, 2 → split, 3-6 → multi-grid (cols clamp at 3). */
export function gridColumns(n: number): number {
  if (n <= 1) return 1;
  if (n === 2) return 2;
  return 3; // 3-6 rivers tile into a 3-wide grid (2 rows at 6).
}
