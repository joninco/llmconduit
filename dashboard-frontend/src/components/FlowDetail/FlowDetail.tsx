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
import { combineMiddleDiff, diffLayers } from './diff';
import { joinMonitor } from './monitorJoin';
import { mergeDeltas, normalizeRestDeltas, type SeqSegment } from './deltas';
import { DeltasPanel } from './DeltasPanel';
import { Timeline } from './Timeline';
import { useScrollSync } from './useScrollSync';
import { useFlowDetail, type KillState } from './useFlowDetail';
import { cn } from '../../lib/cn';

type Tab = 'headers' | 'timeline' | 'error';

export function FlowDetail({ apiCallId, onClose }: { apiCallId: string; onClose: () => void }) {
  const { detail, frozenDetail, liveFlow, status, seeking, seekMonitorSeq, seekAtMs, mutationsEnabled, kill, killState } =
    useFlowDetail(apiCallId);
  const monitor = useDashboard((s) => s.monitor);
  const monitorSeqs = useDashboard((s) => s.monitorSeqs);
  const priceTable = useDashboard((s) => s.priceTable);
  const [tab, setTab] = useState<Tab>('headers');
  // Shared search across all three layers (A inbound · B normalized · C upstream) — find a field
  // once and see how it transformed. Each JsonPane filters to matches + their ancestors.
  const [query, setQuery] = useState('');

  // The flow's response_id (engine id) joins the monitor ring to this flow. While seeking we read
  // it from the FROZEN row (not the live REST detail, which is withheld from non-body surfaces).
  const responseId = liveFlow?.response_id ?? frozenDetail?.response_id ?? null;
  // SEEK BOUND (finding 1): bound the join to the frozen `monitor_seq` so post-cut segments/events/
  // status never leak into the deltas/timeline/error. Live ⇒ no bound (the whole ring is current).
  const join = useMemo(
    () => joinMonitor(monitor, responseId, { seqs: monitorSeqs, maxSeq: seekMonitorSeq }),
    [monitor, monitorSeqs, responseId, seekMonitorSeq],
  );

  // Deltas shown in the sub-panel = the REST replay (base) MERGED with the live monitor segments
  // (appended) — finding 5. While seeking, the live REST replay (`detail.deltas`) is post-cut and
  // withheld; the cut-bounded monitor join alone supplies the frozen stream (finding 1). The merge
  // de-dups the seam by MonitorHub seq (finding 2): the live side carries `join.segmentSeqs` (the
  // per-segment `monitor_seq`) so a coalesced/same-millisecond tail merges without dup or drop.
  const liveSegs = useMemo<SeqSegment[]>(
    () => join.segments.map((segment, i) => ({ segment, seq: join.segmentSeqs[i] ?? null })),
    [join.segments, join.segmentSeqs],
  );
  const segments = useMemo(
    () => mergeDeltas(normalizeRestDeltas(frozenDetail?.deltas), liveSegs),
    [frozenDetail?.deltas, liveSegs],
  );

  // Structural diffs between the captured layers (path → kind).
  const diffAB = useMemo(() => diffLayers(detail?.inbound_body, detail?.normalized), [detail?.inbound_body, detail?.normalized]);
  const diffBC = useMemo(() => diffLayers(detail?.normalized, detail?.upstream_body), [detail?.normalized, detail?.upstream_body]);
  // Pane B sits between both comparisons: it shows what A→B added/changed AND what B→C removes,
  // so it renders the COMBINED middle diff with side `both` (finding 4).
  const diffBMiddle = useMemo(() => combineMiddleDiff(diffAB, diffBC), [diffAB, diffBC]);

  const sync = useScrollSync(3);
  const isActive = status === 'open';

  // Cost from the MERGED flow+detail data: prefer the server roll-up (detail.cost, else the live
  // row's cost), and only fall back to usage×price when neither roll-up exists. Building a merged
  // summary (live status/usage wins; roll-up cost + detail fields fill gaps) lets `flowCost` apply
  // its own roll-up-first precedence, so a live row LACKING cost no longer hides `detail.cost`.
  //
  // SEEK coherence (finding 1/3): while seeking, `liveFlow` IS the frozen snapshot row and the live
  // REST detail is withheld (`frozenDetail` is null) — so cost derives EXCLUSIVELY from the frozen
  // summary, never the live REST roll-up.
  const cost = useMemo(() => {
    const summary = frozenDetail;
    if (!liveFlow && !summary) return null;
    const merged: FlowSummary = {
      ...(summary ?? {}),
      ...(liveFlow ?? {}),
      api_call_id: apiCallId,
      method: liveFlow?.method ?? 'POST',
      uri: liveFlow?.uri ?? '',
      status: status ?? liveFlow?.status ?? summary?.status ?? 'open',
      started_ms: liveFlow?.started_ms ?? summary?.started_ms ?? 0,
      // Roll-up precedence: server detail roll-up first, then the live row's own roll-up.
      cost: summary?.cost ?? liveFlow?.cost ?? null,
      // Freshest usage/model for the usage×price fallback: live row first, then detail.
      usage: liveFlow?.usage ?? summary?.usage ?? null,
      model_served: liveFlow?.model_served ?? summary?.model_served ?? null,
      model_requested: liveFlow?.model_requested ?? summary?.model_requested ?? null,
    };
    return flowCost(merged, priceTable);
  }, [liveFlow, frozenDetail, apiCallId, status, priceTable]);

  return (
    <section className="flex min-h-0 w-[46%] min-w-[420px] flex-col border-l border-line bg-panel" data-testid="flow-detail" aria-label="flow detail">
      <DetailHeader
        apiCallId={apiCallId}
        flow={liveFlow}
        detail={frozenDetail}
        cost={cost}
        seeking={seeking}
        seekAtMs={seekAtMs}
        isActive={isActive}
        mutationsEnabled={mutationsEnabled}
        killState={killState}
        onKill={() => kill(apiCallId)}
        onClose={onClose}
      />

      <SearchBar value={query} onChange={setQuery} />

      {/* 3 scroll-synced panes */}
      <div className="grid min-h-0 flex-1 grid-cols-3 divide-x divide-line" data-testid="pane-row">
        <JsonPane
          label="A · inbound"
          value={detail?.inbound_body}
          diff={diffAB}
          side="left"
          query={query}
          emptyLabel={emptyBodyLabel(seeking)}
          scrollRef={sync.refFor(0)}
          onScroll={sync.bind(0)}
        />
        <JsonPane
          label="B · normalized"
          value={detail?.normalized}
          diff={diffBMiddle}
          side="both"
          query={query}
          emptyLabel={emptyBodyLabel(seeking)}
          scrollRef={sync.refFor(1)}
          onScroll={sync.bind(1)}
        />
        <JsonPane
          label="C · upstream"
          value={detail?.upstream_body}
          diff={diffBC}
          side="right"
          query={query}
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
        {/* Headers + Error read the FROZEN detail (null while seeking) so no live/post-cut metadata
            leaks; Timeline reads the cut-bounded monitor join (finding 1). */}
        {tab === 'headers' && <HeadersTab headers={frozenDetail?.inbound_headers} />}
        {tab === 'timeline' && <Timeline events={join.events} />}
        {tab === 'error' && <ErrorTab detail={frozenDetail} liveFlow={liveFlow} joinError={join.error} />}
      </div>

      {/* deltas sub-panel */}
      <div className="flex max-h-52 shrink-0 flex-col overflow-auto border-t border-line">
        <div className="sticky top-0 z-10 border-b border-line bg-panel-raised px-3 py-1 text-[10px] uppercase tracking-wide text-text-muted">
          deltas
        </div>
        <DeltasPanel segments={segments} />
      </div>
    </section>
  );
}

/** When seeking a historical cut, a missing body is explicitly evicted (D5 tradeoff). */
function emptyBodyLabel(seeking: boolean): string {
  return seeking ? 'body evicted (snapshot)' : 'body evicted';
}

/** Shared search across the three layers — one query, every pane filters + highlights. */
function SearchBar({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return (
    <div
      className="flex shrink-0 items-center gap-2 border-b border-line bg-panel-raised px-3 py-1.5"
      data-testid="json-search-bar"
    >
      <div className="flex flex-1 items-center gap-2 rounded-md border border-line bg-panel px-2 py-1 transition-colors focus-within:border-accent/60">
        <svg viewBox="0 0 16 16" className="h-3.5 w-3.5 shrink-0 text-text-muted" fill="none" aria-hidden="true">
          <circle cx="7" cy="7" r="4.5" stroke="currentColor" strokeWidth="1.5" />
          <path d="M10.5 10.5 14 14" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
        <input
          type="text"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder="search all layers…"
          spellCheck={false}
          className="min-w-0 flex-1 bg-transparent font-mono text-xs text-text placeholder:text-text-muted focus:outline-none"
          data-testid="json-search-input"
        />
        {value && (
          <button
            type="button"
            onClick={() => onChange('')}
            aria-label="clear search"
            className="shrink-0 text-text-muted transition-colors hover:text-text"
          >
            ✕
          </button>
        )}
      </div>
      <span className="hidden shrink-0 font-mono text-[10px] uppercase tracking-[0.14em] text-text-muted sm:inline">
        A · B · C
      </span>
    </div>
  );
}

function DetailHeader({
  apiCallId,
  flow,
  detail,
  cost,
  seeking,
  seekAtMs,
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
  seekAtMs: number | null;
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
  // Elapsed: live = `elapsedMs` (which ticks an OPEN flow against `now`). SEEK coherence
  // (finding 6): a frozen cut must NOT read wall-clock `Date.now()` — that would leak time elapsed
  // AFTER the seeked instant. We pass the frozen cut `at_ms` as `now`, so an OPEN historical flow
  // reads its elapsed AS OF the cut (`at_ms - started_ms`), consistent with the table (which uses
  // the same `at_ms`); a finished flow still derives `finished-started` from the frozen row. `detail`
  // here is already the FROZEN detail (null while seeking), so non-body surfaces never read live.
  const elapsed = flow
    ? elapsedMs(flow, seeking ? seekAtMs ?? flow.started_ms : Date.now())
    : (seeking ? null : detail?.elapsed_ms ?? null);

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
          <KillControl isActive={isActive} mutationsEnabled={mutationsEnabled} seeking={seeking} killState={killState} onKill={onKill} />
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
  seeking,
  killState,
  onKill,
}: {
  isActive: boolean;
  mutationsEnabled: boolean;
  seeking: boolean;
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
  // Kill mutates LIVE state; a frozen historical cut must not be mutable (finding 2). While
  // seeking we DISABLE the button — the optimistic `patchFlowStatus` would otherwise mutate the
  // frozen store row and the POST would abort a flow the operator is only inspecting in the past.
  const disabled = !mutationsEnabled || seeking || killState.phase === 'killing';
  const title = seeking ? 'paused (time-travel)' : mutationsEnabled ? 'abort this flow' : 'mutations disabled';
  return (
    <Button
      variant="danger"
      onClick={onKill}
      disabled={disabled}
      data-testid="kill-button"
      title={title}
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
