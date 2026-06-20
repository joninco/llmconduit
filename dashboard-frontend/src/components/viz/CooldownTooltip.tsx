/**
 * CooldownTooltip (D12) — the hover card for a topology provider node. Surfaces the D4
 * `ProviderHealth` detail the radial map can't show inline: the cooldown countdown
 * (`cooling_until_ms`), last error, failover/consecutive-failure counters, served count, and the
 * catalog size. Positioned as a fixed overlay at the hovered node's screen coordinates (the
 * topology reports them via `onHover`), so d3 keeps owning the SVG while React owns the tooltip.
 */
import type { TopoHover } from './RadialTopology';
import { statusColor } from '../../design/tokens';

/** Format a future `cooling_until_ms` as a live countdown ("cools in 8s"), else "—". */
function cooldownLabel(coolingUntilMs: number | null): string {
  if (coolingUntilMs == null) return '—';
  const remaining = coolingUntilMs - Date.now();
  if (remaining <= 0) return 'ready';
  return `cools in ${Math.ceil(remaining / 1000)}s`;
}

export function CooldownTooltip({ hover }: { hover: TopoHover }) {
  const h = hover.health;
  return (
    <div
      role="tooltip"
      data-testid="cooldown-tooltip"
      className="pointer-events-none fixed z-50 w-60 -translate-x-1/2 translate-y-3 rounded-md border border-line bg-panel-raised p-2.5 text-xs shadow-lg"
      style={{ left: hover.x, top: hover.y }}
    >
      <div className="mb-1 flex items-center gap-2">
        <span className="h-2 w-2 rounded-full" style={{ background: statusColor(h.status) }} aria-hidden />
        <span className="truncate font-mono text-text">{h.name}</span>
        <span className="ml-auto uppercase tracking-wide text-text-muted">{h.status}</span>
      </div>
      <dl className="grid grid-cols-2 gap-x-2 gap-y-0.5 text-text-muted">
        <dt>cooldown</dt>
        <dd className="text-right tabular-nums text-text" data-testid="tooltip-cooldown">{cooldownLabel(h.cooling_until_ms)}</dd>
        <dt>served</dt>
        <dd className="text-right tabular-nums text-text">{h.served_count}</dd>
        <dt>failovers</dt>
        <dd className="text-right tabular-nums text-text">{h.failover_count}</dd>
        <dt>consec. fails</dt>
        <dd className="text-right tabular-nums text-text">{h.consecutive_failures}</dd>
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
