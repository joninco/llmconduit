/**
 * The reqs/s ring buffer behind the scrubber's "hill" — `(t_ms, reqs_per_sec)` samples at ~1s
 * granularity over a ~30 min window. Each live `metric_tick` (or the seed `/metrics`) contributes
 * one point, stamped with arrival wall-clock (the wire carries no per-tick timestamp).
 *
 * Pure + DOM-free: an immutable append-with-eviction reducer, trivially testable and safe in a
 * ref. The scrubber maps a pixel x → a time in `[t0, tEnd]` and looks up the nearest sample for
 * the hover tooltip.
 */

/** One ring sample: a wall-clock instant and the instantaneous reqs/s at it. */
export interface ReqsSample {
  t: number;
  reqs: number;
}

/** Window span retained (~30 min). */
export const REQS_WINDOW_MS = 30 * 60 * 1000;
/** Hard cap on retained samples (defensive — at 1s granularity 30min ≈ 1800). */
export const REQS_MAX_SAMPLES = 2000;

/**
 * Append `(t, reqs)` to `ring`, dropping samples older than `t - REQS_WINDOW_MS` and clamping to
 * `REQS_MAX_SAMPLES`. Returns a NEW array (immutable). Out-of-order/duplicate-`t` samples are
 * appended as-is; the producer (arrival order) is monotonic in practice.
 */
export function appendReqs(ring: ReqsSample[], t: number, reqs: number, now = t): ReqsSample[] {
  const cutoff = now - REQS_WINDOW_MS;
  const kept = ring.filter((s) => s.t >= cutoff);
  kept.push({ t, reqs });
  // Defensive cap (older-first eviction) in case granularity is finer than 1s.
  return kept.length > REQS_MAX_SAMPLES ? kept.slice(kept.length - REQS_MAX_SAMPLES) : kept;
}

/** The time bounds `[t0, tEnd]` spanned by the ring (null when empty). */
export function reqsBounds(ring: ReqsSample[]): { t0: number; tEnd: number } | null {
  if (ring.length === 0) return null;
  return { t0: ring[0]!.t, tEnd: ring[ring.length - 1]!.t };
}

/** Max reqs in the ring (for the hill's y-scale); 1 floor so a flat-zero ring still scales. */
export function reqsPeak(ring: ReqsSample[]): number {
  let peak = 0;
  for (const s of ring) if (s.reqs > peak) peak = s.reqs;
  return peak > 0 ? peak : 1;
}

/** Nearest sample to time `t` (binary-ish linear scan; rings are small). Null when empty. */
export function sampleAt(ring: ReqsSample[], t: number): ReqsSample | null {
  if (ring.length === 0) return null;
  let best = ring[0]!;
  let bestD = Math.abs(best.t - t);
  for (let i = 1; i < ring.length; i++) {
    const d = Math.abs(ring[i]!.t - t);
    if (d < bestD) {
      best = ring[i]!;
      bestD = d;
    }
  }
  return best;
}

/**
 * Map a normalized x in `[0,1]` across the ring's time span to a wall-clock instant. Returns null
 * when the ring is empty or has no span (single sample) — the caller falls back to `Date.now()`.
 */
export function xToTime(ring: ReqsSample[], frac: number): number | null {
  const b = reqsBounds(ring);
  if (!b || b.tEnd === b.t0) return b ? b.tEnd : null;
  const clamped = Math.min(1, Math.max(0, frac));
  return Math.round(b.t0 + clamped * (b.tEnd - b.t0));
}
