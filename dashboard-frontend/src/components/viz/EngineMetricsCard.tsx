import { useState } from 'react';
import type { BackendProviderMetrics } from '../../api/types';
import { cn } from '../../lib/cn';
import { Panel } from '../ui/Panel';
import { ENGINE_DASH, enginePercent, engineStatusText, engineValue, engineWindow } from './backendMetrics';

type WindowName = 'm1' | 'm5' | 'h1';

export function EngineMetricsCard({
  provider,
  metrics,
  nowMs,
  compact = false,
}: {
  provider: string;
  metrics: BackendProviderMetrics | null | undefined;
  nowMs: number;
  compact?: boolean;
}) {
  const [selected, setSelected] = useState<WindowName>('m1');
  const windowName: WindowName = compact ? 'm1' : selected;
  const window = metrics ? engineWindow(metrics, windowName) : null;
  // U2: the strip's "engine gen tok/s" may show the RETAINED last-active interval while this
  // card shows the current window — same metric name with two values needs a distinguishing
  // label, so the card states its window inline (not just via the selector buttons).
  const windowLabel = windowName === 'h1' ? '1h' : `${windowName.slice(1)}m`;
  const status = metrics ? engineStatusText(metrics, nowMs) : 'unavailable';
  const statusClass = metrics?.status === 'fresh' ? 'text-status-healthy'
    : metrics?.status === 'stale' || metrics?.status === 'warming' ? 'text-status-cooling'
      : 'text-status-down';
  const cells: Array<[string, string, 'measured' | 'derived' | 'unavailable']> = [
    ['running', engineValue(metrics?.instant.running_requests, 0), metrics?.instant.running_requests == null ? 'unavailable' : 'measured'],
    ['queued', engineValue(metrics?.instant.waiting_requests, 0), metrics?.instant.waiting_requests == null ? 'unavailable' : 'measured'],
    ['KV used', enginePercent(metrics?.instant.kv_cache_utilization), metrics?.instant.kv_cache_utilization == null ? 'unavailable' : 'measured'],
    [`gen tok/s (${windowLabel})`, engineValue(window?.generated_tokens_per_sec), window?.generated_tokens_per_sec == null ? 'unavailable' : 'derived'],
    ['prefix hit', enginePercent(window?.prefix_cache_hit_ratio), window?.prefix_cache_hit_ratio == null ? 'unavailable' : 'derived'],
    ['spec accept', enginePercent(window?.speculative_acceptance_ratio), window?.speculative_acceptance_ratio == null ? 'unavailable' : 'derived'],
    ['TTFT p95', window?.histograms.ttft_ms?.p95 == null ? ENGINE_DASH : `${engineValue(window.histograms.ttft_ms.p95, 0)} ms`, window?.histograms.ttft_ms?.p95 == null ? 'unavailable' : 'derived'],
    ['ITL p95', window?.histograms.inter_token_ms?.p95 == null ? ENGINE_DASH : `${engineValue(window.histograms.inter_token_ms.p95, 0)} ms`, window?.histograms.inter_token_ms?.p95 == null ? 'unavailable' : 'derived'],
  ];
  return (
    <Panel className="min-w-0 p-3" data-testid="engine-metrics-card" data-provider={provider} data-status={metrics?.status ?? 'absent'}>
      <div className="mb-2 flex items-center gap-2">
        <span className="truncate font-mono text-xs text-text" title={provider}>{provider}</span>
        {metrics && <span className="rounded border border-line px-1 text-[9px] uppercase text-text-muted">{metrics.engine_kind}</span>}
        <span className={cn('ml-auto text-[10px]', statusClass)}>{status}</span>
      </div>
      {!compact && metrics && (
        <div className="mb-2 flex gap-1" aria-label="Engine metrics window">
          {(['m1', 'm5', 'h1'] as const).map((name) => (
            <button key={name} type="button" className={cn('rounded border px-1.5 py-0.5 text-[9px]', selected === name ? 'border-accent text-accent' : 'border-line text-text-muted')} onClick={() => setSelected(name)}>{name === 'h1' ? '1h' : name.slice(1) + 'm'}</button>
          ))}
        </div>
      )}
      <dl className="grid grid-cols-2 gap-x-3 gap-y-1">
        {cells.map(([label, value, quality]) => (
          <div key={label} className="flex items-baseline justify-between gap-2 text-[10px]" data-quality={quality}>
            <dt className="text-text-muted">{label}</dt>
            <dd className={cn('font-mono tabular-nums', quality === 'unavailable' ? 'text-text-muted' : quality === 'derived' ? 'text-accent' : 'text-text')}>{value}</dd>
          </div>
        ))}
      </dl>
      {!compact && window && (
        <div className="mt-2 border-t border-line pt-2 text-[10px] text-text-muted">
          prompt {engineValue(window.prompt_tokens_per_sec)} · cached {engineValue(window.cached_prompt_tokens_per_sec)} · complete {engineValue(window.completed_requests_per_sec)}/s · preempt {engineValue(window.preemptions_per_sec)}/s
          {Object.keys(window.finish_reasons ?? {}).length > 0 && <div className="mt-1">finish {Object.entries(window.finish_reasons ?? {}).map(([key, value]) => `${key}:${value}`).join(' · ')}</div>}
        </div>
      )}
    </Panel>
  );
}
