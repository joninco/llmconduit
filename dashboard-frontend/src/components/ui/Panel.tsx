import type { ReactNode } from 'react';
import { cn } from '../../lib/cn';

/** A dark-ops surface panel (shadcn-style primitive). */
export function Panel({
  children,
  className,
  raised,
}: {
  children: ReactNode;
  className?: string;
  raised?: boolean;
}) {
  return (
    <div
      className={cn(
        'rounded-md border border-line',
        raised ? 'bg-panel-raised' : 'bg-panel',
        className,
      )}
    >
      {children}
    </div>
  );
}
