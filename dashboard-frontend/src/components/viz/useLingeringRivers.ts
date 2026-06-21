/**
 * `useLingeringRivers` (D12, finding 4) — wraps `buildRivers` with the spec's "tiles linger-then-
 * fade" lifecycle. Without it a completed/failed river stays in the grid until the monitor EVICTS it
 * (a `request_remove`, up to ~30 min later — D3 retention), so finished streams pile up looking
 * active. This hook instead, when a river goes terminal (completed/failed): keeps it for a SHORT
 * linger, then flips it to an `exiting` phase (the CSS exit fade), then REMOVES it from the rendered
 * set — independent of when the monitor finally evicts it.
 *
 * StrictMode-safe timers: every scheduled timeout id is tracked in a ref and CLEARED on unmount (and
 * any already-fired bookkeeping is reconciled), so React 18's mount→unmount→remount never leaks a
 * timer or double-schedules. A river that the monitor removes (or that somehow returns to running)
 * has its pending timers cancelled and its bookkeeping dropped.
 *
 * Returned rivers carry an `exiting` flag the tile uses to apply the fade class; removed rivers are
 * filtered out entirely.
 */
import { useEffect, useRef, useState } from 'react';
import { buildRivers, type River } from './riverModel';
import type { DebugWsMessage } from '../../api/types';

/** A river plus its exit-phase flag (true once the linger elapsed and the fade is running). */
export interface LingeringRiver extends River {
  exiting: boolean;
}

/** How long a terminated tile stays fully visible before the fade begins. */
export const LINGER_MS = 4_000;
/** The exit fade duration — MUST match `.river-tile-exiting` in index.css. */
export const FADE_MS = 400;

/** Per-river teardown bookkeeping: the two pending timers + whether we've started its exit. */
interface Pending {
  lingerTimer: number | undefined;
  removeTimer: number | undefined;
}

export function useLingeringRivers(
  monitor: DebugWsMessage[],
  opts: { lingerMs?: number; fadeMs?: number } = {},
): LingeringRiver[] {
  const lingerMs = opts.lingerMs ?? LINGER_MS;
  const fadeMs = opts.fadeMs ?? FADE_MS;

  const rivers = buildRivers(monitor);
  // The per-river lifecycle signature (ids + statuses) — the linger effect keys on THIS, not the
  // rivers array identity, so it re-runs when a river flips terminal / appears / is evicted, NOT on
  // every text delta. `rivers` is read inside the effect via a ref so it stays out of the dep array.
  const signature = riversTerminalSignature(rivers);
  const riversRef = useRef(rivers);
  riversRef.current = rivers;
  // Ids currently in the fade phase (linger elapsed) and ids fully removed (fade elapsed).
  const [exiting, setExiting] = useState<ReadonlySet<string>>(() => new Set());
  const [removed, setRemoved] = useState<ReadonlySet<string>>(() => new Set());
  // The pending timers per river id — a ref so scheduling doesn't churn renders, and so the unmount
  // cleanup can clear every outstanding timer (StrictMode-safe).
  const pendingRef = useRef<Map<string, Pending>>(new Map());

  // Schedule/cancel linger timers as river statuses change. Runs after each render so it sees the
  // freshly-built rivers (via the ref); the ref-tracked timers make re-scheduling idempotent (we
  // arm a river once). Keyed on `signature` so it re-evaluates on a lifecycle change only.
  useEffect(() => {
    const pending = pendingRef.current;
    const current = riversRef.current;
    const liveIds = new Set(current.map((r) => r.id));
    const terminalIds = new Set(current.filter((r) => r.status !== 'running').map((r) => r.id));

    // Arm a linger→fade→remove chain for each newly-terminal river not already scheduled.
    for (const id of terminalIds) {
      if (pending.has(id)) continue;
      const entry: Pending = { lingerTimer: undefined, removeTimer: undefined };
      entry.lingerTimer = window.setTimeout(() => {
        setExiting((s) => new Set(s).add(id)); // begin the fade
        entry.removeTimer = window.setTimeout(() => {
          setRemoved((s) => new Set(s).add(id)); // drop after the fade
        }, fadeMs);
      }, lingerMs);
      pending.set(id, entry);
    }

    // Cancel + forget any scheduled river that is no longer present OR returned to running (so a
    // re-used response_id doesn't get stuck mid-fade), and clear its phase flags.
    for (const [id, entry] of pending) {
      if (liveIds.has(id) && terminalIds.has(id)) continue;
      if (entry.lingerTimer != null) window.clearTimeout(entry.lingerTimer);
      if (entry.removeTimer != null) window.clearTimeout(entry.removeTimer);
      pending.delete(id);
      // Reconcile phase state if this river had already begun/finished exiting.
      setExiting((s) => (s.has(id) ? new Set([...s].filter((x) => x !== id)) : s));
      setRemoved((s) => (s.has(id) ? new Set([...s].filter((x) => x !== id)) : s));
    }
    // Re-run when the set of terminal rivers changes (ids + statuses), not on every text delta. The
    // current rivers are read from `riversRef` inside, so the array is intentionally not a dep.
  }, [signature, lingerMs, fadeMs]);

  // Clear ALL outstanding timers on unmount — StrictMode-safe, no leaked timer survives teardown.
  useEffect(() => {
    const pending = pendingRef.current;
    return () => {
      for (const entry of pending.values()) {
        if (entry.lingerTimer != null) window.clearTimeout(entry.lingerTimer);
        if (entry.removeTimer != null) window.clearTimeout(entry.removeTimer);
      }
      pending.clear();
    };
  }, []);

  return rivers
    .filter((r) => !removed.has(r.id))
    .map((r) => ({ ...r, exiting: exiting.has(r.id) }));
}

/** A stable signature of the rivers' lifecycle state (ids + statuses) — the linger effect's dep. */
function riversTerminalSignature(rivers: River[]): string {
  return rivers.map((r) => `${r.id}:${r.status}`).join('|');
}
