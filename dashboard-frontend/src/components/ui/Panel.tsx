import type { ComponentPropsWithoutRef, ReactNode } from 'react';
import { cn } from '../../lib/cn';

/**
 * A dark-ops surface panel (shadcn-style primitive). Forwards arbitrary `<div>` props (incl.
 * `data-testid`/`data-*`/`aria-*`) to the root so callers can tag the surface for tests/e2e
 * without wrapping it (a bare `data-testid` on `<Panel>` was previously dropped silently).
 */
export function Panel({
  children,
  className,
  raised,
  ...rest
}: {
  children: ReactNode;
  className?: string;
  raised?: boolean;
} & Omit<ComponentPropsWithoutRef<'div'>, 'className' | 'children'>) {
  return (
    <div
      className={cn(
        'rounded-md border border-line',
        raised ? 'bg-panel-raised' : 'bg-panel',
        className,
      )}
      {...rest}
    >
      {children}
    </div>
  );
}
