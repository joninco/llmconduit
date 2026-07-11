/**
 * DashboardSocket — the single WS pipe (`/dashboard/ws`).
 *
 * Responsibilities (D7/D9):
 *  - snapshot-then-live: the first server message is a full `SnapshotFrame`; everything
 *    after is a batched `DashboardFrame`.
 *  - decode the batched envelope `DashboardFrame { domain, seq, batch }`.
 *  - **validate before applying** (finding 6): the WHOLE frame (envelope + every payload
 *    arm) is validated against the runtime guards BEFORE any cursor or store mutation, so
 *    a malformed frame is dropped wholesale WITHOUT advancing the cursor (stays replayable).
 *  - **per-domain whole-frame dedup**: a frame with `seq <= last_seq[domain]` is dropped
 *    WHOLESALE (the entire batch); `seq > last_seq[domain]` is processed and advances the
 *    cursor. The Monitor frame carries ONE envelope per `DebugUpdate` (its `batch` = all
 *    sibling `DebugWsMessage`s under one `sequence`), so no sibling is ever dropped.
 *  - feed the zustand dashboard store; notify `onFrameApplied(frame)` AFTER an accepted
 *    frame so the composition root can narrowly invalidate terminal-flow detail reads.
 *  - auth failure vs. transient blip (finding 7): an EXPLICIT `4401` close → bounce to
 *    login. Any OTHER abnormal close/error is treated as a transient network blip: the
 *    socket schedules a reconnect (capped backoff) AND, to detect a silently-expired
 *    session, runs a protected HTTP probe (`probeAuth`) — a `401` from the probe bounces
 *    to login; otherwise it reconnects. A valid session therefore survives a 1006 blip.
 *  - reconnect safety (finding 4): each opened socket carries a generation id; every
 *    callback is guarded by an identity check so a late `close`/`error` from an OLD socket
 *    cannot clobber a freshly reconnected one (StrictMode mount→unmount→remount).
 *  - D11 time-travel: `seek()` pauses applying live frames and shadow-buffers them;
 *    `live()` resumes by replaying the buffered frames in order.
 *
 * Transport-agnostic: a `WebSocketFactory` is injected so the mock + tests supply a
 * fake socket. The validate/dedup/apply logic is the unit under test.
 */
import type {
  DashboardFrame,
  DashboardPayload,
  Domain,
  SnapshotFrame,
} from './types';
import { assertNever, isDashboardFrame, isSnapshotFrame } from './types';
import { dashboardStore, type LiveBaseline } from '../store/dashboardStore';
import { assertDashboardSchemaVersion, DashboardSchemaMismatchError } from './schemaVersion';

/** Clean WS close code (RFC 6455 §7.4.1). A REMOTE 1000 is still reconnectable. */
const WS_NORMAL_CLOSE = 1000;
/** Our convention for an explicit auth/expiry close from the server (→ bounce to login). */
const WS_AUTH_CLOSE = 4401;
/** Client-side close used when the no-frame watchdog declares the transport stale. */
const WS_STALE_CLOSE = 4000;
/** Reconnect backoff schedule (ms) for transient blips; index clamps at the last entry. */
const RECONNECT_BACKOFF_MS = [500, 1000, 2000, 5000, 10000];
/** A connected socket that delivers no frame for this long is considered half-open. */
const NO_FRAME_WATCHDOG_MS = 15_000;
/** The seek feed is bounded independently by bytes, frames, and elapsed wall time. */
const SHADOW_MAX_BYTES = 8 * 1024 * 1024;
const SHADOW_MAX_FRAMES = 5_000;
const SHADOW_MAX_AGE_MS = 5 * 60 * 1_000;
/** Large replays yield between batches so returning LIVE cannot monopolize the main thread. */
const REPLAY_BATCH_FRAMES = 250;

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
  /** Called ONLY on a confirmed auth failure (explicit 4401 close, or a probe `401`). */
  onUnauthorized?: () => void;
  /** Fired AFTER an accepted (post-dedup) frame so the caller can invalidate REST queries. */
  onFrameApplied?: (frame: DashboardFrame) => void;
  /**
   * Protected-endpoint auth probe used after a TRANSIENT abnormal close (finding 7).
   * Resolves `true` if the session is still valid (→ reconnect), `false` if it returned
   * `401` (→ bounce to login). If omitted, a transient blip just reconnects (no probe).
   */
  probeAuth?: () => Promise<boolean>;
  /** Whether to auto-reconnect on transient blips. Default true. Disabled in unit tests. */
  autoReconnect?: boolean;
  /** Injected timer (test seam). Defaults to `setTimeout`. */
  setTimer?: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimer?: (h: ReturnType<typeof setTimeout>) => void;
  /** Store the socket feeds. Defaults to the singleton dashboard store. */
  store?: typeof dashboardStore;
  /** No-frame timeout. Set to 0 only in deterministic tests that do not model time. */
  watchdogMs?: number;
  /** Clock and animation-frame seams for bounded seek/replay tests. */
  now?: () => number;
  raf?: (cb: () => void) => number;
  cancelRaf?: (handle: number) => void;
  /** Override only for deterministic boundary tests; production uses the fixed safe limits. */
  shadowLimits?: { bytes: number; frames: number; ageMs: number };
}

type LastSeq = Record<Domain, number>;

export class DashboardSocket {
  private readonly url: string;
  private readonly factory: WebSocketFactory;
  private readonly onUnauthorized: (() => void) | undefined;
  private readonly onFrameApplied: ((frame: DashboardFrame) => void) | undefined;
  private readonly probeAuth: (() => Promise<boolean>) | undefined;
  private readonly autoReconnect: boolean;
  private readonly setTimer: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  private readonly clearTimer: (h: ReturnType<typeof setTimeout>) => void;
  private readonly store: typeof dashboardStore;
  private readonly watchdogMs: number;
  private readonly now: () => number;
  private readonly raf: (cb: () => void) => number;
  private readonly cancelRaf: (handle: number) => void;
  private readonly shadowLimits: { bytes: number; frames: number; ageMs: number };

  private ws: WsLike | null = null;
  /** Monotonic id of the CURRENT socket; stale-callback guard compares against it. */
  private generation = 0;
  /** Whether the current socket reached a terminal close (so error after that is ignored). */
  private closedCleanly = false;
  /** Set true by `disconnect()` so a pending reconnect/probe is abandoned. */
  private stopped = false;
  /** Consecutive transient reconnect attempts (drives the backoff index). */
  private reconnectAttempts = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private watchdogTimer: ReturnType<typeof setTimeout> | null = null;
  private watchdogNonce = 0;
  private replayFrame: number | null = null;

  /** Per-domain dedup cursors. */
  private lastSeq: LastSeq = { flow: 0, metrics: 0, topology: 0, monitor: 0 };
  /** Whether the initial snapshot has been applied (gates live frames). */
  private snapshotApplied = false;

  /** Time-travel: when paused, live frames are buffered here instead of applied. */
  private paused = false;
  private shadowBuffer: DashboardFrame[] = [];
  private shadowBufferBytes = 0;
  private shadowBufferStartedAtMs: number | null = null;
  private resyncRequired = false;
  /**
   * A snapshot that arrived from a RECONNECT while seeking (finding 6). It is STAGED here
   * rather than applied, so a reconnect-during-seek does not clobber the frozen historical
   * cut or flip the connection to `live`. `live()` applies it on explicit resume.
   */
  private pendingSnapshot: SnapshotFrame | null = null;
  /**
   * The LIVE store state captured the instant `seek()` paused the feed (D11 R2 finding 1). The
   * Scrubber's `applySeekCut` overwrites the store with the frozen historical cut, so on `live()`
   * (when no reconnect snapshot re-baselined the store) this is restored FIRST — re-establishing the
   * up-to-date live rows/cursors/monitor — before the shadow-buffered frames replay. Without it,
   * resuming would replay onto the frozen cut, leaving rows/cursors that existed between the cut and
   * the pause missing or rewound while `connection==='live'`.
   */
  private liveBaseline: LiveBaseline | null = null;

  constructor(opts: DashboardSocketOptions = {}) {
    this.url = opts.url ?? defaultWsUrl();
    this.factory = opts.factory ?? ((u: string) => new WebSocket(u) as unknown as WsLike);
    this.onUnauthorized = opts.onUnauthorized;
    this.onFrameApplied = opts.onFrameApplied;
    this.probeAuth = opts.probeAuth;
    this.autoReconnect = opts.autoReconnect ?? true;
    this.setTimer = opts.setTimer ?? ((cb, ms) => setTimeout(cb, ms));
    this.clearTimer = opts.clearTimer ?? ((h) => clearTimeout(h));
    this.store = opts.store ?? dashboardStore;
    this.watchdogMs = opts.watchdogMs ?? NO_FRAME_WATCHDOG_MS;
    this.now = opts.now ?? (() => Date.now());
    this.raf = opts.raf ?? ((cb) => (typeof requestAnimationFrame === 'function'
      ? requestAnimationFrame(cb)
      : (setTimeout(cb, 0) as unknown as number)));
    this.cancelRaf = opts.cancelRaf ?? ((handle) => (typeof cancelAnimationFrame === 'function'
      ? cancelAnimationFrame(handle)
      : clearTimeout(handle)));
    this.shadowLimits = opts.shadowLimits ?? {
      bytes: SHADOW_MAX_BYTES,
      frames: SHADOW_MAX_FRAMES,
      ageMs: SHADOW_MAX_AGE_MS,
    };
  }

  /** Opens the socket and wires handlers. Idempotent if already connected. */
  connect(): void {
    if (this.ws) return;
    this.stopped = false;
    // (D11 R4 finding 1) Do NOT flip to `'connecting'` while a seek is active: a reconnect timer
    // re-entering `connect()` mid-seek must not clear the frozen cut (`seekAtMs`/`seekMonitorSeq`).
    // The fresh socket's snapshot is STAGED (`applySnapshotMessage`), not applied, until `live()`.
    if (!this.paused) this.store.getState().setConnection('connecting');
    const ws = this.factory(this.url);
    // This socket's identity. Every callback below checks `this.ws === ws` (and the
    // generation) so a late event from a REPLACED socket is ignored (finding 4).
    const gen = ++this.generation;
    this.ws = ws;
    this.closedCleanly = false;

    const isCurrent = () => this.ws === ws && this.generation === gen;

    ws.onopen = () => {
      if (!isCurrent()) return;
      // A successful open clears the transient-reconnect backoff.
      this.reconnectAttempts = 0;
      this.armWatchdog(ws, gen);
      // Live state begins after the snapshot is applied; marked 'live' there.
    };
    ws.onmessage = (ev) => {
      if (!isCurrent()) return;
      // Any received frame proves the transport is alive, even if contract validation later drops
      // it. Re-arm before parsing so a malformed application frame cannot create a reconnect loop.
      this.armWatchdog(ws, gen);
      this.handleRaw(ev.data);
    };
    ws.onclose = (ev) => {
      // A close from a stale socket must NOT touch current state (finding 4).
      if (!isCurrent()) return;
      const code = ev?.code;
      this.detach(ws);
      this.ws = null;
      this.closedCleanly = true;
      if (code === WS_AUTH_CLOSE) {
        // EXPLICIT auth/expiry close → confirmed auth failure → bounce to login.
        this.bounceToLogin();
      } else {
        // Every REMOTE close, including code 1000, is reconnectable. `disconnect()` detaches the
        // handlers and bumps the generation before issuing its own 1000, so only a remote clean
        // close reaches this branch. Probe before reconnecting to distinguish expiry from a blip.
        this.handleTransientDrop();
      }
    };
    ws.onerror = () => {
      if (!isCurrent()) return;
      // An error often precedes a close; if a close already handled it, skip. Otherwise
      // treat as a transient drop (probe + reconnect), NOT an automatic logout.
      if (this.closedCleanly) return;
      this.detach(ws);
      this.ws = null;
      this.closedCleanly = true;
      this.handleTransientDrop();
    };
  }

  /** Closes the socket and resets dedup/snapshot/buffer/reconnect state. */
  disconnect(): void {
    this.stopped = true;
    this.clearWatchdog();
    if (this.reconnectTimer !== null) {
      this.clearTimer(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const ws = this.ws;
    if (ws) {
      this.detach(ws);
      this.ws = null;
      // Bump generation so any in-flight callback for `ws` is treated as stale.
      this.generation++;
      try {
        ws.close(WS_NORMAL_CLOSE);
      } catch {
        // ignore — already closing/closed
      }
    }
    this.snapshotApplied = false;
    this.paused = false;
    this.clearShadowBuffer();
    this.pendingSnapshot = null;
    this.liveBaseline = null;
    this.resyncRequired = false;
    this.store.getState().setResyncRequired(false);
    if (this.replayFrame !== null) {
      this.cancelRaf(this.replayFrame);
      this.replayFrame = null;
    }
    this.reconnectAttempts = 0;
    this.lastSeq = { flow: 0, metrics: 0, topology: 0, monitor: 0 };
  }

  /** Confirmed auth failure: mark error + bounce to login (no reconnect). */
  private bounceToLogin(): void {
    this.store.getState().setConnection('error');
    this.onUnauthorized?.();
  }

  /**
   * Handle a transient abnormal drop (finding 7): if a `probeAuth` is configured, probe
   * the protected endpoint — a `401` (resolve `false`) bounces to login; anything else
   * reconnects. With no probe, just reconnect. `disconnect()` (stopped) cancels both.
   *
   * The probe is BOUND to the generation that initiated it (finding 5): if a
   * disconnect/remount bumps the generation before the probe resolves, the stale result is
   * ignored — it must NOT log out (or reconnect) a NEWER connection.
   */
  private handleTransientDrop(): void {
    if (this.stopped) return;
    const probeGen = this.generation;
    const isStaleProbe = () => this.stopped || this.generation !== probeGen;
    // (D11 R4 finding 1) PRESERVE the seek freeze across a transient transport drop. While paused,
    // the store holds the FROZEN seek cut (`seekAtMs`/`seekMonitorSeq`); flipping to `'connecting'`
    // would clear those fields (setConnection nulls them) and yank D10/D12 off the frozen view
    // BEFORE the user pressed LIVE. The transport state (reconnect/probe below) is decoupled from
    // the seek/paused intent: the reconnect still runs and any fresh snapshot is STAGED
    // (`applySnapshotMessage` → `pendingSnapshot`), never applied over the frozen cut. The freeze
    // exits ONLY on an explicit `live()` or a confirmed auth failure (401/4401).
    if (!this.paused) this.store.getState().setConnection('connecting');
    if (this.probeAuth) {
      this.probeAuth()
        .then((authed) => {
          if (isStaleProbe()) return; // a newer connection superseded this probe
          if (authed) this.scheduleReconnect();
          else this.bounceToLogin();
        })
        .catch(() => {
          // Probe failed for a non-auth reason (e.g. network) → treat as transient.
          if (!isStaleProbe()) this.scheduleReconnect();
        });
    } else {
      this.scheduleReconnect();
    }
  }

  /** Schedule a reconnect with capped backoff (no-op if auto-reconnect is off/stopped). */
  private scheduleReconnect(): void {
    if (this.stopped || !this.autoReconnect) return;
    if (this.reconnectTimer !== null) return; // one in flight
    const idx = Math.min(this.reconnectAttempts, RECONNECT_BACKOFF_MS.length - 1);
    const delay = RECONNECT_BACKOFF_MS[idx] ?? RECONNECT_BACKOFF_MS[RECONNECT_BACKOFF_MS.length - 1]!;
    this.reconnectAttempts += 1;
    this.reconnectTimer = this.setTimer(() => {
      this.reconnectTimer = null;
      if (this.stopped || this.ws) return;
      this.connect();
    }, delay);
  }

  /** Re-arm the half-open transport watchdog after open and after every received frame. */
  private armWatchdog(ws: WsLike, gen: number): void {
    this.clearWatchdog();
    if (this.watchdogMs <= 0) return;
    const nonce = ++this.watchdogNonce;
    this.watchdogTimer = this.setTimer(() => {
      this.watchdogTimer = null;
      if (
        nonce !== this.watchdogNonce
        || this.stopped
        || this.ws !== ws
        || this.generation !== gen
      ) return;
      // A browser can leave a TCP connection half-open indefinitely. Detach first so the close we
      // initiate cannot race the transient-drop path, then reconnect through the normal probe flow.
      this.detach(ws);
      this.ws = null;
      this.closedCleanly = true;
      try {
        ws.close(WS_STALE_CLOSE, 'dashboard no-frame timeout');
      } catch {
        // The transport is already unusable; the reconnect path below is still correct.
      }
      this.handleTransientDrop();
    }, this.watchdogMs);
  }

  private clearWatchdog(): void {
    this.watchdogNonce += 1;
    if (this.watchdogTimer !== null) {
      this.clearTimer(this.watchdogTimer);
      this.watchdogTimer = null;
    }
  }

  /** Detaches all handlers from a socket so it can never call back into the instance. */
  private detach(ws: WsLike): void {
    this.clearWatchdog();
    ws.onopen = null;
    ws.onmessage = null;
    ws.onclose = null;
    ws.onerror = null;
  }

  /** Current per-domain dedup cursors (for tests / display). */
  getCursors(): LastSeq {
    return { ...this.lastSeq };
  }

  // -- Time travel (D11) ----------------------------------------------------

  /**
   * Pause applying live frames; subsequent frames accumulate in the shadow buffer. This does NOT
   * flip `connection` to `'seeking'` (D11 finding 1): pausing the live feed and EXPOSING the seek
   * state are decoupled, so the store is never observed `seeking` while its rows/cursors are still
   * LIVE. The Scrubber flips to `'seeking'` only atomically, via `applySeekCut`, once the fetched
   * frozen cut lands. The shadow buffer keeps the live monitor cursor from advancing meanwhile, so
   * the cut's `monitor_seq` is the true boundary.
   */
  seek(): void {
    // First seek of a drag (not-paused → paused): capture the LIVE baseline BEFORE any
    // `applySeekCut` overwrites the store with the frozen cut, so `live()` can restore the
    // up-to-date live rows/cursors/monitor (finding 1). Re-entrant `seek()` calls during the same
    // drag must NOT recapture — by then the store may already hold the frozen cut.
    if (!this.paused) {
      this.liveBaseline = this.store.getState().captureLiveBaseline();
      this.clearShadowBuffer();
      this.resyncRequired = false;
      this.store.getState().setResyncRequired(false);
    }
    this.paused = true;
  }

  /**
   * Resume LIVE. The store currently holds the FROZEN seek cut (`applySeekCut`), so before replaying
   * the shadow buffer we MUST re-establish the live store, else buffered frames would replay onto the
   * frozen cut and the rows/cursors between the cut and the pause would stay missing/rewound while
   * `connection==='live'` (finding 1). Two re-baseline paths, each of which flips to `'live'`
   * ATOMICALLY with its store restore BEFORE any frame replays (D11 R3 — never replay live data
   * while `connection==='seeking'`):
   *  - a RECONNECT during the seek STAGED a fresh snapshot (finding 6): apply it FIRST — it is the
   *    authoritative current cut + cursors (and supersedes the now-stale captured baseline).
   *    `commitSnapshot` installs it AND flips to live ATOMICALLY (`restoreLiveSnapshot`, D11 R6),
   *    before draining early frames — so the staged-resume path never exposes `seeking` with live
   *    snapshot data, mirroring the baseline-restore path's atomic flip;
   *  - otherwise RESTORE the live baseline captured at `seek()` (the up-to-date pre-seek live state).
   *    `restoreLiveBaseline` flips `connection='live'` in the SAME atomic update as the restore.
   * Then replay buffered live frames in arrival order (dedup still applies) on top of the live store.
   * The trailing `setConnection('live')` is a no-op for both re-baseline paths (already live); it
   * only covers the degenerate case where neither a staged snapshot nor a baseline exists.
   */
  live(): void {
    this.paused = false;
    const staged = this.pendingSnapshot;
    this.pendingSnapshot = null;
    const baseline = this.liveBaseline;
    this.liveBaseline = null;
    if (this.resyncRequired && !staged) {
      // The historical cut stays intact until LIVE is explicitly selected. At that point the
      // incomplete shadow feed cannot be trusted, so discard it and force a new snapshot-bearing
      // connection instead of replaying a partial continuation.
      this.clearShadowBuffer();
      this.resyncRequired = false;
      this.store.getState().setResyncRequired(false);
      this.forceFreshSnapshot();
      return;
    }
    this.resyncRequired = false;
    this.store.getState().setResyncRequired(false);
    if (staged) {
      this.commitSnapshot(staged); // resets store + cursors, marks live, drains early frames
    } else if (baseline) {
      // Re-establish the up-to-date live store AND flip to live atomically, so buffered frames don't
      // replay onto the frozen cut nor under a stale `connection==='seeking'`.
      this.store.getState().restoreLiveBaseline(baseline);
    }
    const buffered = this.shadowBuffer;
    this.clearShadowBuffer();
    this.replayBuffered(buffered);
    // Degenerate fallback only (no staged snapshot, no baseline). The re-baseline paths above
    // already flipped to live atomically, so this is a no-op there (re-applying 'live' won't bump
    // the epoch — see setConnection).
    this.store.getState().setConnection('live');
  }

  isPaused(): boolean {
    return this.paused;
  }

  shadowBufferLength(): number {
    return this.shadowBuffer.length;
  }

  requiresResync(): boolean {
    return this.resyncRequired;
  }

  /** Append a seek frame unless one of the independent memory/time bounds is exhausted. */
  private bufferFrame(frame: DashboardFrame): void {
    if (this.resyncRequired) return;
    const now = this.now();
    const started = this.shadowBufferStartedAtMs ?? now;
    const bytes = encodedFrameBytes(frame);
    if (
      this.shadowBuffer.length >= this.shadowLimits.frames
      || this.shadowBufferBytes + bytes > this.shadowLimits.bytes
      || now - started > this.shadowLimits.ageMs
    ) {
      this.resyncRequired = true;
      this.store.getState().setResyncRequired(true);
      return;
    }
    if (this.shadowBufferStartedAtMs === null) this.shadowBufferStartedAtMs = now;
    this.shadowBuffer.push(frame);
    this.shadowBufferBytes += bytes;
  }

  private clearShadowBuffer(): void {
    this.shadowBuffer = [];
    this.shadowBufferBytes = 0;
    this.shadowBufferStartedAtMs = null;
  }

  /** Apply a bounded feed in small batches; small feeds complete in the first synchronous batch. */
  private replayBuffered(frames: DashboardFrame[]): void {
    if (this.replayFrame !== null) {
      this.cancelRaf(this.replayFrame);
      this.replayFrame = null;
    }
    let offset = 0;
    const drain = () => {
      this.replayFrame = null;
      const end = Math.min(frames.length, offset + REPLAY_BATCH_FRAMES);
      while (offset < end) this.applyFrame(frames[offset++]!);
      if (offset < frames.length && !this.stopped) this.replayFrame = this.raf(drain);
    };
    drain();
  }

  /** Tear down the current transport and reconnect immediately so the first frame is a snapshot. */
  private forceFreshSnapshot(): void {
    if (this.replayFrame !== null) {
      this.cancelRaf(this.replayFrame);
      this.replayFrame = null;
    }
    const ws = this.ws;
    if (ws) {
      this.detach(ws);
      this.ws = null;
      this.generation += 1;
      try {
        ws.close(WS_STALE_CLOSE, 'dashboard seek buffer overflow');
      } catch {
        // A closed transport still permits opening its replacement below.
      }
    }
    this.snapshotApplied = false;
    this.lastSeq = { flow: 0, metrics: 0, topology: 0, monitor: 0 };
    this.store.getState().setConnection('connecting');
    this.connect();
  }

  // -- Decode + dispatch ----------------------------------------------------

  /** Decodes a raw WS payload (string or already-parsed object) and routes it. */
  private handleRaw(data: unknown): void {
    let parsed: unknown;
    try {
      parsed = typeof data === 'string' ? JSON.parse(data) : data;
    } catch {
      // Malformed JSON: ignore (a bad frame must not crash the pipe or advance a cursor).
      return;
    }
    this.handleParsed(parsed);
  }

  /**
   * Public for tests: route an UNTRUSTED decoded value. Snapshots and frames are validated
   * before anything mutates. Anything that fails validation is dropped silently.
   */
  handleParsed(parsed: unknown): void {
    if (isSnapshotCandidate(parsed)) {
      try {
        assertDashboardSchemaVersion(parsed.schema_version, 'WebSocket snapshot');
      } catch (error) {
        if (error instanceof DashboardSchemaMismatchError) {
          this.disconnect();
          this.store.getState().setConnection('error');
          if (!error.reloadRequested) this.store.getState().setFatalError(error.message);
          return;
        }
        throw error;
      }
      if (!isSnapshotFrame(parsed)) {
        // A malformed ROOT snapshot cannot be treated like an ordinary dropped live frame: there
        // is no trustworthy baseline to connect to. Surface the contract failure explicitly.
        this.disconnect();
        this.store.getState().setConnection('error');
        this.store.getState().setFatalError('dashboard contract validation failed: WebSocket snapshot');
        return;
      }
    }
    if (isSnapshotFrame(parsed)) {
      this.applySnapshotMessage(parsed);
      return;
    }
    if (!isDashboardFrame(parsed)) {
      // Not a valid frame → drop. No cursor moves, no store mutation (finding 6).
      return;
    }
    const frame: DashboardFrame = parsed;
    // A live frame before the snapshot is buffered until the snapshot lands.
    if (!this.snapshotApplied) {
      this.bufferFrame(frame);
      return;
    }
    if (this.paused) {
      this.bufferFrame(frame);
      return;
    }
    this.applyFrame(frame);
  }

  private applySnapshotMessage(snap: SnapshotFrame): void {
    // Finding 6: a reconnect snapshot arriving WHILE SEEKING must NOT overwrite the frozen
    // cut or flip to live. Stage it; `live()` applies it on explicit resume.
    if (this.paused) {
      this.pendingSnapshot = snap;
      return;
    }
    this.commitSnapshot(snap);
  }

  /**
   * Installs a snapshot AS the live store + cursors and drains any early-arrived frames. The
   * store install and the flip to `connection='live'` happen in ONE atomic update
   * (`restoreLiveSnapshot`) — never `applySnapshot` then a separate `setConnection('live')` (D11
   * R6). That ordering matters on the STAGED-reconnect resume path (`live()`), where the store
   * still holds the FROZEN cut under `connection==='seeking'`: a non-atomic install would expose
   * `seeking` WITH the snapshot's live rows/cursors/metrics before the flip — the window D10 must
   * never observe. The combined action also covers the initial/reconnect-while-live path (already
   * non-`seeking`), where the atomic flip is simply a strict improvement. The shadow-buffer replay
   * below therefore always runs on a store that is already `'live'`.
   */
  private commitSnapshot(snap: SnapshotFrame): void {
    this.store.getState().restoreLiveSnapshot({
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
    this.resyncRequired = false;
    this.store.getState().setResyncRequired(false);

    // Drain any pre-snapshot frames that arrived early.
    const early = this.shadowBuffer;
    this.clearShadowBuffer();
    for (const frame of early) {
      if (!this.paused) this.applyFrame(frame);
      else this.bufferFrame(frame);
    }
  }

  /**
   * Applies one batched frame. Order of operations (finding 6):
   *   1. VALIDATE the whole frame (envelope + every payload). Invalid → drop, NO mutation.
   *   2. Dedup: `seq <= cursor` → drop the whole batch, cursor unchanged.
   *   3. Advance the cursor, apply every payload, then notify `onFrameApplied`.
   * Returns true if the frame was applied, false if dropped (stale or invalid).
   */
  applyFrame(frame: unknown): boolean {
    // (1) Validate BEFORE touching any cursor/store. A partially-valid frame is rejected
    // wholesale so it never half-applies and stays replayable on a later valid resend.
    if (!isDashboardFrame(frame)) {
      return false;
    }
    const valid: DashboardFrame = frame;
    const cursorKey = domainToCursorKey(valid.domain);
    // REST `/flows` reconciliation may advance the authoritative flow cursor when it observes a
    // TTL/count/quota removal that has no row-shaped WS event. Include the store cursor in dedup so
    // a delayed pre-removal frame cannot resurrect that row after reconciliation.
    const cursor = Math.max(this.lastSeq[valid.domain], this.store.getState().cursors[cursorKey]);
    this.lastSeq[valid.domain] = cursor;
    // (2) Whole-frame dedup: a stale or duplicate seq drops the ENTIRE batch.
    if (valid.seq <= cursor) {
      return false;
    }
    // (3) Accept: advance cursor, apply every payload (no sibling dropped), then notify.
    this.lastSeq[valid.domain] = valid.seq;
    const store = this.store.getState();
    store.setCursor(cursorKey, valid.seq);
    if (valid.domain === 'monitor') {
      // Validation already proved domain↔payload compatibility. Apply the entire originating
      // DebugUpdate in ONE store mutation so a replay batch does not clone both monitor rings once
      // per sibling; every message receives the frame's one monitor-domain seq stamp.
      store.pushMonitorBatch(
        valid.batch.flatMap((payload) => payload.type === 'monitor' ? [payload.message] : []),
        valid.seq,
      );
    } else {
      for (const payload of valid.batch) this.applyPayload(payload);
    }
    this.onFrameApplied?.(valid);
    return true;
  }

  /** Exhaustive dispatch over the `DashboardPayload` union (no `any`, no fallthrough). */
  private applyPayload(payload: DashboardPayload): void {
    const store = this.store.getState();
    switch (payload.type) {
      case 'monitor':
        // The monitor arm NESTS an itself-tagged DebugWsMessage under `message`
        // (see types WIRE CONTRACT — it is NOT flattened). Stamp it with the frame's
        // already-advanced monitor seq so the inspector can bound the join to a seek cut.
        store.pushMonitor(payload.message, this.lastSeq.monitor);
        return;
      case 'usage':
        store.patchUsage(payload.api_call_id, {
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
          generated_at_ms: payload.generated_at_ms,
          instant: payload.instant,
        });
        return;
      case 'flow_status':
        // Keyed by api_call_id (the store keys flows by api_call_id).
        store.patchFlowStatus(payload);
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

function isSnapshotCandidate(value: unknown): value is { type: 'snapshot'; schema_version?: unknown } {
  return typeof value === 'object' && value !== null && (value as { type?: unknown }).type === 'snapshot';
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/** Conservative UTF-8 size of one decoded frame for the seek shadow-buffer quota. */
function encodedFrameBytes(frame: DashboardFrame): number {
  try {
    const json = JSON.stringify(frame);
    if (typeof TextEncoder === 'function') return new TextEncoder().encode(json).byteLength;
    // UTF-16 code units are an upper bound for ASCII and a useful fallback on very old engines.
    return json.length * 2;
  } catch {
    // A validated frame is JSON-shaped, but fail closed if a host object ever slips through.
    return SHADOW_MAX_BYTES + 1;
  }
}
