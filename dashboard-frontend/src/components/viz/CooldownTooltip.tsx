/**
 * CooldownTooltip (D12) — the hover card for a topology provider node. Surfaces the D4
 * `ProviderHealth` detail the radial map can't show inline: the cooldown countdown
 * (`cooling_until_ms`), last error, failover/consecutive-failure counters, served count, the p99
 * latency (finding 6), and the catalog size. Positioned as a fixed overlay at the hovered node's
 * screen coordinates (the topology reports them via `onHover`), so d3 keeps owning the SVG while
 * React owns the tooltip.
 *
 * The countdown is computed against an EXPLICIT `nowMs` clock supplied by the view (finding 1): live
 * it is the wall clock (ticked once a second); while SEEKING it is FROZEN to `seekAtMs` so the
 * historical view never advances into the future (a cooldown that was 8 s out at the seeked instant
 * stays "cools in 8s", not a value that drifts as real time passes). The provider-level p99 is not
 * on the D4 DTO, so the view sources it from the metrics window (frozen cut while seeking) and
 * passes the overall-window value with a clear "window" label (finding 6).
 */
import type { ProviderHealth } from '../../api/types';
import { statusColor } from '../../design/tokens';

/** Format a future `cooling_until_ms` as a countdown ("cools in 8s") against `nowMs`, else "—". */
function cooldownLabel(coolingUntilMs: number | null, nowMs: number): string {
  if (coolingUntilMs == null) return '—';
  const remaining = coolingUntilMs - nowMs;
  if (remaining <= 0) return 'ready';
  return `cools in ${Math.ceil(remaining / 1000)}s`;
}

/** Format a p99 latency (ms) for the tooltip, or "—" when unavailable. */
function p99Label(p99: number | null): string {
  if (p99 == null) return '—';
  return `${Math.round(p99)}ms`;
}

export interface CooldownTooltipProps {
  /** The CURRENT provider health (re-resolved by id each render by the view — finding 7). */
  health: ProviderHealth;
  /** Hovered node center in the SVG's client coordinate space (the topology reports it via onHover). */
  x: number;
  y: number;
  /** The clock the countdown is measured against: wall clock live, FROZEN `seekAtMs` while seeking. */
  nowMs: number;
  /** Overall metrics-window p99 (ms) — frozen while seeking; null when unavailable (finding 6). */
  p99: number | null;
}

export function CooldownTooltip({ health: h, x, y, nowMs, p99 }: CooldownTooltipProps) {
  return (
    <div
      role="tooltip"
      data-testid="cooldown-tooltip"
      className="pointer-events-none fixed z-50 w-60 -translate-x-1/2 translate-y-3 rounded-md border border-line bg-panel-raised p-2.5 text-xs shadow-lg"
      style={{ left: x, top: y }}
    >
      <div className="mb-1 flex items-center gap-2">
        <span className="h-2 w-2 rounded-full" style={{ background: statusColor(h.status) }} aria-hidden />
        <span className="truncate font-mono text-text">{h.name}</span>
        <span className="ml-auto uppercase tracking-wide text-text-muted">{h.status}</span>
      </div>
      <dl className="grid grid-cols-2 gap-x-2 gap-y-0.5 text-text-muted">
        <dt>cooldown</dt>
        <dd className="text-right tabular-nums text-text" data-testid="tooltip-cooldown">{cooldownLabel(h.cooling_until_ms, nowMs)}</dd>
        <dt>served</dt>
        <dd className="text-right tabular-nums text-text">{h.served_count}</dd>
        <dt>failovers</dt>
        <dd className="text-right tabular-nums text-text">{h.failover_count}</dd>
        <dt>consec. fails</dt>
        <dd className="text-right tabular-nums text-text">{h.consecutive_failures}</dd>
        {/* p99 is NOT per-provider on the D4 DTO — show the overall metrics-window p99, labeled as
            such so it is not mistaken for this node's own latency (finding 6). */}
        <dt>p99 (window)</dt>
        <dd className="text-right tabular-nums text-text" data-testid="tooltip-p99">{p99Label(p99)}</dd>
        <dt>catalog</dt>
        <dd className="text-right tabular-nums text-text">{h.catalog_size}</dd>
      </dl>
      {h.last_error && (
        <p className="mt-1.5 truncate border-t border-line pt-1.5 text-status-down" title={h.last_error} data-testid="tooltip-error">
          {h.last_error}
        </p>
      )}
    </div>
  );
}
