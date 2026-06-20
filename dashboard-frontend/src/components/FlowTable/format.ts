/** Display formatters for the flow table/detail. Pure + DOM-free for reuse and testing. */

/** `HH:MM:SS.mmm` clock for the timestamp column (local time, dense). */
export function fmtClock(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2) => String(n).padStart(w, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

/** Elapsed ms → compact human string (`820ms`, `4.2s`, `1m02s`). `null` ⇒ "—". */
export function fmtElapsed(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return '—';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s - m * 60);
  return `${m}m${String(rem).padStart(2, '0')}s`;
}

/** Token counts → compact (`812`, `1.5k`, `2.5m`). `null/undefined` ⇒ "—". */
export function fmtTokens(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return '—';
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}m`;
}

/** Dollar cost → `$0.0061` (4dp under a cent, 2–4dp otherwise). `null` ⇒ "—". */
export function fmtCost(cost: number | null): string {
  if (cost === null || !Number.isFinite(cost)) return '—';
  if (cost === 0) return '$0.00';
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(cost < 1 ? 4 : 2)}`;
}

/** `requested → served` model pair, eliding when identical or absent. */
export function fmtModelPair(requested?: string | null, served?: string | null): string {
  if (requested && served) return requested === served ? served : `${requested} → ${served}`;
  return served ?? requested ?? '—';
}
