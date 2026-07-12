import { useQuery } from '@tanstack/react-query';
import { getConnection, queryKeys } from '../api/connection';

function bytes(value: number): string {
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
  return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
}

/** Global, auth-gated alarm for a runtime archive that can no longer honor durability. */
export function DurabilityBanner() {
  const { client } = getConnection();
  const query = useQuery({
    queryKey: queryKeys.durability,
    queryFn: () => client.durability(),
    refetchInterval: 5_000,
    refetchIntervalInBackground: true,
  });

  if (!query.isError && query.data?.state !== 'degraded') return null;
  const detail = query.data;
  return (
    <div
      className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 border-b border-status-down/70 bg-status-down/15 px-4 py-2 text-xs text-status-down"
      role="alert"
      aria-live="assertive"
      data-testid="durability-critical-banner"
    >
      <strong className="uppercase tracking-[0.14em]">durability degraded</strong>
      <span className="text-text">
        {query.isError
          ? 'Archive health could not be verified.'
          : `Persistence is behind (${detail?.pending_commits ?? 0} pending; ${detail?.error_code ?? 'archive error'}).`}
      </span>
      {detail && (
        <span className="ml-auto font-mono tabular-nums text-text-muted">
          {detail.archived_flows} flows · DB {bytes(detail.database_bytes)} · artifacts {bytes(detail.artifact_bytes)}
        </span>
      )}
    </div>
  );
}
