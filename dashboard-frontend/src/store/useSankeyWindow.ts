/**
 * The token-Sankey rolling-window accumulator (D12, finding 2), split into an APP-LIFETIME fold
 * engine + a thin read hook (D12 R3).
 *
 * THE FOLD IS APP-LIFETIME, NOT MOUNT-SCOPED (D12 R3 — the fix for "usage that grew while away from
 * the Sankey is folded at REMOUNT time and stamped at the remount instant"): folding lives in a
 * MODULE-level store subscription started ONCE at app bootstrap (`startSankeyFold`, called from the
 * composition root), so it runs from app start regardless of which view is mounted. Each usage
 * increment is stamped with the wall-clock instant it ACTUALLY arrives, never `Date.now()` at a
 * remount. The `SankeyView` only READS the already-maintained windowed ring via `useSankeyWindow`;
 * mounting/unmounting it no longer changes WHEN deltas are recorded. Growth that happens while the
 * Sankey is unmounted is folded at its real arrival time and correctly ages out of the 30 s window.
 *
 * Why deltas (not cumulative totals): a `FlowSummary.usage` is the flow's LIFETIME cumulative count.
 * The band must be tokens/30 s, so we cannot sum cumulative totals of every flow that overlaps the
 * window — a single long-running flow would report its entire lifetime as the 30 s rate. Instead we
 * record the INCREMENT each flow grew by, stamped with the wall-clock instant we observed it, and the
 * model sums only the increments inside `[now - windowMs, now]`. A flow that streamed a million
 * tokens an hour ago but is idle now contributes nothing to the current band.
 *
 * Why a direct store subscription (not a selector + an effect): React batches synchronous store
 * updates into ONE commit, so a `useSyncExternalStore` selector only exposes the LATEST `flows` —
 * intermediate usage bumps landing in the same tick would be lost. The module fold runs on every
 * vanilla-store change, so no increment is dropped.
 *
 * STABLE SINGLETON COLLECTOR (finding 3 — the fix for "route entry / seek resume restamps every
 * cumulative total as fresh traffic"): the baselines + delta ring live at MODULE scope, so a flow
 * already at cumulative total T keeps baseline T; its next observation after a remount/seek-resume
 * diffs to 0 and emits NOTHING — its lifetime total is never re-stamped as a fresh 30 s band. Only
 * TRUE incremental growth folds, with its real arrival time. The baselines + ring are cleared ONLY
 * on a genuine TEARDOWN edge — the connection entering `'idle'`/`'closed'`, where cumulative
 * continuity is broken and a reused `api_call_id` may restart at a lower total. Because the fold
 * engine is app-lifetime, that clear fires even when no `SankeyView` is mounted at the instant of
 * teardown.
 *
 * SNAPSHOT-SEED, NOT LIVE-GROWTH (D12 R4 — the fix for "the INITIAL snapshot stamps every retained
 * flow's LIFETIME cumulative as a fresh delta at arrival time, so a 30-min-old completed flow's
 * lifetime tokens inflate the current 30 s window"): a `FlowSummary.usage` in a snapshot is the
 * flow's LIFETIME total, NOT growth that just happened. Folding the first observation of every
 * snapshot flow as a `now()`-stamped delta (diff against an empty baseline) would dump hours of
 * historical tokens into the live rolling window. So whenever the store crosses a SNAPSHOT/RESET
 * boundary — signalled by the monotonic `connEpoch` advancing (`applySnapshot` = the initial &
 * reconnect snapshots, `restoreLiveSnapshot`/`restoreLiveBaseline` = seek resume, `reset()` =
 * teardown) — the NEXT fold runs in SEED mode: it SILENTLY stamps each flow's baseline to its
 * CURRENT cumulative usage and emits NOTHING. Only usage GROWTH observed on LATER frames (same
 * epoch, no boundary crossed) folds as timestamped deltas. This subsumes finding 3's no-restamp
 * guarantee for the seek-resume path (the resumed flows seed silently instead of diffing to 0) and
 * fixes the initial-snapshot inflation: a completed flow that streamed a million tokens before the
 * snapshot seeds at that total and contributes to the window only if it grows AFTER the snapshot.
 */
import { useEffect, useRef, useState } from 'react';
import { dashboardStore } from './dashboardStore';
import type { FlowSummary } from '../api/types';
import type { SankeyUsageDelta } from '../components/viz/sankeyModel';

export interface SankeyWindow {
  /** Bumps once per fold-engine ring change so the component re-renders to read the (ref-held) ring. */
  version: number;
  /** The current windowed deltas (ref-held — read after a `version` bump). */
  deltasRef: React.MutableRefObject<SankeyUsageDelta[]>;
}

/** The last cumulative usage we saw per flow, to diff the next snapshot into a delta. */
interface Cumulative {
  prompt: number;
  cached: number;
  completion: number;
  total: number;
}

export const DEFAULT_WINDOW_MS = 30_000;
/** Hard cap on retained deltas (defensive — pruning by time is the primary bound). */
const MAX_DELTAS = 5_000;

/** Connection states that signal a genuine session teardown (cumulative continuity broken). */
function isTeardown(connection: string): boolean {
  return connection === 'idle' || connection === 'closed';
}

/**
 * The APP-LIFETIME fold engine: owns the per-flow baselines + the windowed delta ring, folds usage
 * growth as store frames arrive (stamping each delta at its REAL arrival time), and notifies readers
 * when the ring changes. One instance per app, installed once by `startSankeyFold`.
 */
interface FoldEngine {
  baselines: Map<string, Cumulative>;
  /** The windowed ring, appended in arrival-time order so pruning is incremental from the front. */
  deltas: SankeyUsageDelta[];
  /** Last `flows` Map reference folded — an UNCHANGED reference means no flow grew (skip the scan). */
  lastFlows: Map<string, FlowSummary> | null;
  /** Last connection state, to detect the EDGE into a teardown state (clears baselines + ring). */
  lastConnection: string;
  /**
   * Last `connEpoch` folded, to detect a SNAPSHOT/RESET boundary (D12 R4). When it advances the next
   * fold SEEDS baselines silently (a snapshot's flows carry LIFETIME totals, not just-arrived growth)
   * so historical lifetime tokens never fold into the live rolling window as a fresh delta.
   */
  lastEpoch: number;
  retentionMs: number;
  now: () => number;
  /** Reader callbacks (the hook) fired after every ring change. */
  listeners: Set<() => void>;
  /** Tears down the store subscription (test-only; the production engine is never torn down). */
  unsubscribe: (() => void) | null;
}

let engine: FoldEngine | null = null;

/** Notify readers (the hook) that the ring changed so they re-render to read it. */
function emit(eng: FoldEngine): void {
  for (const l of eng.listeners) l();
}

/**
 * Drop ring entries older than the retention window. The ring is in arrival-time ORDER, so every
 * expired entry is a contiguous PREFIX — drop them from the front (O(dropped), not O(n)), then defend
 * with the hard cap. Returns whether anything was dropped (callers decide whether to notify). Used by
 * both the fold path AND the read path, so a ring read between store frames (e.g. a SankeyView
 * remount after a long idle) is still time-accurate without waiting for the next fold.
 */
function pruneRing(eng: FoldEngine, nowMs: number): boolean {
  const cutoff = nowMs - eng.retentionMs;
  let dropped = 0;
  while (dropped < eng.deltas.length && eng.deltas[dropped]!.ts < cutoff) dropped++;
  if (dropped > 0) eng.deltas = eng.deltas.slice(dropped);
  if (eng.deltas.length > MAX_DELTAS) {
    eng.deltas = eng.deltas.slice(eng.deltas.length - MAX_DELTAS);
    return true;
  }
  return dropped > 0;
}

/**
 * Fold the store's CURRENT `flows` into timestamped deltas. Runs on every store change (and once
 * eagerly at install — on the empty pre-connect store). Cheap when nothing relevant changed:
 *  - SKIPS the per-flow scan entirely when the `flows` Map reference is UNCHANGED (MED fix: avoid an
 *    O(flows + deltas) cost on every unrelated monitor/metrics frame — those never replace `flows`,
 *    and `upsertFlow`/`patchUsage`/`patchFlowStatus` each install a NEW Map, so an identical
 *    reference proves no flow grew). Time-pruning when the clock advances WITHOUT a flow change is
 *    handled lazily by `pruneRing` on the read path (+ the view's per-second recompute tick). A SEED
 *    frame overrides this skip — the baselines must be re-stamped to the snapshot's totals.
 *  - SKIPS folding (and seeding) while `seeking` (the store holds the FROZEN cut, not live increments
 *    — D11 R5). The epoch is not advanced by entering seek, so the resume frame still seeds.
 *  - SEEDS baselines silently (emitting NO deltas) whenever the monotonic `connEpoch` advances — a
 *    SNAPSHOT/RESET boundary (`applySnapshot`/`restoreLiveSnapshot`/`restoreLiveBaseline`/`reset`). A
 *    snapshot's `usage` is LIFETIME cumulative, not just-arrived growth, so seeding prevents
 *    historical totals from folding into the live window as a fresh band (D12 R4). Only same-epoch
 *    growth on later frames folds as timestamped deltas.
 *  - CLEARS baselines + ring on the EDGE into a teardown state (`idle`/`closed`) — a genuine session
 *    teardown where cumulative continuity is broken and a reused `api_call_id` may restart at a lower
 *    total. A `live → seeking → live` round-trip never enters those states, so it does NOT clear (the
 *    resumed flows seed silently on the epoch-advancing resume frame — no restamp). Driven here
 *    (app-lifetime subscription) so it fires even with no SankeyView mounted (a `reset()` may land
 *    between route mounts).
 */
function fold(eng: FoldEngine): void {
  const s = dashboardStore.getState();

  const teardownEdge = isTeardown(s.connection) && !isTeardown(eng.lastConnection);
  eng.lastConnection = s.connection;
  if (teardownEdge) {
    const had = eng.deltas.length > 0 || eng.baselines.size > 0;
    eng.baselines = new Map();
    eng.deltas = [];
    // Re-baseline against the post-teardown flows on the next change (force the scan past the MED
    // reference check). The flows after a `reset()` are empty, but a fresh snapshot may land them in
    // the SAME teardown state (`idle`) before the flip to `live` — folding resumes on the next frame.
    eng.lastFlows = null;
    if (had) emit(eng);
  }

  // While SEEKING the store holds the FROZEN cut (not live increments) — never fold OR seed it. The
  // epoch is NOT advanced here, so the resume frame (which DOES advance the epoch) still seeds. (D11
  // R5 — the frozen cut never enters the live ring or baselines.)
  if (s.connection === 'seeking') return;

  // D12 R4: a SNAPSHOT/RESET boundary (monotonic `connEpoch` advanced) means the upcoming `flows`
  // carry LIFETIME cumulative usage, not just-arrived growth. Run this fold in SEED mode: silently
  // stamp baselines and emit NOTHING, so historical totals never fold into the live rolling window as
  // a fresh `now()`-stamped delta. Subsequent same-epoch frames fold true growth normally.
  const seed = s.connEpoch !== eng.lastEpoch;
  eng.lastEpoch = s.connEpoch;
  // Force the per-flow scan when seeding even if the `flows` reference happens to be unchanged (the
  // baselines MUST be re-stamped to the snapshot's totals); otherwise apply the MED reference-skip.
  if (!seed && s.flows === eng.lastFlows) return;
  eng.lastFlows = s.flows;

  const ts = eng.now();
  const baselines = eng.baselines;
  let folded = false;
  for (const f of s.flows.values()) {
    const u = f.usage;
    if (!u) continue;
    const model = f.model_served ?? f.model_requested;
    if (!model) continue;
    // On a SEED frame: silently re-baseline to the snapshot's cumulative total and emit no delta.
    // Otherwise diff against the prior cumulative snapshot; a brand-new flow's first LIVE observation
    // IS its delta (prev = 0s). A non-increasing total (no growth) records nothing — so a historical
    // cumulative total already captured in the baseline (after a remount/seek-resume) emits NOTHING,
    // never a fresh `now()`-stamped band.
    if (!seed) {
      const prev = baselines.get(f.api_call_id);
      const dTotal = u.total - (prev?.total ?? 0);
      if (dTotal > 0) {
        eng.deltas.push({
          ts,
          upstream: f.upstream_target ?? null,
          model,
          prompt: Math.max(0, u.prompt - (prev?.prompt ?? 0)),
          cached: Math.max(0, u.cached - (prev?.cached ?? 0)),
          completion: Math.max(0, u.completion - (prev?.completion ?? 0)),
          total: dTotal,
        });
        folded = true;
      }
    }
    // Always advance the baseline to the latest cumulative (even on a non-positive diff, e.g. a
    // corrected/reset total, or a SEED frame) so the NEXT diff is against the current truth.
    baselines.set(f.api_call_id, { prompt: u.prompt, cached: u.cached, completion: u.completion, total: u.total });
  }

  const pruned = pruneRing(eng, ts);
  if (folded || pruned) emit(eng);
}

/**
 * Install the app-lifetime fold engine (idempotent). Call ONCE from the composition root at bootstrap
 * so folding runs from app start regardless of which view is mounted. Subsequent calls are no-ops
 * (the first install wins) so a defensive call from the hook never replaces the running engine or
 * its accumulated state.
 *
 * @param windowMs the rolling-window retention; entries older than `now - windowMs` are pruned.
 * @param now injectable clock (tests); defaults to `Date.now`.
 */
export function startSankeyFold(windowMs = DEFAULT_WINDOW_MS, now: () => number = Date.now): void {
  if (engine) return;
  const eng: FoldEngine = {
    baselines: new Map(),
    deltas: [],
    lastFlows: null,
    lastConnection: dashboardStore.getState().connection as string,
    lastEpoch: dashboardStore.getState().connEpoch,
    retentionMs: windowMs,
    now,
    listeners: new Set(),
    unsubscribe: null,
  };
  engine = eng;
  // Fold the current state immediately, then on EVERY store change (no increment lost to batching).
  // The composition root installs this on an EMPTY store before the socket connects, so the eager
  // fold sees no flows; the initial snapshot lands LATER via `applySnapshot` (epoch advances → that
  // frame SEEDS, never folding lifetime totals into the window — D12 R4).
  fold(eng);
  eng.unsubscribe = dashboardStore.subscribe(() => fold(eng));
}

const EMPTY: SankeyUsageDelta[] = [];

/**
 * Read the live windowed ring, time-pruned to NOW first. Pruning on read (not only on fold) keeps the
 * ring accurate when it is read BETWEEN store frames — e.g. a SankeyView remount after a long idle,
 * where deltas aged past the window since the last fold but no frame has arrived to prune them. The
 * engine's own clock bounds the window so the read agrees with the fold path.
 */
function readSankeyDeltas(): SankeyUsageDelta[] {
  if (!engine) return EMPTY;
  pruneRing(engine, engine.now());
  return engine.deltas;
}

/**
 * Tear down + forget the fold engine (TEST ONLY). Production never calls this — the engine is an
 * app-lifetime global. Tests use it to restart the engine with an injected clock between cases.
 */
export function __resetSankeyFold(): void {
  if (engine?.unsubscribe) engine.unsubscribe();
  engine = null;
}

/**
 * READ the app-lifetime windowed delta ring for the Sankey. This hook NO LONGER folds — folding is
 * the app-lifetime engine's job (`startSankeyFold`, installed at bootstrap). The hook ensures the
 * engine is running (idempotent safety net), subscribes for ring-change notifications, and returns
 * the ref-held ring + a `version` that bumps on each change so the consumer re-renders to read it.
 *
 * @param windowMs the rolling window; forwarded to the engine's retention on first install only.
 * @param now injectable clock (tests); forwarded to the engine on first install only.
 */
export function useSankeyWindow(windowMs = DEFAULT_WINDOW_MS, now: () => number = Date.now): SankeyWindow {
  // Defensive: in production the composition root has already started the engine; this no-ops then.
  // It only installs if some entry path mounted the view without bootstrapping (keeps the view robust).
  startSankeyFold(windowMs, now);

  const deltasRef = useRef<SankeyUsageDelta[]>(readSankeyDeltas());
  const [version, setVersion] = useState(0);

  useEffect(() => {
    // Re-read (time-pruned) + re-render once on mount — the engine may have folded (or deltas may
    // have aged out) while this view was unmounted — then on every subsequent ring change.
    const sync = () => {
      deltasRef.current = readSankeyDeltas();
      setVersion((n) => n + 1);
    };
    sync();
    if (!engine) return;
    const eng = engine;
    eng.listeners.add(sync);
    return () => {
      eng.listeners.delete(sync);
    };
  }, []);

  return { version, deltasRef };
}
