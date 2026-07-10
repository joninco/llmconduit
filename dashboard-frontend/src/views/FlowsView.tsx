/**
 * FlowsView (D10) — the flagship transformation-inspector screen: the virtualized FlowTable,
 * with a FULL-WIDTH FlowDetail drill-down takeover when a row is selected. Replaces the
 * D9 placeholder on the `#/flows` route.
 *
 * The table is driven by the live WS store ∪ the `/flows` query (useFlowRows); selecting a row
 * navigates to `#/flows/<api_call_id>` — the selection LIVES IN THE HASH, so a drill-down is
 * deep-linkable/shareable and browser back (or Esc, or the ← button) dismisses it back to the
 * table. While the drill-down is open the table column is `hidden` (NOT unmounted) so its query
 * cache, live store subscription, and scroll position survive the round-trip.
 *
 * Seek (D11 time-travel paused) coherence: while seeking, the store holds ONLY the frozen
 * snapshot cut. A selection that is ABSENT from that cut (e.g. a row selected while live, then
 * scrubbed to a moment before that flow existed) must NOT open the inspector — doing so would
 * fetch live `/flows/:id` detail for a "future" flow and leak post-seek data into the frozen
 * view (HIGH finding 1). So the EFFECTIVE selection is gated to ids present in the frozen rows
 * while seeking; on LIVE it is the raw selection again. We do NOT rewrite the hash itself, so
 * leaving seek re-reveals the same drill-down if the flow reappears in the live store.
 */
import { lazy, Suspense, useEffect } from 'react';
import { FlowTable } from '../components/FlowTable/FlowTable';
import { FailureTaxonomy } from '../components/FlowTable/FailureTaxonomy';
import { useDashboard } from '../store/hooks';
import { navigate, useHashDetail } from '../router/useHashRoute';
import { cn } from '../lib/cn';

// The inspector pulls in highlight.js and the JSON/diff machinery. Keep that cost behind
// the row-selection boundary so the Flows route itself remains light.
const FlowDetail = lazy(() =>
  import('../components/FlowDetail/FlowDetail').then((module) => ({ default: module.FlowDetail })),
);

export function FlowsView() {
  const selectedId = useHashDetail();
  const seeking = useDashboard((s) => s.connection === 'seeking');
  // During seek the store IS the frozen cut; a selection not in it is a future flow → suppress
  // (no live detail fetch). Live: the raw selection stands. The hash itself is left untouched,
  // so leaving seek re-reveals the same row if it reappears in the live store (finding 1).
  const inSnapshot = useDashboard((s) => (selectedId ? s.flows.has(selectedId) : false));
  const effectiveId = selectedId && (!seeking || inSnapshot) ? selectedId : null;

  // Esc dismisses the drill-down back to the table (same as ← / browser back).
  useEffect(() => {
    if (!effectiveId) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') navigate('flows');
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [effectiveId]);

  return (
    <div className="flex min-h-0 min-w-0 flex-1" data-testid="flows-view">
      {/* Table column: the AGGREGATE failure taxonomy (gap 14) above the virtualized flow table — so
          the operator sees "what is failing and why, in aggregate" before drilling one red row. The
          panel renders only when flows are observed (else it's absent — don't-lie-with-zeros).
          Kept MOUNTED (hidden) while the drill-down is open so table state survives dismiss. */}
      <div className={cn('min-h-0 min-w-0 flex-1 flex-col overflow-y-auto lg:overflow-hidden', effectiveId ? 'hidden' : 'flex')}>
        <FailureTaxonomy />
        <FlowTable selectedId={effectiveId} onSelect={(id) => navigate('flows', id)} />
      </div>
      {effectiveId && (
        <Suspense
          fallback={
            <div className="flex min-h-0 flex-1 items-center justify-center text-sm text-text-muted" role="status">
              Loading flow detail…
            </div>
          }
        >
          <FlowDetail key={effectiveId} apiCallId={effectiveId} onClose={() => navigate('flows')} />
        </Suspense>
      )}
    </div>
  );
}
