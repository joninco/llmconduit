import { useEffect, useState } from 'react';
import { formatStaleAge } from '../lib/staleAge';

/** Shared provenance alarm for analytics restored from the most recent request-bearing window. */
export function StaleFallbackBanner({ asOfMs, surface }: { asOfMs: number; surface: string }) {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    setNowMs(Date.now());
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, [asOfMs]);
  const age = formatStaleAge(nowMs - asOfMs);
  return (
    <div
      className="mb-3 flex flex-wrap items-baseline gap-2 rounded-md border border-status-cooling/60 bg-status-cooling/10 px-3 py-2 text-xs text-status-cooling"
      role="status"
      data-testid={`${surface}-stale-fallback`}
      data-stale="true"
    >
      <strong className="uppercase tracking-[0.14em]">stale fallback</strong>
      <span>as of {new Date(asOfMs).toLocaleString()}</span>
      <span className="font-mono text-sm font-bold tabular-nums">{age}</span>
      <span className="text-text-muted">since the most recent matching request window</span>
    </div>
  );
}
