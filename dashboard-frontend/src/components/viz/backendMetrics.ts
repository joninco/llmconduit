import type { BackendMetricsWindow, BackendProviderMetrics } from '../../api/types';

export const ENGINE_DASH = '—';

export type EngineQuality = 'measured' | 'derived' | 'unavailable';

export function engineWindow(
  metrics: BackendProviderMetrics,
  window: 'm1' | 'm5' | 'h1',
): BackendMetricsWindow {
  return metrics.windows[window];
}

export function engineValue(value: number | null | undefined, digits = 1): string {
  return value == null || !Number.isFinite(value) ? ENGINE_DASH : value.toFixed(digits);
}

export function enginePercent(value: number | null | undefined): string {
  return value == null || !Number.isFinite(value) ? ENGINE_DASH : `${(value * 100).toFixed(1)}%`;
}

export function engineAge(metrics: BackendProviderMetrics, nowMs: number): string | null {
  if (metrics.last_success_ms == null) return null;
  return `${Math.max(0, Math.floor((nowMs - metrics.last_success_ms) / 1000))}s old`;
}

export function engineStatusText(metrics: BackendProviderMetrics, nowMs: number): string {
  if (metrics.status === 'stale') return `stale${engineAge(metrics, nowMs) ? ` · ${engineAge(metrics, nowMs)}` : ''}`;
  if (metrics.status === 'unsupported') return 'unsupported schema';
  if (metrics.status === 'error') return metrics.last_error_class ? `error · ${metrics.last_error_class}` : 'unavailable';
  if (metrics.status === 'warming') return 'warming';
  return metrics.coverage === 'partial' ? 'fresh · partial' : 'fresh';
}
