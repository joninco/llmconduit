/**
 * FlowDetail — the 3-pane transformation inspector (D10 flagship).
 *
 * Layout:
 *   ┌ header: id, models, status chip, kill button ─────────────────────────────┐
 *   ├ 3 scroll-synced JSON panes ───────────────────────────────────────────────┤
 *   │   A inbound body   →   B normalized Responses   →   C upstream chat body    │
 *   │   (diff A→B left)      (diff A→B right)              (diff B→C right)        │
 *   ├ tabs: Headers / Timeline / Error ─────────────────────────────────────────┤
 *   └ deltas sub-panel (segment_append: output/reasoning/tool cards) ────────────┘
 *
 * The structural diff (./diff) tints each JSON PATH: B is tinted vs A (added/changed), C is
 * tinted vs B, and A surfaces what B removed. The panes scroll together (useScrollSync — the
 * containers, not react-virtual). Bodies absent from `/flows/:id` (evicted under the D5
 * body-free snapshot tradeoff, or while time-travel `seek` shows a historical cut) render the
 * pane's "body evicted" placeholder. Kill POSTs with CSRF, optimistically flips the row, and
 * shows a distinct state on 403.
 */
import { useMemo, useState } from 'react';
import type { FlowDetail as FlowDetailDto, FlowSummary } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { Button } from '../ui/Button';
import { StatusChip } from '../FlowTable/StatusChip';
import { fmtCost, fmtElapsed, fmtModelPair } from '../FlowTable/format';
import { elapsedMs, flowCost } from '../FlowTable/flowModel';
import { JsonPane } from '../viz/JsonPane';
import { diffLayers } from './diff';
import { joinMonitor } from './monitorJoin';
import { DeltasPanel } from './DeltasPanel';
import { Timeline } from './Timeline';
import { useScrollSync } from './useScrollSync';
import { useFlowDetail, type KillState } from './useFlowDetail';
import { cn } from '../../lib/cn';

type Tab = 'headers' | 'timeline' | 'error';

export function FlowDetail({ apiCallId, onClose }: { apiCallId: string; onClose: () => void }) {
  const { detail, liveFlow, status, seeking, mutationsEnabled, kill, killState } = useFlowDetail(apiCallId);
  const monitor = useDashboard((s) => s.monitor);
  const priceTable = useDashboard((s) => s.priceTable);
  const [tab, setTab] = useState<Tab>('headers');

  // The flow's response_id (engine id) joins the monitor ring to this flow.
  const responseId = liveFlow?.response_id ?? detail?.response_id ?? null;
  const join = useMemo(() => joinMonitor(monitor, responseId), [monitor, responseId]);

  // Structural diffs between the captured layers (path → kind).
  const diffAB = useMemo(() => diffLayers(detail?.inbound_body, detail?.normalized), [detail?.inbound_body, detail?.normalized]);
  const diffBC = useMemo(() => diffLayers(detail?.normalized, detail?.upstream_body), [detail?.normalized, detail?.upstream_body]);

  const sync = useScrollSync(3);
  const isActive = status === 'open';

  return (
    <section className="flex min-h-0 w-[46%] min-w-[420px] flex-col border-l border-line bg-panel" data-testid="flow-detail" aria-label="flow detail">
      <DetailHeader
        apiCallId={apiCallId}
        flow={liveFlow}
        detail={detail}
        cost={liveFlow ? flowCost(liveFlow, priceTable) : detail?.cost ?? null}
        seeking={seeking}
        isActive={isActive}
        mutationsEnabled={mutationsEnabled}
        killState={killState}
        onKill={() => kill(apiCallId)}
        onClose={onClose}
      />

      {/* 3 scroll-synced panes */}
      <div className="grid min-h-0 flex-1 grid-cols-3 divide-x divide-line" data-testid="pane-row">
        <JsonPane
          label="A · inbound"
          value={detail?.inbound_body}
          diff={diffAB}
          side="left"
          emptyLabel={emptyBodyLabel(seeking)}
          scrollRef={sync.refFor(0)}
          onScroll={sync.bind(0)}
        />
        <JsonPane
          label="B · normalized"
          value={detail?.normalized}
          diff={diffAB}
          side="right"
          emptyLabel={emptyBodyLabel(seeking)}
          scrollRef={sync.refFor(1)}
          onScroll={sync.bind(1)}
        />
        <JsonPane
          label="C · upstream"
          value={detail?.upstream_body}
          diff={diffBC}
          side="right"
          emptyLabel={emptyBodyLabel(seeking)}
          scrollRef={sync.refFor(2)}
          onScroll={sync.bind(2)}
        />
      </div>

      {/* tabs */}
      <div className="flex shrink-0 items-center gap-1 border-y border-line bg-panel-raised px-2 py-1" role="tablist">
        <TabButton id="headers" active={tab} onClick={setTab}>Headers</TabButton>
        <TabButton id="timeline" active={tab} onClick={setTab}>Timeline</TabButton>
        <TabButton id="error" active={tab} onClick={setTab}>Error</TabButton>
      </div>
      <div className="max-h-44 min-h-[3rem] shrink-0 overflow-auto" role="tabpanel" data-testid={`tabpanel-${tab}`}>
        {tab === 'headers' && <HeadersTab headers={detail?.inbound_headers} />}
        {tab === 'timeline' && <Timeline events={join.events} />}
        {tab === 'error' && <ErrorTab detail={detail} liveFlow={liveFlow} joinError={join.error} />}
      </div>

      {/* deltas sub-panel */}
      <div className="flex max-h-52 shrink-0 flex-col overflow-auto border-t border-line">
        <div className="sticky top-0 z-10 border-b border-line bg-panel-raised px-3 py-1 text-[10px] uppercase tracking-wide text-text-muted">
          deltas
        </div>
        <DeltasPanel segments={join.segments} />
      </div>
    </section>
  );
}

/** When seeking a historical cut, a missing body is explicitly evicted (D5 tradeoff). */
function emptyBodyLabel(seeking: boolean): string {
  return seeking ? 'body evicted (snapshot)' : 'body evicted';
}

function DetailHeader({
  apiCallId,
  flow,
  detail,
  cost,
  seeking,
  isActive,
  mutationsEnabled,
  killState,
  onKill,
  onClose,
}: {
  apiCallId: string;
  flow: FlowSummary | null;
  detail: FlowDetailDto | null;
  cost: number | null;
  seeking: boolean;
  isActive: boolean;
  mutationsEnabled: boolean;
  killState: KillState;
  onKill: () => void;
  onClose: () => void;
}) {
  const status = flow?.status ?? detail?.status ?? 'open';
  const modelReq = flow?.model_requested ?? detail?.model_requested;
  const modelServed = flow?.model_served ?? detail?.model_served;
  const upstream = flow?.upstream_target ?? detail?.upstream_target ?? '—';
  const elapsed = flow ? elapsedMs(flow, Date.now()) : detail?.elapsed_ms ?? null;

  return (
    <header className="flex shrink-0 flex-col gap-2 border-b border-line bg-panel-raised px-3 py-2">
      <div className="flex items-center gap-2">
        <StatusChip status={status} terminalReason={flow?.terminal_reason ?? detail?.terminal_reason} />
        <span className="font-mono text-sm text-text" title={apiCallId}>{apiCallId}</span>
        {seeking && (
          <span className="rounded-sm bg-status-cooling/15 px-1.5 py-0.5 text-[10px] uppercase text-status-cooling" data-testid="seek-badge">
            snapshot
          </span>
        )}
        <div className="ml-auto flex items-center gap-2">
          <KillControl isActive={isActive} mutationsEnabled={mutationsEnabled} killState={killState} onKill={onKill} />
          <button
            type="button"
            onClick={onClose}
            aria-label="close detail"
            className="rounded-md border border-transparent px-2 py-1 text-sm text-text-muted hover:text-text"
          >
            ✕
          </button>
        </div>
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
        <dt className="text-text-muted">model</dt>
        <dd className="font-mono text-text">{fmtModelPair(modelReq, modelServed)}</dd>
        <dt className="text-text-muted">upstream</dt>
        <dd className="font-mono text-text">{upstream}</dd>
        <dt className="text-text-muted">cost / elapsed</dt>
        <dd className="tabular-nums text-text">
          <span className="text-meta">{fmtCost(cost)}</span>
          <span className="text-line"> · </span>
          {fmtElapsed(elapsed)}
        </dd>
      </dl>
    </header>
  );
}

function KillControl({
  isActive,
  mutationsEnabled,
  killState,
  onKill,
}: {
  isActive: boolean;
  mutationsEnabled: boolean;
  killState: KillState;
  onKill: () => void;
}) {
  if (killState.phase === 'forbidden') {
    return <span className="text-xs text-status-down" data-testid="kill-forbidden">mutations disabled</span>;
  }
  if (killState.phase === 'killed') {
    return <span className="text-xs text-text-muted" data-testid="kill-done">killed</span>;
  }
  if (killState.phase === 'error') {
    return <span className="text-xs text-status-down" data-testid="kill-error" title={killState.message}>kill failed</span>;
  }
  if (!isActive) return null;
  return (
    <Button
      variant="danger"
      onClick={onKill}
      disabled={!mutationsEnabled || killState.phase === 'killing'}
      data-testid="kill-button"
      title={mutationsEnabled ? 'abort this flow' : 'mutations disabled'}
    >
      {killState.phase === 'killing' ? 'killing…' : 'Kill'}
    </Button>
  );
}

function TabButton({ id, active, onClick, children }: { id: Tab; active: Tab; onClick: (t: Tab) => void; children: React.ReactNode }) {
  const selected = id === active;
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      onClick={() => onClick(id)}
      className={cn(
        'rounded-md px-2.5 py-1 text-xs transition-colors',
        selected ? 'bg-accent/15 text-accent' : 'text-text-muted hover:text-text',
      )}
    >
      {children}
    </button>
  );
}

function HeadersTab({ headers }: { headers?: Record<string, string> }) {
  const entries = headers ? Object.entries(headers) : [];
  if (entries.length === 0) {
    return <div className="px-3 py-3 text-xs italic text-text-muted" data-testid="headers-empty">No inbound headers captured.</div>;
  }
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 px-3 py-2 font-mono text-xs" data-testid="headers-tab">
      {entries.map(([k, v]) => (
        <div key={k} className="contents">
          <dt className="text-accent">{k}</dt>
          <dd className="truncate text-text" title={v}>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

function ErrorTab({ detail, liveFlow, joinError }: { detail: FlowDetailDto | null; liveFlow: FlowSummary | null; joinError: string | null }) {
  const reason = liveFlow?.terminal_reason ?? detail?.terminal_reason ?? null;
  if (!reason && !joinError) {
    return <div className="px-3 py-3 text-xs italic text-text-muted" data-testid="error-empty">No error.</div>;
  }
  return (
    <div className="px-3 py-2 text-xs" data-testid="error-tab">
      {reason && (
        <div className="mb-1">
          <span className="text-text-muted">terminal reason: </span>
          <span className="font-mono text-status-down">{reason}</span>
        </div>
      )}
      {joinError && (
        <div>
          <span className="text-text-muted">monitor error: </span>
          <span className="font-mono text-status-down">{joinError}</span>
        </div>
      )}
    </div>
  );
}
