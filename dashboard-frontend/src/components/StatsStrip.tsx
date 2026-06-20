/**
 * Stats-strip SLOT (top of the shell). D9 provides the slot + a live readout from the
 * metrics store; the full strip (sparklines, per-window tiles) is D11. Numeric chips use
 * tabular-nums via StatChip.
 */
import { Panel } from './ui/Panel';
import { StatChip } from './ui/StatChip';
import { useDashboard } from '../store/hooks';

export function StatsStrip() {
  const metrics = useDashboard((s) => s.metrics);
  const connection = useDashboard((s) => s.connection);

  return (
    <Panel className="m-4 mb-0 flex items-center gap-2 px-2 py-1">
      <StatChip label="req/s" value={metrics ? metrics.reqs_per_sec.toFixed(1) : '—'} accent="accent" />
      <StatChip label="active" value={metrics?.active_streams ?? '—'} />
      <StatChip label="err%" value={metrics ? metrics.error_pct.toFixed(1) : '—'} accent={metrics && metrics.error_pct > 5 ? 'down' : undefined} />
      <StatChip label="p95 ms" value={metrics?.p95 ?? '—'} />
      <StatChip label="tok/s" value={metrics?.tokens_per_sec ?? '—'} accent="healthy" />
      <StatChip label="$/min" value={metrics ? metrics.cost_per_min.toFixed(2) : '—'} accent="meta" />
      <div className="ml-auto pr-2">
        <ConnectionDot state={connection} />
      </div>
    </Panel>
  );
}

function ConnectionDot({ state }: { state: string }) {
  const color =
    state === 'live' ? 'bg-status-healthy'
    : state === 'connecting' || state === 'seeking' ? 'bg-status-cooling'
    : state === 'error' ? 'bg-status-down'
    : 'bg-text-muted';
  return (
    <span className="flex items-center gap-2 text-xs text-text-muted">
      <span className={`h-2 w-2 rounded-full ${color}`} aria-hidden />
      {state}
    </span>
  );
}
