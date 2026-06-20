/**
 * DashboardSocket — the single WS pipe (`/dashboard/ws`).
 *
 * Responsibilities (D7/D9):
 *  - snapshot-then-live: the first server message is a full `SnapshotFrame`; everything
 *    after is a batched `DashboardFrame`.
 *  - decode the batched envelope `DashboardFrame { domain, seq, batch }`.
 *  - **per-domain whole-frame dedup**: a frame with `seq <= last_seq[domain]` is dropped
 *    WHOLESALE (the entire batch); `seq > last_seq[domain]` is processed and advances the
 *    cursor. Because the Monitor frame carries ONE envelope per `DebugUpdate` (its `batch`
 *    = all sibling `DebugWsMessage`s under one `sequence`), no sibling is ever dropped.
 *  - feed the zustand dashboard store.
 *  - D11 time-travel: `seek()` pauses applying live frames and shadow-buffers them;
 *    `live()` resumes by replaying the buffered frames in order.
 *
 * Transport-agnostic: a `WebSocketFactory` is injected so the mock + tests supply a
 * fake socket. The dedup/apply logic is the unit under test.
 */
import type {
  DashboardFrame,
  DashboardPayload,
  Domain,
  SnapshotFrame,
  WsServerMessage,
} from './types';
import { assertNever } from './types';
import { dashboardStore } from '../store/dashboardStore';

/** Minimal structural subset of `WebSocket` we depend on (eases mocking). */
export interface WsLike {
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onclose: ((ev: { code?: number } | undefined) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
}

export type WebSocketFactory = (url: string) => WsLike;

export interface DashboardSocketOptions {
  url?: string;
  factory?: WebSocketFactory;
  /** Called on a WS-level 401/close-with-auth so the shell bounces to login. */
  onUnauthorized?: () => void;
  /** Store the socket feeds. Defaults to the singleton dashboard store. */
  store?: typeof dashboardStore;
}

type LastSeq = Record<Domain, number>;

export class DashboardSocket {
  private readonly url: string;
  private readonly factory: WebSocketFactory;
  private readonly onUnauthorized: (() => void) | undefined;
  private readonly store: typeof dashboardStore;

  private ws: WsLike | null = null;
  /** Per-domain dedup cursors. */
  private lastSeq: LastSeq = { flow: 0, metrics: 0, topology: 0, monitor: 0 };
  /** Whether the initial snapshot has been applied (gates live frames). */
  private snapshotApplied = false;

  /** Time-travel: when paused, live frames are buffered here instead of applied. */
  private paused = false;
  private shadowBuffer: DashboardFrame[] = [];

  constructor(opts: DashboardSocketOptions = {}) {
    this.url = opts.url ?? defaultWsUrl();
    this.factory = opts.factory ?? ((u: string) => new WebSocket(u) as unknown as WsLike);
    this.onUnauthorized = opts.onUnauthorized;
    this.store = opts.store ?? dashboardStore;
  }

  /** Opens the socket and wires handlers. Idempotent if already connected. */
  connect(): void {
    if (this.ws) return;
    this.store.getState().setConnection('connecting');
    const ws = this.factory(this.url);
    this.ws = ws;
    ws.onopen = () => {
      // Live state begins after the snapshot is applied; mark 'live' on first snapshot.
    };
    ws.onmessage = (ev) => this.handleRaw(ev.data);
    ws.onclose = (ev) => {
      // 4401 is our convention for an auth/expiry close (cookie exp passed, bad origin).
      if (ev?.code === 4401) {
        this.onUnauthorized?.();
        this.store.getState().setConnection('error');
      } else {
        this.store.getState().setConnection('closed');
      }
      this.ws = null;
    };
    ws.onerror = () => {
      this.store.getState().setConnection('error');
    };
  }

  /** Closes the socket and resets dedup/snapshot/buffer state. */
  disconnect(): void {
    this.ws?.close();
    this.ws = null;
    this.snapshotApplied = false;
    this.paused = false;
    this.shadowBuffer = [];
    this.lastSeq = { flow: 0, metrics: 0, topology: 0, monitor: 0 };
  }

  /** Current per-domain dedup cursors (for tests / display). */
  getCursors(): LastSeq {
    return { ...this.lastSeq };
  }

  // -- Time travel (D11) ----------------------------------------------------

  /** Pause applying live frames; subsequent frames accumulate in the shadow buffer. */
  seek(): void {
    this.paused = true;
    this.store.getState().setConnection('seeking');
  }

  /**
   * Resume LIVE: replay every buffered frame in arrival order (dedup still applies),
   * then clear the buffer and go back to applying frames as they arrive.
   */
  live(): void {
    this.paused = false;
    const buffered = this.shadowBuffer;
    this.shadowBuffer = [];
    for (const frame of buffered) {
      this.applyFrame(frame);
    }
    this.store.getState().setConnection('live');
  }

  isPaused(): boolean {
    return this.paused;
  }

  shadowBufferLength(): number {
    return this.shadowBuffer.length;
  }

  // -- Decode + dispatch ----------------------------------------------------

  /** Decodes a raw WS payload (string or already-parsed object) and routes it. */
  private handleRaw(data: unknown): void {
    let msg: WsServerMessage;
    try {
      msg = typeof data === 'string' ? (JSON.parse(data) as WsServerMessage) : (data as WsServerMessage);
    } catch {
      // Malformed frame: ignore (defensive — a bad frame must not crash the pipe).
      return;
    }
    this.handleMessage(msg);
  }

  /** Public for tests: route a decoded server message (snapshot or batched frame). */
  handleMessage(msg: WsServerMessage): void {
    if (isSnapshot(msg)) {
      this.applySnapshotMessage(msg);
      return;
    }
    // A live frame before the snapshot is buffered until the snapshot lands.
    if (!this.snapshotApplied) {
      this.shadowBuffer.push(msg);
      return;
    }
    if (this.paused) {
      this.shadowBuffer.push(msg);
      return;
    }
    this.applyFrame(msg);
  }

  private applySnapshotMessage(snap: SnapshotFrame): void {
    this.store.getState().applySnapshot({
      cursors: snap.cursors,
      flows: snap.flows,
      metrics: snap.metrics,
      topology: snap.topology,
    });
    this.lastSeq = {
      flow: snap.cursors.flow_seq,
      metrics: snap.cursors.metrics_seq,
      topology: snap.cursors.topology_seq,
      monitor: snap.cursors.monitor_seq,
    };
    this.snapshotApplied = true;
    this.store.getState().setConnection('live');

    // Drain any pre-snapshot frames that arrived early.
    const early = this.shadowBuffer;
    this.shadowBuffer = [];
    for (const frame of early) {
      if (!this.paused) this.applyFrame(frame);
      else this.shadowBuffer.push(frame);
    }
  }

  /**
   * Applies one batched frame with per-domain whole-frame dedup. Returns true if the
   * frame was applied, false if it was dropped as stale.
   */
  applyFrame(frame: DashboardFrame): boolean {
    const cursor = this.lastSeq[frame.domain];
    // Whole-frame dedup: a stale or duplicate seq drops the ENTIRE batch.
    if (frame.seq <= cursor) {
      return false;
    }
    this.lastSeq[frame.domain] = frame.seq;
    const store = this.store.getState();
    store.setCursor(domainToCursorKey(frame.domain), frame.seq);
    // Every payload in the batch is applied — no sibling dropped.
    for (const payload of frame.batch) {
      this.applyPayload(payload);
    }
    return true;
  }

  /** Exhaustive dispatch over the `DashboardPayload` union (no `any`, no fallthrough). */
  private applyPayload(payload: DashboardPayload): void {
    const store = this.store.getState();
    switch (payload.type) {
      case 'monitor':
        store.pushMonitor(payload.message);
        return;
      case 'usage':
        store.patchUsage(payload.response_id, {
          prompt: payload.prompt,
          completion: payload.completion,
          total: payload.total,
          cached: payload.cached,
          reasoning: payload.reasoning,
        });
        return;
      case 'metric_tick':
        store.setMetrics({
          metrics_seq: this.lastSeq.metrics,
          reqs_per_sec: payload.reqs_per_sec,
          active_streams: payload.active_streams,
          error_pct: payload.error_pct,
          p50: payload.p50,
          p95: payload.p95,
          p99: payload.p99,
          tokens_per_sec: payload.tokens_per_sec,
          cost_per_min: payload.cost_per_min,
          windows: payload.windows,
        });
        return;
      case 'flow_status':
        store.patchFlowStatus({
          response_id: payload.response_id,
          status: payload.status,
          served_model: payload.served_model,
          upstream_target: payload.upstream_target,
          usage: payload.usage,
          elapsed_ms: payload.elapsed_ms,
        });
        return;
      case 'topology_update':
        store.setTopology(payload.nodes, payload.edges);
        return;
      default:
        // Compile-time exhaustiveness: a new arm without a case is a TS error here.
        assertNever(payload);
    }
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function isSnapshot(msg: WsServerMessage): msg is SnapshotFrame {
  return (msg as SnapshotFrame).type === 'snapshot';
}

function domainToCursorKey(domain: Domain): 'flow_seq' | 'metrics_seq' | 'topology_seq' | 'monitor_seq' {
  switch (domain) {
    case 'flow':
      return 'flow_seq';
    case 'metrics':
      return 'metrics_seq';
    case 'topology':
      return 'topology_seq';
    case 'monitor':
      return 'monitor_seq';
    default:
      return assertNever(domain);
  }
}

/** Builds the default WS URL from the current origin (ws/wss to match http/https). */
function defaultWsUrl(): string {
  if (typeof window === 'undefined') return 'ws://localhost/dashboard/ws';
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${window.location.host}/dashboard/ws`;
}
