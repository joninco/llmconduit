/**
 * FlowsView (D10) — the flagship transformation-inspector screen: the virtualized FlowTable on
 * the left, the 3-pane FlowDetail inspector on the right when a row is selected. Replaces the
 * D9 placeholder on the `#/flows` route.
 *
 * The table is driven by the live WS store ∪ the `/flows` query (useFlowRows); selecting a row
 * opens its inspector (`/flows/:id` + the monitor join). The split is plain flex (the detail
 * panel is conditionally mounted) so an unselected state shows the full-width table.
 */
import { useState } from 'react';
import { FlowTable } from '../components/FlowTable/FlowTable';
import { FlowDetail } from '../components/FlowDetail/FlowDetail';

export function FlowsView() {
  const [selectedId, setSelectedId] = useState<string | null>(null);

  return (
    <div className="flex min-h-0 min-w-0 flex-1" data-testid="flows-view">
      <FlowTable selectedId={selectedId} onSelect={setSelectedId} />
      {selectedId && <FlowDetail key={selectedId} apiCallId={selectedId} onClose={() => setSelectedId(null)} />}
    </div>
  );
}
