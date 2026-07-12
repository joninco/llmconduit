/**
 * Pure stream-theater model (D12), kept out of the .tsx so it is unit-testable and the component
 * exports only the component (react-refresh). Folds monitor messages (D3/monitor `segment_append`/
 * `request_upsert`/`request_status`/`request_remove`) into one "river" per stream (`response_id`),
 * each carrying its concatenated output/reasoning/tool text + a derived tokens/s.
 *
 * INCREMENTAL FOLD (theater fix): the fold is a persistent `RiverFold` state fed ONE message at a
 * time by the store's `pushMonitor` — NOT rebuilt from the capped monitor ring. The ring holds only
 * the most recent ~500 messages (the inspector's join window), so a river rebuilt from it LOSES its
 * head as old `segment_append`s are evicted — the theater visibly "deleted tokens from the top",
 * and reasoning (which streams FIRST) vanished entirely on long streams. Folding at arrival keeps
 * the FULL stream text independent of ring eviction. Memory stays bounded by explicit caps below
 * (per-channel char caps with head-trim + a `truncated` flag, a river-count cap), not by the ring.
 *
 * Fold updates are IMMUTABLE (fresh Map + fresh river object per applied message; unchanged rivers
 * share references) so `useSyncExternalStore` snapshots never tear and a captured seek baseline
 * stays frozen via a shallow copy.
 *
 * tokens/s is APPROXIMATE (the monitor deltas are text, not token counts): we estimate tokens as
 * chars/4 and divide the delta since each river's first segment by the elapsed wall time. This is
 * the standard "≈ tokens" heuristic the theater meter shows; the authoritative token totals live
 * on the flow rows. A river with a completed/failed status stops accumulating tok/s (frozen final).
 */
import { assertNever, type DebugWsMessage, type DebugRequestStatus } from '../../api/types';
import { splitToolCallText } from '../../lib/toolCalls';

export interface River {
  /** The stream id (`response_id`). */
  id: string;
  model: string | null;
  status: DebugRequestStatus;
  /** Request start from the monitor clock; distinct from the first emitted segment (TTFT). */
  startedAtMs: number | null;
  /** Terminal failure detail, bounded by `RIVER_ERROR_CHAR_CAP`; null for non-failures. */
  error: string | null;
  /** Concatenated `output` deltas (the bright mono body). */
  output: string;
  /** Concatenated `reasoning` deltas (dim, rendered ABOVE the output — it streams first). */
  reasoning: string;
  /**
   * Tool-call texts, in order (one card each). A streamed tool call arrives as many `segment_append`
   * fragments of its arguments, so stamping each fragment as its own card would shred one call into
   * dozens. We accumulate consecutive tool fragments (the same delta-coalescing D10 applies to text),
   * then split the coalesced run into one card PER DISTINCT call on the backend's
   * `tool arguments <id>:` boundary line (the shared `splitToolCallText` rule). So two back-to-back
   * calls become two cards even with NO interleaving output/reasoning between them (D12 R6), while
   * fragments WITHIN one call stay a single card. An interleaving non-tool segment also closes the
   * current run, as before.
   */
  tools: string[];
  /**
   * True once any channel's HEAD was trimmed by a memory cap (`RIVER_CHANNEL_CHAR_CAP`/
   * `RIVER_TOOL_CHAR_CAP`). The tile then shows an explicit "earlier text trimmed" marker — the
   * caps are a memory bound (AGENTS: bounded dashboard memory), never a silent deletion.
   */
  truncated: boolean;
  /** First/last segment timestamps (ms) seen for this river — the tok/s window. */
  firstMs: number | null;
  lastMs: number | null;
  /**
   * Wall-clock ms the river went TERMINAL (completed/failed), from the monitor's `completed_at_ms`
   * (falling back to the last segment ts). `null` while running. The linger lifecycle computes its
   * REMAINING fade from THIS absolute instant — not from mount time — so a long-finished river is
   * not re-lingered (and re-shown) on every navigation/seek remount (finding 4).
   */
  terminalAtMs: number | null;
  /** Approx tokens emitted (chars/4 across output+reasoning) — the meter numerator. */
  approxTokens: number;
  /** Approx tokens/sec over the river's lifetime (0 until ≥2 timestamps). */
  tokensPerSec: number;
}

/**
 * The accumulating per-river record inside a `RiverFold`. Same shape as the public `River` minus
 * the derived fields (`tools` split, `tokensPerSec`) plus the fold-transient `lastWasTool` run
 * marker; `finalizeRivers` projects it to the public shape.
 */
export interface RiverAccum {
  id: string;
  model: string | null;
  status: DebugRequestStatus;
  startedAtMs: number | null;
  error: string | null;
  output: string;
  reasoning: string;
  /** Coalesced tool RUNS (split into per-call cards only at finalize). */
  toolRuns: string[];
  /** Was the previous folded segment a `tool`? (adjacent tool fragments coalesce into one run). */
  lastWasTool: boolean;
  truncated: boolean;
  firstMs: number | null;
  lastMs: number | null;
  terminalAtMs: number | null;
  approxTokens: number;
}

/** The persistent incremental fold state (held by the dashboard store, fed by `pushMonitor`). */
export interface RiverFold {
  rivers: Map<string, RiverAccum>;
  /** First-seen river order (render order). */
  order: string[];
  /**
   * Newest terminal response, retained independently of the active map. Monitor retention later
   * sends `request_remove`; keeping this single bounded snapshot prevents an idle Theater from
   * going blank while avoiding an unbounded archive of completed bodies.
   */
  lastTerminal: RiverAccum | null;
}

const CHARS_PER_TOKEN = 4;

/**
 * Memory caps (the theater accumulates FULL stream text, so these — not the monitor ring — bound
 * it). Far above any realistic stream (1.5M chars ≈ 375k tokens per channel), so in practice the
 * head is never trimmed; a pathological stream trims oldest-first and flags `truncated`.
 */
export const RIVER_CHANNEL_CHAR_CAP = 1_500_000;
/** When a channel exceeds its cap we keep this many chars (hysteresis, so the O(len) slice is amortized). */
const RIVER_CHANNEL_KEEP = 1_350_000;
/** Total chars across a river's tool runs (oldest runs drop first). */
export const RIVER_TOOL_CHAR_CAP = 300_000;
/** Max retained terminal-error text per river. Failure detail stays useful without unbounded memory. */
export const RIVER_ERROR_CHAR_CAP = 4_000;
/** Max concurrently-tracked rivers; creating past the cap evicts the oldest TERMINAL river first. */
export const MAX_RIVERS = 24;

/** Approx token count for a text delta (chars/4, floored at 0). */
function approxTokensFor(text: string): number {
  return text.length / CHARS_PER_TOKEN;
}

/** A fresh, empty fold (store initial state / `buildRivers` seed). */
export function createRiverFold(): RiverFold {
  return { rivers: new Map(), order: [], lastTerminal: null };
}

function emptyRiver(id: string): RiverAccum {
  return {
    id, model: null, status: 'running', startedAtMs: null, error: null,
    output: '', reasoning: '', toolRuns: [],
    lastWasTool: false, truncated: false, firstMs: null, lastMs: null, terminalAtMs: null,
    approxTokens: 0,
  };
}

/** Preserve the useful head of a terminal error while keeping the incremental fold bounded. */
function capError(error: string | null | undefined): string | null {
  if (!error) return null;
  if (error.length <= RIVER_ERROR_CHAR_CAP) return error;
  return `${error.slice(0, RIVER_ERROR_CHAR_CAP - 1)}…`;
}

/** Head-trim a channel past its cap (keep the newest `RIVER_CHANNEL_KEEP` chars). */
function capChannel(text: string): { text: string; trimmed: boolean } {
  if (text.length <= RIVER_CHANNEL_CHAR_CAP) return { text, trimmed: false };
  return { text: text.slice(text.length - RIVER_CHANNEL_KEEP), trimmed: true };
}

/** Drop oldest tool runs (then head-trim a lone oversized run) past the total tool cap. */
function capToolRuns(runs: string[]): { runs: string[]; trimmed: boolean } {
  let total = 0;
  for (const r of runs) total += r.length;
  if (total <= RIVER_TOOL_CHAR_CAP) return { runs, trimmed: false };
  const next = [...runs];
  while (total > RIVER_TOOL_CHAR_CAP && next.length > 1) {
    total -= next[0]!.length;
    next.shift();
  }
  if (total > RIVER_TOOL_CHAR_CAP && next.length === 1) {
    const lone = next[0]!;
    next[0] = lone.slice(lone.length - RIVER_TOOL_CHAR_CAP);
  }
  return { runs: next, trimmed: true };
}

/**
 * Apply one monitor message to the fold. Returns a NEW fold when the message changed river state
 * (fresh Map + fresh updated river object; untouched rivers share references) and the SAME fold
 * reference for non-river messages (`hello`/`event_append`/`usage`/`snapshot_done`) — so store
 * subscribers can cheaply detect "no river change".
 */
export function foldRiverMessage(fold: RiverFold, msg: DebugWsMessage): RiverFold {
  switch (msg.type) {
    case 'request_upsert': {
      const id = msg.request.response_id;
      const prev = fold.rivers.get(id);
      const base = prev ?? emptyRiver(id);
      const next: RiverAccum = {
        ...base,
        model: msg.request.model,
        status: msg.request.status,
        startedAtMs: msg.request.started_at_ms,
        error: msg.request.status === 'failed' ? capError(msg.request.error ?? base.error) : null,
        // Record the terminal instant from the monitor's own clock (a replayed/already-finished
        // flow upserts as terminal) so the linger fade counts from when it ACTUALLY finished.
        terminalAtMs:
          msg.request.status === 'running'
            ? null
            : (msg.request.completed_at_ms ?? base.terminalAtMs ?? base.lastMs ?? msg.request.updated_at_ms),
      };
      return insertRiver(fold, next, prev !== undefined);
    }
    case 'segment_append': {
      const id = msg.response_id;
      const prev = fold.rivers.get(id);
      const base = prev ?? emptyRiver(id);
      const t = msg.segment.timestamp_ms;
      const next: RiverAccum = { ...base, firstMs: base.firstMs ?? t, lastMs: t };
      if (msg.segment.kind === 'output') {
        const capped = capChannel(base.output + msg.segment.text);
        next.output = capped.text;
        next.truncated = base.truncated || capped.trimmed;
        next.approxTokens = base.approxTokens + approxTokensFor(msg.segment.text);
        next.lastWasTool = false; // a non-tool segment ends the current tool card's run.
      } else if (msg.segment.kind === 'reasoning') {
        const capped = capChannel(base.reasoning + msg.segment.text);
        next.reasoning = capped.text;
        next.truncated = base.truncated || capped.trimmed;
        next.approxTokens = base.approxTokens + approxTokensFor(msg.segment.text);
        next.lastWasTool = false; // a non-tool segment ends the current tool card's run.
      } else {
        // Coalesce consecutive tool fragments into the current run; a fresh run only opens when
        // the previous segment was NOT a tool (D12 R5 MED). Distinct calls that arrive BACK-TO-BACK
        // (no interleaving non-tool segment) land in the same run here and are split apart at
        // finalize by their `tool arguments <id>:` boundary line (D12 R6).
        const toolRuns = base.lastWasTool
          ? [...base.toolRuns.slice(0, -1), base.toolRuns[base.toolRuns.length - 1] + msg.segment.text]
          : [...base.toolRuns, msg.segment.text];
        const capped = capToolRuns(toolRuns);
        next.toolRuns = capped.runs;
        next.truncated = base.truncated || capped.trimmed;
        next.lastWasTool = true;
      }
      return insertRiver(fold, next, prev !== undefined);
    }
    case 'request_status': {
      const id = msg.response_id;
      const prev = fold.rivers.get(id);
      const base = prev ?? emptyRiver(id);
      const next: RiverAccum = {
        ...base,
        status: msg.status,
        error: msg.status === 'failed' ? capError(msg.error ?? base.error) : null,
        // A terminal status stamps the river's finish instant (the monitor's `completed_at_ms`,
        // falling back to its last segment ts); returning to running clears it.
        terminalAtMs:
          msg.status === 'running' ? null : (msg.completed_at_ms ?? base.terminalAtMs ?? base.lastMs),
      };
      return insertRiver(fold, next, prev !== undefined);
    }
    case 'request_remove': {
      if (!fold.rivers.has(msg.response_id)) return fold;
      const rivers = new Map(fold.rivers);
      rivers.delete(msg.response_id);
      // `lastTerminal` intentionally survives monitor eviction: it is the Theater's one-response
      // idle retention, replaced by the next terminal response and cleared only with the store.
      return { rivers, order: fold.order.filter((id) => id !== msg.response_id), lastTerminal: fold.lastTerminal };
    }
    // These arms carry no river body — intentionally ignored, but enumerated so a NEW protocol
    // arm (added to the `DebugWsMessage` union) is a COMPILE error here, not a silent drop
    // (finding 11). `hello` is the handshake; `event_append` feeds the inspector timeline (not the
    // theater); `usage` is the D3 cumulative-token echo (the authoritative totals live on the flow
    // rows, not the theater meter, which derives ≈tok/s from segment text); `snapshot_done` marks
    // end-of-replay.
    case 'hello':
    case 'event_append':
    case 'usage':
    case 'snapshot_done':
      return fold;
    default:
      return assertNever(msg);
  }
}

/**
 * Install an updated river into a fresh fold. A NEW river (not previously present) appends to the
 * order and, past `MAX_RIVERS`, evicts the oldest TERMINAL river (never a running one unless every
 * tracked river is running — the backend caps concurrency long before that).
 */
function insertRiver(fold: RiverFold, river: RiverAccum, existed: boolean): RiverFold {
  const rivers = new Map(fold.rivers);
  let order = fold.order;
  if (!existed) {
    order = [...order, river.id];
    if (rivers.size >= MAX_RIVERS) {
      const victim =
        order.find((id) => id !== river.id && rivers.get(id)?.status !== 'running') ??
        order.find((id) => id !== river.id);
      if (victim !== undefined) {
        rivers.delete(victim);
        order = order.filter((id) => id !== victim);
      }
    }
  }
  rivers.set(river.id, river);
  return {
    rivers,
    order,
    // A terminal river becomes the retained response. Later terminal segments update this same
    // snapshot; running activity leaves the previous completed response available for idle mode.
    lastTerminal: river.status === 'running' ? fold.lastTerminal : river,
  };
}

/** Project one accumulator into the public/render-ready river shape. */
function finalizeRiver(r: RiverAccum): River {
  // A still-running river with a single timestamp has no measurable rate yet (0); a completed
  // river keeps its final rate.
  const tokensPerSec =
    r.firstMs != null && r.lastMs != null && r.lastMs > r.firstMs
      ? r.approxTokens / ((r.lastMs - r.firstMs) / 1000)
      : 0;
  return {
    id: r.id,
    model: r.model,
    status: r.status,
    startedAtMs: r.startedAtMs,
    error: r.error,
    output: r.output,
    reasoning: r.reasoning,
    tools: r.toolRuns.flatMap(splitToolCallText),
    truncated: r.truncated,
    firstMs: r.firstMs,
    lastMs: r.lastMs,
    terminalAtMs: r.status !== 'running' && r.terminalAtMs == null ? r.lastMs : r.terminalAtMs,
    approxTokens: r.approxTokens,
    tokensPerSec,
  };
}

/**
 * Project the fold into render-ready rivers (first-seen order): split each coalesced tool run into
 * one card per DISTINCT call on the backend's `tool arguments <id>:` boundary (shared rule with
 * DeltasPanel — D12 R6), derive tok/s from the accumulated window, and give a terminal river whose
 * status carried no `completed_at_ms` a finish instant from its last segment so the linger fade has
 * an absolute anchor (finding 4). Pure — memoize on the fold reference.
 */
export function finalizeRivers(fold: RiverFold): River[] {
  return fold.order.map((id) => finalizeRiver(fold.rivers.get(id)!));
}

/** The one terminal response retained for Theater idle mode, including after `request_remove`. */
export function finalizeLastTerminalRiver(fold: RiverFold): River | null {
  return fold.lastTerminal ? finalizeRiver(fold.lastTerminal) : null;
}

/**
 * One-shot fold of a message array into rivers — the original D12 entry point, kept for tests and
 * any caller that has a full message list in hand. The LIVE theater does NOT use this off the
 * monitor ring anymore (the ring's cap would truncate long streams — see header); it reads the
 * store's incrementally-fed `riverFold` instead.
 */
export function buildRivers(monitor: DebugWsMessage[]): River[] {
  let fold = createRiverFold();
  for (const msg of monitor) fold = foldRiverMessage(fold, msg);
  return finalizeRivers(fold);
}

/** Grid template for N rivers: 1 → big, 2 → split, 3-6 → multi-grid (cols clamp at 3). */
export function gridColumns(n: number): number {
  if (n <= 1) return 1;
  if (n === 2) return 2;
  return 3; // 3-6 rivers tile into a 3-wide grid (2 rows at 6).
}

/**
 * Split a tool-card string into a human prefix + a parseable JSON tail (U8). Tool lines arrive
 * as `tool arguments <id>: {"command": …}` — the tail is what deserves pretty-printing. Tries
 * successive `{`/`[` positions (R2: a brace inside the tool id must not kill pretty-printing of
 * the real object that follows), bounded to a few attempts. Returns null when nothing parses
 * (the card renders as plain text).
 */
export function splitJsonTail(text: string): { prefix: string; value: unknown } | null {
  const candidates: number[] = [];
  for (let from = 0; candidates.length < 8; ) {
    const brace = text.indexOf('{', from);
    const bracket = text.indexOf('[', from);
    const at = brace === -1 ? bracket : bracket === -1 ? brace : Math.min(brace, bracket);
    if (at === -1) break;
    candidates.push(at);
    from = at + 1;
  }
  for (const at of candidates.slice(0, 4)) {
    try {
      return { prefix: text.slice(0, at).trimEnd(), value: JSON.parse(text.slice(at).trim()) };
    } catch {
      // try the next opener
    }
  }
  return null;
}
