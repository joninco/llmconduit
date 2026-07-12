import type { ConnectionState } from '../store/dashboardStore';

/** One source of truth for when a dashboard metric publication becomes stale. */
export const METRICS_STALE_AFTER_MS = 15_000;

export type ConnectionLabel = 'Connected' | 'Connecting' | 'Reconnecting' | 'Disconnected';
export type FreshnessLabel = 'Fresh' | 'Stale' | 'Unavailable';
export type TrafficLabel = 'Active' | 'Idle';

export interface DashboardStatusModel {
  connection: {
    label: ConnectionLabel;
    tone: 'healthy' | 'cooling' | 'down' | 'muted';
    detail: string;
  };
  freshness: {
    label: FreshnessLabel;
    tone: 'healthy' | 'cooling' | 'muted';
    detail: string;
    ageMs: number | null;
  };
  traffic: {
    label: TrafficLabel;
    tone: 'healthy' | 'muted';
    detail: string;
  };
}

export function formatRelativeAge(ageMs: number): string {
  const seconds = Math.max(0, Math.floor(ageMs / 1_000));
  if (seconds < 2) return 'just now';
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

export function deriveDashboardStatus({
  connection,
  hasDashboardData,
  generatedAtMs,
  activeStreams,
  lastActivityAtMs,
  nowMs,
}: {
  connection: ConnectionState;
  hasDashboardData: boolean;
  generatedAtMs: number | null;
  activeStreams: number;
  lastActivityAtMs: number | null;
  nowMs: number;
}): DashboardStatusModel {
  const connectionModel: DashboardStatusModel['connection'] =
    connection === 'live' || connection === 'seeking'
      ? {
          label: 'Connected',
          tone: connection === 'seeking' ? 'cooling' : 'healthy',
          detail: connection === 'seeking' ? 'Connected to the server; viewing a historical cut.' : 'Dashboard stream connected.',
        }
      : connection === 'connecting'
        ? {
            label: hasDashboardData ? 'Reconnecting' : 'Connecting',
            tone: 'cooling',
            detail: hasDashboardData
              ? 'The live stream is reconnecting; retained data remains visible.'
              : 'Establishing the dashboard stream.',
          }
        : {
            label: 'Disconnected',
            tone: connection === 'error' ? 'down' : 'muted',
            detail: connection === 'error' ? 'The dashboard stream encountered an error.' : 'No dashboard stream is connected.',
          };

  const metricAgeMs = generatedAtMs === null ? null : Math.max(0, nowMs - generatedAtMs);
  const freshness: DashboardStatusModel['freshness'] = metricAgeMs === null
    ? { label: 'Unavailable', tone: 'muted', detail: 'No metrics publication has been received.', ageMs: null }
    : metricAgeMs > METRICS_STALE_AFTER_MS
      ? {
          label: 'Stale',
          tone: 'cooling',
          detail: `Last metrics update ${formatRelativeAge(metricAgeMs)}.`,
          ageMs: metricAgeMs,
        }
      : {
          label: 'Fresh',
          tone: 'healthy',
          detail: `Updated ${formatRelativeAge(metricAgeMs)}.`,
          ageMs: metricAgeMs,
        };

  const activityAgeMs = lastActivityAtMs === null ? null : Math.max(0, nowMs - lastActivityAtMs);
  const traffic: DashboardStatusModel['traffic'] = activeStreams > 0
    ? {
        label: 'Active',
        tone: 'healthy',
        detail: `${activeStreams.toLocaleString()} ${activeStreams === 1 ? 'request' : 'requests'} currently flowing.`,
      }
    : {
        label: 'Idle',
        tone: 'muted',
        detail: activityAgeMs === null ? 'No request has been observed yet.' : `Last request ${formatRelativeAge(activityAgeMs)}.`,
      };

  return { connection: connectionModel, freshness, traffic };
}
