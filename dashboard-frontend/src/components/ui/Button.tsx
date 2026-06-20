import type { ButtonHTMLAttributes } from 'react';
import { cn } from '../../lib/cn';

/** Minimal button primitive (shadcn-style). */
export function Button({
  className,
  variant = 'default',
  ...props
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: 'default' | 'danger' | 'ghost' }) {
  const variantClass =
    variant === 'danger'
      ? 'bg-status-down/15 text-status-down hover:bg-status-down/25 border-status-down/40'
      : variant === 'ghost'
        ? 'bg-transparent text-text-muted hover:text-text border-transparent'
        : 'bg-accent/15 text-accent hover:bg-accent/25 border-accent/40';
  return (
    <button
      className={cn(
        'inline-flex items-center justify-center rounded-md border px-3 py-1.5 text-sm font-medium transition-colors disabled:opacity-50 disabled:pointer-events-none',
        variantClass,
        className,
      )}
      {...props}
    />
  );
}
