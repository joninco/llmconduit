/**
 * CooldownTooltip (D12 · gap 13) — the hover card for a topology provider node. Surfaces the D4
 * `ProviderHealth` detail the radial map can't show inline: the cooldown countdown
 * (`cooling_until_ms`), last error, failover/consecutive-failure counters, served count, the
 * catalog size — and (gap 13) THIS provider's per-provider p50/p95/p99 + error rate + per-class
 * failure distribution. Positioned as a fixed overlay at the hovered node's screen coordinates (the
 * topology reports them via `onHover`), so d3 keeps owning the SVG while React owns the tooltip.
 *
 * The countdown is computed against an EXPLICIT `nowMs` clock supplied by the view (finding 1): live
 * it is the wall clock (ticked once a second); while SEEKING it is FROZEN to `seekAtMs` so the
 * historical view never advances into the future (a cooldown that was 8 s out at the seeked instant
 * stays "cools in 8s", not a value that drifts as real time passes).
 *
 * Gap 13: the tooltip previously showed the GLOBAL metrics-window p99 (it could not tell an operator
 * WHICH upstream was degrading). It now shows the PER-PROVIDER latency tile, sourced from the
 * spec-12 `ProviderLatency` the view reads off the REST `/topology` (live) / `/snapshot` (seek) node
 * — NOT the live WS topology frame, which carries `per_provider` ABSENT (it does not join the
 * metrics window). An absent entry (a provider with no in-window samples) renders `—` honestly.
 */
import type { ProviderHealth, ProviderLatency } from '../../api/types';
import { statusColor } from '../../design/tokens';
import { ProviderLatencyTile } from './ProviderLatencyTile';
import { buildProviderLatency } from './providerLatency';

/** Format a future `cooling_until_ms` as a countdown ("cools in 8s") against `nowMs`, else "—". */
function cooldownLabel(coolingUntilMs: number | null, nowMs: number): string {
  if (coolingUntilMs == null) return '—';
  const remaining = coolingUntilMs - nowMs;
  if (remaining <= 0) return 'ready';
  return `cools in ${Math.ceil(remaining / 1000)}s`;
}

export interface CooldownTooltipProps {
  /** The CURRENT provider health (re-resolved by id each render by the view — finding 7). */
  health: ProviderHealth;
  /** Hovered node center in the SVG's client coordinate space (the topology reports it via onHover). */
  x: number;
  y: number;
  /** The clock the countdown is measured against: wall clock live, FROZEN `seekAtMs` while seeking. */
  nowMs: number;
  /**
   * Gap 13 — THIS provider's per-provider latency/error metrics (spec-12 `ProviderLatency`), read
   * by the view off the REST/snapshot topology node. `null`/`undefined` ⇒ no in-window samples ⇒
   * the tile renders `—` (unavailable), never a fabricated `0`.
   */
  perProvider: ProviderLatency | null | undefined;
}

export function CooldownTooltip({ health: h, x, y, nowMs, perProvider }: CooldownTooltipProps) {
  const providerModel = buildProviderLatency(perProvider, h.id);
  return (
    <div
      role="tooltip"
      data-testid="cooldown-tooltip"
      className="pointer-events-none fixed z-50 w-64 -translate-x-1/2 translate-y-3 rounded-md border border-line bg-panel-raised p-2.5 text-xs shadow-lg"
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
        <dt>catalog</dt>
        <dd className="text-right tabular-nums text-text">{h.catalog_size}</dd>
      </dl>
      {/* Gap 13: the per-provider latency tile — replaces the old global p99 with THIS provider's
          p50/p95/p99 + error rate + failure distribution (from the REST/snapshot node). */}
      <ProviderLatencyTile model={providerModel} />
      {h.last_error && (
        <p className="mt-1.5 truncate border-t border-line pt-1.5 text-status-down" title={h.last_error} data-testid="tooltip-error">
          {h.last_error}
        </p>
      )}
    </div>
  );
}
