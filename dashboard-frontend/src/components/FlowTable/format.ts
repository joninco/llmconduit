/** Display formatters for the flow table/detail. Pure + DOM-free for reuse and testing. */

/** `HH:MM:SS.mmm` clock for the timestamp column (local time, dense). */
export function fmtClock(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2) => String(n).padStart(w, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

const DASH = '—';

function finite(value: number | null | undefined): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

function trim(value: string): string {
  return value.replace(/(\.\d*?[1-9])0+$/u, '$1').replace(/\.0+$/u, '');
}

/** Elapsed ms → compact human string (`820 ms`, `4.2 s`, `1 m 02 s`). */
export function fmtElapsed(ms: number | null): string {
  if (!finite(ms) || ms < 0) return DASH;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const s = ms / 1000;
  if (s < 60) return `${trim(s.toFixed(s < 10 ? 1 : 0))} s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s - m * 60);
  return `${m} m ${String(rem).padStart(2, '0')} s`;
}

export const fmtLatency = fmtElapsed;

/** Token counts → compact (`812`, `1.5k`, `2.5m`). `null/undefined` ⇒ "—". */
export function fmtTokens(n: number | null | undefined): string {
  if (!finite(n) || n < 0) return DASH;
  if (n < 1000) return trim(n.toFixed(Number.isInteger(n) ? 0 : 1));
  if (n < 1_000_000) return `${trim((n / 1000).toFixed(1))}k`;
  return `${trim((n / 1_000_000).toFixed(2))}m`;
}

/** Dollar cost → `$0.0061` (4dp under a cent, 2–4dp otherwise). `null` ⇒ "—". */
export function fmtCost(cost: number | null): string {
  if (!finite(cost)) return DASH;
  if (cost === 0) return '$0.00';
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(cost < 1 ? 4 : 2)}`;
}

/** `requested → served` model pair, eliding when identical or absent. */
export function fmtModelPair(requested?: string | null, served?: string | null): string {
  if (requested && served) return requested === served ? served : `${requested} → ${served}`;
  return served ?? requested ?? '—';
}

/**
 * Throughput → compact `tok/s` (`142 tok/s`, `1.2k tok/s`). `null` ⇒ "—" (UNAVAILABLE, never a
 * fabricated `0`). A measured `0` would read `0 tok/s` (distinct from unavailable). Used by the
 * gap-10 latency breakdown's derived stream-rate readout.
 */
export function fmtTokensPerSec(n: number | null): string {
  if (!finite(n) || n < 0) return DASH;
  if (n < 1000) return `${trim(n.toFixed(n < 10 ? 1 : 0))} tok/s`;
  return `${trim((n / 1000).toFixed(1))}k tok/s`;
}

export function fmtRate(n: number | null | undefined, unit = 'req/s'): string {
  if (!finite(n) || n < 0) return DASH;
  const value = n >= 1000
    ? `${trim((n / 1000).toFixed(1))}k`
    : n >= 10 ? trim(n.toFixed(0))
      : n >= 1 ? trim(n.toFixed(1))
        : n === 0 ? '0'
          : n >= 0.1 ? trim(n.toFixed(2))
            : n >= 0.01 ? trim(n.toFixed(3))
              : trim(n.toFixed(4));
  return unit ? `${value} ${unit}` : value;
}

export function fmtPercent(n: number | null | undefined): string {
  if (!finite(n) || n < 0) return DASH;
  return `${trim(n.toFixed(1))}%`;
}

export function fmtSamples(n: number | null | undefined, noun = 'sample'): string {
  if (!finite(n) || n < 0) return DASH;
  const rounded = Math.round(n);
  return `${rounded.toLocaleString('en-US')} ${noun}${rounded === 1 ? '' : 's'}`;
}

export function fmtCostRate(n: number | null | undefined): string {
  if (!finite(n) || n < 0) return DASH;
  if (n === 0) return '$0.00/min';
  const precision = n >= 0.1 ? 2 : n >= 0.01 ? 3 : 4;
  return `$${n.toFixed(precision)}/min`;
}
