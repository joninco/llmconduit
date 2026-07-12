import type { DashboardStatusModel } from '../../lib/dashboardStatus';
import { cn } from '../../lib/cn';

const TONE: Record<'healthy' | 'cooling' | 'down' | 'muted', string> = {
  healthy: 'bg-status-healthy',
  cooling: 'bg-status-cooling',
  down: 'bg-status-down',
  muted: 'bg-text-muted',
};

export function OperationalStatus({ model }: { model: DashboardStatusModel }) {
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1" role="status" aria-label="Dashboard operational status">
      <StatusIndicator name="Connection" item={model.connection} testId="status-connection" />
      <StatusIndicator name="Metrics" item={model.freshness} testId="status-freshness" />
      <StatusIndicator name="Traffic" item={model.traffic} testId="status-traffic" />
    </div>
  );
}

function StatusIndicator({
  name,
  item,
  testId,
}: {
  name: string;
  item: { label: string; tone: keyof typeof TONE; detail: string };
  testId: string;
}) {
  return (
    <span
      className="inline-flex min-w-0 items-center gap-1.5 text-[11px] text-text-muted"
      title={`${name}: ${item.label}. ${item.detail}`}
      data-testid={testId}
      data-state={item.label.toLowerCase()}
    >
      <span className={cn('h-2 w-2 shrink-0 rounded-full', TONE[item.tone])} aria-hidden />
      <span>{name}:</span>
      <strong className="font-medium text-text">{item.label}</strong>
      <span className={cn('text-text-muted', name === 'Connection' ? 'hidden 2xl:inline' : 'hidden sm:inline')}>· {item.detail}</span>
    </span>
  );
}
