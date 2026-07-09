/**
 * A collapsed panel's thin edge strip: a small label that IS the re-expand button — the panel
 * is "out of the way", never hidden entirely (and its splitters never stack on one pixel).
 * `vertical` renders a rotated label for side rails/panes; `horizontal` a one-line band for
 * top/bottom chrome.
 */
import { cn } from '../../lib/cn';

export function EdgeStrip({
  label,
  onExpand,
  testid,
  orientation = 'vertical',
  className,
}: {
  label: string;
  onExpand: () => void;
  testid: string;
  orientation?: 'vertical' | 'horizontal';
  className?: string;
}) {
  return (
    <button
      type="button"
      onClick={onExpand}
      aria-label={`expand ${label}`}
      data-testid={testid}
      className={cn(
        'flex h-full w-full bg-panel-raised/60 text-[10px] uppercase tracking-wide text-text-muted transition-colors hover:text-accent',
        orientation === 'vertical' ? 'items-start justify-center py-2' : 'items-center justify-center leading-none',
        className,
      )}
    >
      <span style={orientation === 'vertical' ? { writingMode: 'vertical-rl' } : undefined}>{label}</span>
    </button>
  );
}
