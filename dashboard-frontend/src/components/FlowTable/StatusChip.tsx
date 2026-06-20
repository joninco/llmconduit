/**
 * Status chip for a flow row: running (pulse) / 2xx / 4xx / 5xx, colored from the design
 * tokens. The pulse is a CSS `animate-pulse` on the DOT only (not a layout/transform on the
 * row) so it never triggers a FLIP on the virtualized list (D10 constraint). Honors
 * prefers-reduced-motion globally via index.css.
 */
import type { FlowStatus } from '../../api/types';
import { statusClass, type StatusClass } from './flowModel';
import { cn } from '../../lib/cn';

const CLASS_STYLE: Record<StatusClass, { dot: string; text: string; label: (s: FlowStatus) => string }> = {
  running: { dot: 'bg-status-cooling animate-pulse', text: 'text-status-cooling', label: () => 'running' },
  ok: { dot: 'bg-status-healthy', text: 'text-status-healthy', label: () => '2xx' },
  'client-error': { dot: 'bg-status-cooling', text: 'text-status-cooling', label: (s) => (s === 'cancelled' ? 'killed' : '4xx') },
  'server-error': { dot: 'bg-status-down', text: 'text-status-down', label: () => '5xx' },
};

export function StatusChip({ status, terminalReason }: { status: FlowStatus; terminalReason?: string | null }) {
  const klass = statusClass(status, terminalReason);
  const style = CLASS_STYLE[klass];
  return (
    <span className={cn('inline-flex items-center gap-1.5 text-xs font-medium', style.text)} data-status-class={klass}>
      <span className={cn('h-1.5 w-1.5 rounded-full', style.dot)} aria-hidden />
      {style.label(status)}
    </span>
  );
}
