/**
 * FlowsView (D10) — the flagship transformation-inspector screen: the virtualized FlowTable on
 * the left, the 3-pane FlowDetail inspector on the right when a row is selected. Replaces the
 * D9 placeholder on the `#/flows` route.
 *
 * The table is driven by the live WS store ∪ the `/flows` query (useFlowRows); selecting a row
 * opens its inspector (`/flows/:id` + the monitor join). The split is plain flex (the detail
 * panel is conditionally mounted) so an unselected state shows the full-width table.
 *
 * Seek (D11 time-travel paused) coherence: while seeking, the store holds ONLY the frozen
 * snapshot cut. A selection that is ABSENT from that cut (e.g. a row selected while live, then
 * scrubbed to a moment before that flow existed) must NOT open the inspector — doing so would
 * fetch live `/flows/:id` detail for a "future" flow and leak post-seek data into the frozen
 * view (HIGH finding 1). So the EFFECTIVE selection is gated to ids present in the frozen rows
 * while seeking; on LIVE it is the raw selection again.
 */
import { useState } from 'react';
import { FlowTable } from '../components/FlowTable/FlowTable';
import { FlowDetail } from '../components/FlowDetail/FlowDetail';
import { useDashboard } from '../store/hooks';

export function FlowsView() {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const seeking = useDashboard((s) => s.connection === 'seeking');
  // During seek the store IS the frozen cut; a selection not in it is a future flow → suppress
  // (no live detail fetch). Live: the raw selection stands. We do NOT clear `selectedId` itself,
  // so leaving seek re-reveals the same row if it reappears in the live store (finding 1).
  const inSnapshot = useDashboard((s) => (selectedId ? s.flows.has(selectedId) : false));
  const effectiveId = selectedId && (!seeking || inSnapshot) ? selectedId : null;

  return (
    <div className="flex min-h-0 min-w-0 flex-1" data-testid="flows-view">
      <FlowTable selectedId={effectiveId} onSelect={setSelectedId} />
      {effectiveId && <FlowDetail key={effectiveId} apiCallId={effectiveId} onClose={() => setSelectedId(null)} />}
    </div>
  );
}
