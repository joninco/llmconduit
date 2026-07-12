import { useEffect, useState } from 'react';
import { formatStaleAge } from '../lib/staleAge';
import { METRICS_STALE_AFTER_MS } from '../lib/dashboardStatus';
import { useDashboard } from '../store/hooks';

/**
 * Shared provenance banner for analytics restored from the most recent request-bearing window.
 *
 * U1 — idle must not look like failure: while the gateway is HEALTHY (stream connected, metrics
 * publication fresh) and simply has no traffic, showing the retained window is the normal idle
 * state — the banner renders neutral/informational. The yellow warning voice is reserved for
 * data that is stale when it should not be: a degraded/disconnected stream, a stale metrics
 * publication, or traffic in flight while analytics still serve an old cut.
 */
export function StaleFallbackBanner({ asOfMs, surface }: { asOfMs: number; surface: string }) {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    setNowMs(Date.now());
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, [asOfMs]);
  const connection = useDashboard((s) => s.connection);
  const generatedAtMs = useDashboard((s) => s.metrics?.generated_at_ms ?? null);
  const activeStreams = useDashboard((s) => s.metrics?.instant.active_streams_now ?? 0);
  const metricsFresh = generatedAtMs !== null && nowMs - generatedAtMs <= METRICS_STALE_AFTER_MS;
  const healthyIdle = connection === 'live' && metricsFresh && activeStreams === 0;
  const age = formatStaleAge(nowMs - asOfMs);
  if (healthyIdle) {
    return (
      <div
        className="mb-3 flex flex-wrap items-baseline gap-2 rounded-md border border-line bg-panel px-3 py-2 text-xs text-text-muted"
        role="status"
        data-testid={`${surface}-stale-fallback`}
        data-stale="true"
        data-tone="idle"
      >
        <strong className="uppercase tracking-[0.14em]">idle</strong>
        <span>showing last activity from {new Date(asOfMs).toLocaleString()}</span>
        <span className="font-mono tabular-nums">({age} ago)</span>
      </div>
    );
  }
  return (
    <div
      className="mb-3 flex flex-wrap items-baseline gap-2 rounded-md border border-status-cooling/60 bg-status-cooling/10 px-3 py-2 text-xs text-status-cooling"
      role="status"
      data-testid={`${surface}-stale-fallback`}
      data-stale="true"
      data-tone="warning"
    >
      <strong className="uppercase tracking-[0.14em]">stale fallback</strong>
      <span>as of {new Date(asOfMs).toLocaleString()}</span>
      <span className="font-mono text-sm font-bold tabular-nums">{age}</span>
      <span className="text-text-muted">since the most recent matching request window</span>
    </div>
  );
}
