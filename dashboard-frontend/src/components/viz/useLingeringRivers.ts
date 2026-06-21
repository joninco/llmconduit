/**
 * `useLingeringRivers` (D12, finding 4) — wraps `buildRivers` with the spec's "tiles linger-then-
 * fade" lifecycle. Without it a completed/failed river stays in the grid until the monitor EVICTS it
 * (a `request_remove`, up to ~30 min later — D3 retention), so finished streams pile up looking
 * active. This hook instead, when a river goes terminal (completed/failed): keeps it for a SHORT
 * linger, then flips it to an `exiting` phase (the CSS exit fade), then REMOVES it from the rendered
 * set — independent of when the monitor finally evicts it.
 *
 * ABSOLUTE lifecycle (finding 4 — the fix for "old streams reappear on remount"): the phase is
 * derived from each river's `terminalAtMs` (the monitor's `completed_at_ms`) against the CURRENT
 * clock, NOT from a fresh `setTimeout(lingerMs)` armed at mount. So a river that finished long before
 * this component mounted (after a navigation/seek that remounts the theater) is computed as already
 * past its fade and OMITTED immediately — it never restarts a 4 s linger and re-appears. A timer is
 * scheduled only for the REMAINING time of an in-progress linger/fade, so a still-fresh terminal
 * river finishes its fade exactly once, on the same wall-clock schedule, across remounts.
 *
 * StrictMode-safe timers: every scheduled timeout id is tracked in a ref and CLEARED on unmount (and
 * re-derived from `terminalAtMs` on remount), so React 18's mount→unmount→remount never leaks a
 * timer, double-schedules, nor restarts a lifecycle. A river that the monitor removes (or that
 * returns to running) has its pending timer cancelled.
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

/** The three lifecycle phases of a terminal river, derived from `terminalAtMs` vs the clock. */
type Phase = 'visible' | 'exiting' | 'removed';

export function useLingeringRivers(
  monitor: DebugWsMessage[],
  opts: { lingerMs?: number; fadeMs?: number; now?: () => number } = {},
): LingeringRiver[] {
  const lingerMs = opts.lingerMs ?? LINGER_MS;
  const fadeMs = opts.fadeMs ?? FADE_MS;
  const now = opts.now ?? Date.now;

  const rivers = buildRivers(monitor);
  // The per-river lifecycle signature (ids + their terminal instants) — the effect keys on THIS, not
  // the rivers array identity, so it re-runs when a river flips terminal / appears / is evicted, NOT
  // on every text delta. `rivers` is read inside the effect via a ref so it stays out of the dep array.
  const signature = riversTerminalSignature(rivers);
  const riversRef = useRef(rivers);
  riversRef.current = rivers;
  // A render nonce bumped by the timers so the body re-runs `phaseOf` (which reads the live clock) at
  // each linger/fade boundary even when no new monitor frame arrived.
  const [, setTick] = useState(0);
  // The single pending boundary timer (the NEXT phase transition across all rivers) — a ref so
  // scheduling doesn't churn renders and the unmount cleanup can clear it (StrictMode-safe).
  const timerRef = useRef<number | undefined>(undefined);

  /** The phase of a terminal river from its absolute finish instant; running rivers are 'visible'. */
  const phaseOf = (r: River, t: number): Phase => {
    if (r.status === 'running' || r.terminalAtMs == null) return 'visible';
    const age = t - r.terminalAtMs;
    if (age < lingerMs) return 'visible';
    if (age < lingerMs + fadeMs) return 'exiting';
    return 'removed';
  };

  // Schedule a SINGLE timer for the soonest upcoming phase boundary (a linger→exiting or
  // exiting→removed transition), computed from `terminalAtMs` against the clock. Re-derived from
  // scratch on every lifecycle change / remount, so nothing is double-armed and an already-expired
  // river never starts a fresh linger. Keyed on `signature` (+ the timing opts) so it re-evaluates
  // on a terminal-set change only.
  useEffect(() => {
    const schedule = (): void => {
      const t = now();
      const current = riversRef.current;
      // The nearest future boundary: for each non-removed terminal river, the time until it next
      // changes phase (linger end, then fade end). The min drives one timer.
      let nextDelay = Infinity;
      for (const r of current) {
        if (r.status === 'running' || r.terminalAtMs == null) continue;
        const age = t - r.terminalAtMs;
        const lingerEnds = r.terminalAtMs + lingerMs - t;
        const fadeEnds = r.terminalAtMs + lingerMs + fadeMs - t;
        if (age < lingerMs) nextDelay = Math.min(nextDelay, lingerEnds);
        else if (age < lingerMs + fadeMs) nextDelay = Math.min(nextDelay, fadeEnds);
      }
      if (timerRef.current != null) window.clearTimeout(timerRef.current);
      if (nextDelay !== Infinity) {
        timerRef.current = window.setTimeout(() => {
          setTick((n) => n + 1); // re-derive phases at the boundary
          schedule(); // arm the following boundary (if any)
        }, Math.max(0, nextDelay));
      }
    };
    schedule();
    return () => {
      if (timerRef.current != null) window.clearTimeout(timerRef.current);
      timerRef.current = undefined;
    };
    // Re-run when the terminal set changes (ids + finish instants), not on every text delta. The
    // current rivers are read from `riversRef` inside, so the array is intentionally not a dep.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature, lingerMs, fadeMs]);

  const t = now();
  return rivers
    .map((r) => ({ r, phase: phaseOf(r, t) }))
    .filter(({ phase }) => phase !== 'removed')
    .map(({ r, phase }) => ({ ...r, exiting: phase === 'exiting' }));
}

/**
 * A stable signature of the rivers' lifecycle state (ids + statuses + terminal instants) — the
 * linger effect's dep. Including `terminalAtMs` re-arms the timer if a river's finish instant is
 * (re)stamped, but NOT on every text delta (a running river's `terminalAtMs` is null).
 */
function riversTerminalSignature(rivers: River[]): string {
  return rivers.map((r) => `${r.id}:${r.status}:${r.terminalAtMs ?? ''}`).join('|');
}
