import type { ReactNode } from 'react';
import { cn } from '../../lib/cn';

export type EmptyStateKind = 'empty' | 'unavailable' | 'disabled' | 'not-applicable' | 'insufficient';

const LABEL: Record<EmptyStateKind, string> = {
  empty: 'Empty',
  unavailable: 'Unavailable',
  disabled: 'Capture disabled',
  'not-applicable': 'Not applicable',
  insufficient: 'Not enough samples',
};

export function EmptyState({
  kind,
  children,
  action,
  className,
  testId,
}: {
  kind: EmptyStateKind;
  children: ReactNode;
  action?: ReactNode;
  className?: string;
  testId?: string;
}) {
  return (
    <div
      className={cn('flex flex-wrap items-center gap-x-2 gap-y-1 px-3 py-2 text-xs text-text-muted', className)}
      data-state={kind}
      data-testid={testId}
      role="status"
    >
      <span className="rounded border border-line px-1.5 py-0.5 text-[10px] font-medium text-text-muted">{LABEL[kind]}</span>
      <span>{children}</span>
      {action && <span className="ml-auto">{action}</span>}
    </div>
  );
}
