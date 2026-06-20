import { cn } from '../../lib/cn';

/**
 * A numeric stat chip. ALL numbers in the UI render through `tabular-nums` so columns
 * stay aligned (D9 acceptance criterion: tabular-nums on numeric chips).
 */
export function StatChip({
  label,
  value,
  accent,
  className,
}: {
  label: string;
  value: string | number;
  accent?: 'healthy' | 'cooling' | 'down' | 'accent' | 'meta';
  className?: string;
}) {
  const accentClass =
    accent === 'healthy' ? 'text-status-healthy'
    : accent === 'cooling' ? 'text-status-cooling'
    : accent === 'down' ? 'text-status-down'
    : accent === 'accent' ? 'text-accent'
    : accent === 'meta' ? 'text-meta'
    : 'text-text';
  return (
    <div className={cn('flex flex-col gap-grid px-3 py-2', className)}>
      <span className="text-xs uppercase tracking-wide text-text-muted">{label}</span>
      <span className={cn('tabular-nums text-lg font-medium', accentClass)}>{value}</span>
    </div>
  );
}
