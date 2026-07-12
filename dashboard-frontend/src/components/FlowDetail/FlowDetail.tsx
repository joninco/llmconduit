/**
 * FlowDetail — the FULL-WIDTH transformation drill-down (D10 flagship, reworked from the old
 * 46%-side-panel squeeze). Mounted by FlowsView as a takeover of the whole view area; the
 * selection lives in the hash (`#/flows/<id>`) so it is deep-linkable and Esc/←/browser-back
 * dismiss it back to the table.
 *
 * Layout:
 *   ┌ top bar: ← flows · status chip · id · seek badge · kill · ✕ ───────────────────────────┐
 *   ├ summary band: identity/cost/tokens dl │ context gauge + timing waterfall │ failover ────┤
 *   ├ main row (flex-1) ───────────────────────────────────────────┬ deltas rail ────────────┤
 *   │   transformation rail + 3 scroll-synced JSON panes          │ live segment stream     │
 *   │   A inbound  →  B normalized  →  C upstream                  │ (output/reasoning/tool) │
 *   │   (diff A→B left)  (combined middle)  (diff B→C right)       │                         │
 *   ├ tabs: Headers / Timeline / Error (full-width strip) ─────────┴─────────────────────────┤
 *   └──────────────────────────────────────────────────────────────────────────────────────────┘
 *
 * The structural diff (./diff) now drives explicit operation labels + a changes-only view: each
 * hop reports introduced/rewritten/omitted counts, and changed rows explain their counterpart
 * value instead of relying on red/green backgrounds. The panes scroll together (useScrollSync —
 * the containers, not react-virtual). Bodies absent from `/flows/:id` (evicted under the D5
 * body-free snapshot tradeoff, or while time-travel `seek` shows a historical cut) render the
 * pane's "body evicted" placeholder. Kill POSTs with CSRF, optimistically flips the row, and
 * shows a distinct state on 403.
 */
import { Fragment, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Group, Panel, Separator, useDefaultLayout, usePanelRef } from 'react-resizable-panels';
import type { CostConfidence, DebugSegment, FlowDetail as FlowDetailDto, FlowSummary, Usage } from '../../api/types';
import { useDashboard } from '../../store/hooks';
import { Button } from '../ui/Button';
import { StatusChip } from '../FlowTable/StatusChip';
import { fmtElapsed, fmtTokens } from '../FlowTable/format';
import { costDisplay, elapsedMs, flowCost } from '../FlowTable/flowModel';
import { tokenEconomics } from '../FlowTable/tokenEconomics';
import { contextLimitFor, contextUtilization, type ContextUtilization } from '../FlowTable/contextUtilization';
import { ContextGauge } from '../FlowTable/ContextGauge';
import { latencyBreakdown, type LatencyBreakdown as LatencyBreakdownModel, type SpineFlow } from './latencyBreakdown';
import { LatencyBreakdown } from './LatencyBreakdown';
import { attemptTrace, type AttemptTrace as AttemptTraceModel } from './attemptTrace';
import { AttemptTrace } from './AttemptTrace';
import { capturedErrorBody } from '../FlowTable/failureTaxonomy';
import { pickAttempts } from '../../api/attempts';
import { useCatalog } from '../FlowTable/useCatalog';
import { JsonPane } from '../viz/JsonPane';
import { combineMiddleDiff, describeChanges, diffLayers, summarizeChanges } from './diff';
import { RequestTransformationBar, type RequestViewMode, type TransformationHop } from './RequestTransformationBar';
import { joinMonitor } from './monitorJoin';
import { mergeDeltas, normalizeRestDeltas, type MonitorSegment } from './deltas';
import { DeltasPanel } from './DeltasPanel';
import { Timeline } from './Timeline';
import { useScrollSync } from './useScrollSync';
import { useFlowDetail, type KillState } from './useFlowDetail';
import { usePersistedFlag } from './layoutPrefs';
import { EdgeStrip } from '../ui/EdgeStrip';
import { cn } from '../../lib/cn';
import { useMediaQuery } from '../../lib/useMediaQuery';
import { EmptyState } from '../ui/EmptyState';

type Tab = 'headers' | 'captures' | 'timeline' | 'error';

/** Focus-mode target: one of the three JSON layers, or the deltas rail. */
type ZoomTarget = 'A' | 'B' | 'C' | 'deltas';
type NarrowTab = ZoomTarget | Tab;

const NARROW_QUERY = '(max-width: 1023px)';
const DRAWER_TABS: ReadonlyArray<{ id: Tab; label: string }> = [
  { id: 'headers', label: 'Headers' },
  { id: 'captures', label: 'Captured I/O' },
  { id: 'timeline', label: 'Timeline' },
  { id: 'error', label: 'Error' },
];
const NARROW_TABS: ReadonlyArray<{ id: NarrowTab; label: string }> = [
  { id: 'A', label: 'A · inbound' },
  { id: 'B', label: 'B · normalized' },
  { id: 'C', label: 'C · upstream' },
  { id: 'deltas', label: 'Deltas' },
  { id: 'headers', label: 'Headers' },
  { id: 'captures', label: 'Captured I/O' },
  { id: 'timeline', label: 'Timeline' },
  { id: 'error', label: 'Error' },
];

function findFlowTrigger(apiCallId: string): HTMLButtonElement | null {
  if (typeof document === 'undefined') return null;
  return Array.from(document.querySelectorAll<HTMLButtonElement>('[data-testid="flow-row"] button'))
    .find((button) => button.title === apiCallId) ?? null;
}

function restoreFlowTrigger(apiCallId: string, preferred: HTMLElement | null, attempts = 12): void {
  const target = preferred?.isConnected ? preferred : findFlowTrigger(apiCallId);
  if (target?.isConnected) {
    target.focus({ preventScroll: true });
    return;
  }
  // The virtualized list is hidden while detail owns the route. Its first measurable render can
  // land a frame or two after this component unmounts, so wait for the actual trigger instead of
  // moving focus to a generic page fallback.
  if (attempts > 0) requestAnimationFrame(() => restoreFlowTrigger(apiCallId, preferred, attempts - 1));
}

/**
 * Splitter visuals: a thin `border-line`-colored strip with an accent on hover/keyboard focus.
 * The library expands the pointer hit-target well beyond the 1px visual, owns the resize cursor
 * and arrow-key resizing, and double-click resets the neighboring panel to its default size.
 */
const SPLIT_V = 'w-px bg-line outline-none transition-colors hover:bg-accent/70 focus-visible:bg-accent';
const SPLIT_H = 'h-px bg-line outline-none transition-colors hover:bg-accent/70 focus-visible:bg-accent';

export function FlowDetail({ apiCallId, onClose }: { apiCallId: string; onClose: () => void }) {
  const { detail, detailQuery, frozenDetail, liveFlow, status, seeking, seekMonitorSeq, seekAtMs, mutationsEnabled, kill, killState } =
    useFlowDetail(apiCallId);
  const monitor = useDashboard((s) => s.monitor);
  const monitorSeqs = useDashboard((s) => s.monitorSeqs);
  // Gap 09: per-model context-window capacities (gap-06 nullable `context_limit`), for the gauge.
  const contextLimits = useCatalog();
  const [tab, setTab] = useState<Tab>('headers');
  const [narrowTab, setNarrowTab] = useState<NarrowTab>('A');
  const narrow = useMediaQuery(NARROW_QUERY);
  const detailRef = useRef<HTMLElement>(null);
  const openerRef = useRef<HTMLElement | null>(null);
  const openerCapturedRef = useRef(false);
  if (!openerCapturedRef.current) {
    openerCapturedRef.current = true;
    const activeElement = typeof document !== 'undefined'
      && document.activeElement instanceof HTMLElement
      && document.activeElement !== document.body
      ? document.activeElement
      : null;
    openerRef.current = activeElement ?? findFlowTrigger(apiCallId);
  }
  // Shared search across all three layers (A inbound · B normalized · C upstream) — find a field
  // once and see how it transformed. Each JsonPane filters to matches + their ancestors.
  const [query, setQuery] = useState('');
  // Lead with operations rather than three walls of JSON. Full bodies remain one click away, and
  // search always scans every field regardless of this presentation mode.
  const [requestView, setRequestView] = useState<RequestViewMode>('changes');
  const detailLoading = detail === null && detailQuery.isPending && detailQuery.fetchStatus === 'fetching';
  const detailFailed = detail === null && detailQuery.isError;
  const bodyEmptyLabel = detailLoading
    ? 'loading captured body…'
    : detailFailed
      ? 'body unavailable — load failed'
      : emptyBodyLabel(seeking);

  // A request change is a new reading context. Reset every detail-owned scroll viewport before
  // paint and focus the summary heading, while the keyed component still preserves the originating
  // table's scroll/focus for Back. This covers direct URLs and async body arrival without landing
  // a new request on the previous request's Headers/Timeline depth.
  useLayoutEffect(() => {
    const root = detailRef.current;
    if (!root) return;
    root.scrollTop = 0;
    root.querySelectorAll<HTMLElement>('[data-detail-scroll], [role="tabpanel"]').forEach((node) => {
      node.scrollTop = 0;
      node.scrollLeft = 0;
    });
    root.querySelector<HTMLElement>('[data-testid="request-summary"] h2, [data-testid="summary-line"] h2')
      ?.focus({ preventScroll: true });
  }, [apiCallId]);

  // Move keyboard focus into the takeover on open. On every unmount path (back button, close,
  // Escape/browser navigation), restore the originating flow trigger once the hidden table becomes
  // visible again. The connected-node guard keeps StrictMode's effect replay from stealing focus.
  useEffect(() => {
    const detailNode = detailRef.current;
    const opener = openerRef.current;
    if (!detailNode?.contains(document.activeElement)) detailNode?.focus({ preventScroll: true });
    return () => {
      requestAnimationFrame(() => {
        if (!detailNode?.isConnected) restoreFlowTrigger(apiCallId, opener);
      });
    };
  }, [apiCallId]);

  // ── Adjustable sections ──────────────────────────────────────────────────────────────────
  // Focus mode (zoom): one layer (A/B/C/deltas) fills the whole main region. NOT persisted — a
  // reopened drill-down always starts un-zoomed (FlowsView keys this component by id, so a row
  // switch also resets it).
  const [zoom, setZoom] = useState<ZoomTarget | null>(null);
  // Collapse-to-strip flags — persisted so the operator's arrangement survives close/reopen.
  const [summaryCollapsed, setSummaryCollapsed] = usePersistedFlag('summary-collapsed', false);
  const [drawerCollapsed, setDrawerCollapsed] = usePersistedFlag('drawer-collapsed', false);
  const [railCollapsed, setRailCollapsed] = usePersistedFlag('rail-collapsed', false);
  useLayoutEffect(() => {
    const key = 'argus-flowdetail-last-request';
    try {
      if (window.localStorage.getItem(key) !== apiCallId) setSummaryCollapsed(false);
      window.localStorage.setItem(key, apiCallId);
    } catch {
      setSummaryCollapsed(false);
    }
  }, [apiCallId, setSummaryCollapsed]);
  const railRef = usePanelRef();
  const drawerRef = usePanelRef();
  // Per-pane + panes-column collapse (NOT persisted flags — the persisted %-layout restores the
  // size, and each panel's mount-time onResize re-syncs the boolean from `isCollapsed()`).
  const paneARef = usePanelRef();
  const paneBRef = usePanelRef();
  const paneCRef = usePanelRef();
  const paneRefs = useMemo(() => ({ A: paneARef, B: paneBRef, C: paneCRef }) as const, [paneARef, paneBRef, paneCRef]);
  const [collapsedPanes, setCollapsedPanes] = useState<Record<'A' | 'B' | 'C', boolean>>({ A: false, B: false, C: false });
  const panesColRef = usePanelRef();
  const [panesColCollapsed, setPanesColCollapsed] = useState(false);
  // Mirrors for the drag-snap sync below (onResize fires per pointer move — only write on change).
  const railCollapsedRef = useRef(railCollapsed);
  railCollapsedRef.current = railCollapsed;
  const drawerCollapsedRef = useRef(drawerCollapsed);
  drawerCollapsedRef.current = drawerCollapsed;

  // Splitter sizes persist via react-resizable-panels' own storage hook (localStorage keys
  // `react-resizable-panels:argus-flowdetail-*`).
  const vsplit = useDefaultLayout({ id: 'argus-flowdetail-vsplit', panelIds: ['detail-main', 'detail-drawer'] });
  const hsplit = useDefaultLayout({ id: 'argus-flowdetail-hsplit', panelIds: ['detail-panes', 'detail-rail'] });
  const abc = useDefaultLayout({ id: 'argus-flowdetail-abc', panelIds: ['pane-a', 'pane-b', 'pane-c'] });

  // Esc PRECEDENCE: the FIRST Esc restores an active zoom; only the SECOND dismisses the
  // drill-down. The dismiss lives in FlowsView's bubble-phase window keydown — this CAPTURE-phase
  // listener runs first and swallows the event ONLY while a zoom is active, so with no zoom the
  // first Esc still dismisses (the e2e `dismissDetail` helper depends on that).
  useEffect(() => {
    if (!zoom) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      e.stopPropagation();
      setZoom(null);
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [zoom]);

  // DevTools console-drawer gesture: clicking the ACTIVE tab toggles the drawer collapsed;
  // clicking an inactive tab switches AND expands. The flag→panel effects below drive the
  // imperative collapse/expand, so a gesture and a splitter drag stay in the same state machine.
  const onTabClick = useCallback(
    (t: Tab) => {
      if (t === tab && !drawerCollapsed) {
        setDrawerCollapsed(true);
        return;
      }
      setTab(t);
      if (drawerCollapsed) setDrawerCollapsed(false);
    },
    [tab, drawerCollapsed, setDrawerCollapsed],
  );

  // Collapsible-panel state sync, both directions:
  //  - DRAG: the library snaps a collapsible panel past its minSize to its collapsedSize;
  //    `onResize` mirrors that into the persisted flag (guarded — it fires per pointer move).
  //  - FLAG (gesture buttons / persisted restore): the effects re-assert the panel's collapse
  //    state from the flag. A restored %-layout may not land exactly on a PIXEL collapsedSize
  //    (viewport changed since save), so this also heals reopen-after-collapse.
  const onRailResize = useCallback(() => {
    const c = railRef.current?.isCollapsed() ?? false;
    if (c !== railCollapsedRef.current) setRailCollapsed(c);
  }, [railRef, setRailCollapsed]);
  const onDrawerResize = useCallback(() => {
    const c = drawerRef.current?.isCollapsed() ?? false;
    if (c !== drawerCollapsedRef.current) setDrawerCollapsed(c);
  }, [drawerRef, setDrawerCollapsed]);
  const onPaneResize = useCallback(
    (key: 'A' | 'B' | 'C') => {
      const c = paneRefs[key].current?.isCollapsed() ?? false;
      setCollapsedPanes((prev) => (prev[key] === c ? prev : { ...prev, [key]: c }));
    },
    [paneRefs],
  );
  const expandPane = useCallback(
    (key: 'A' | 'B' | 'C') => {
      setCollapsedPanes((prev) => ({ ...prev, [key]: false }));
      paneRefs[key].current?.expand();
    },
    [paneRefs],
  );
  const onPanesColResize = useCallback(() => {
    const c = panesColRef.current?.isCollapsed() ?? false;
    setPanesColCollapsed((prev) => (prev === c ? prev : c));
  }, [panesColRef]);
  useEffect(() => {
    const h = railRef.current;
    if (!h) return;
    if (railCollapsed && !h.isCollapsed()) h.collapse();
    else if (!railCollapsed && h.isCollapsed()) h.expand();
  }, [railCollapsed, railRef, zoom]);
  useEffect(() => {
    const h = drawerRef.current;
    if (!h) return;
    if (drawerCollapsed && !h.isCollapsed()) h.collapse();
    else if (!drawerCollapsed && h.isCollapsed()) h.expand();
  }, [drawerCollapsed, drawerRef]);

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
  // de-dups the seam with the explicit MonitorHub watermark captured alongside the REST replay:
  // `FlowDelta.sequence` remains an ordinal, while the live side carries real monitor sequences.
  const liveSegs = useMemo<MonitorSegment[]>(
    () => join.segments.map((segment, i) => ({ segment, monitorSeq: join.segmentSeqs[i] ?? null })),
    [join.segments, join.segmentSeqs],
  );
  const segments = useMemo(
    () => mergeDeltas(
      normalizeRestDeltas(frozenDetail?.deltas),
      liveSegs,
      frozenDetail?.deltas_through_monitor_seq,
    ),
    [frozenDetail?.deltas, frozenDetail?.deltas_through_monitor_seq, liveSegs],
  );

  // Structural diffs between the captured layers (path → kind).
  const diffAB = useMemo(() => diffLayers(detail?.inbound_body, detail?.normalized), [detail?.inbound_body, detail?.normalized]);
  const diffBC = useMemo(() => diffLayers(detail?.normalized, detail?.upstream_body), [detail?.normalized, detail?.upstream_body]);
  const changesAB = useMemo(() => describeChanges(detail?.inbound_body, detail?.normalized), [detail?.inbound_body, detail?.normalized]);
  const changesBC = useMemo(() => describeChanges(detail?.normalized, detail?.upstream_body), [detail?.normalized, detail?.upstream_body]);
  // Pane B sits between both comparisons: it shows what A→B added/changed AND what B→C removes,
  // so it renders the COMBINED middle diff with side `both` (finding 4).
  const diffBMiddle = useMemo(() => combineMiddleDiff(diffAB, diffBC), [diffAB, diffBC]);
  const normalizationHop = useMemo<TransformationHop>(() => ({
    label: 'normalize',
    available: detail?.inbound_body !== undefined && detail?.normalized !== undefined,
    summary: summarizeChanges(changesAB),
  }), [detail?.inbound_body, detail?.normalized, changesAB]);
  const loweringHop = useMemo<TransformationHop>(() => ({
    label: 'lower',
    available: detail?.normalized !== undefined && detail?.upstream_body !== undefined,
    summary: summarizeChanges(changesBC),
  }), [detail?.normalized, detail?.upstream_body, changesBC]);

  const sync = useScrollSync(3);
  const isActive = status === 'open';

  // Cost + its confidence tag are derived TOGETHER as a PAIR from the SAME source (gap 07 review
  // round 3): the displayed dollar value and the `estimated`/`unavailable` provenance tag MUST never
  // come from different rows, or a stale `detail` row with `cost: null, cost_confidence: 'unavailable'`
  // can mask an `estimated` LIVE cost as `—` (dropping the figure AND its est marker).
  //
  // Source precedence MIRRORS `flowCost`'s own roll-up-first precedence so the tag follows the value:
  //   1. server detail roll-up (`detail.cost` finite)  ⇒ pair with `detail.cost_confidence`
  //   2. else the live row's roll-up (`liveFlow.cost` finite) ⇒ pair with `liveFlow.cost_confidence`
  //   3. else usage×price fallback (neither roll-up) ⇒ freshest tag, live-first, mirroring the merged
  //      usage source (`liveFlow.usage ?? detail.usage`), so the tag tracks the usage being priced.
  // Building a merged summary (live status/usage wins; roll-up cost + detail fields fill gaps) lets
  // `flowCost` apply its roll-up-first precedence, so a live row LACKING cost no longer hides
  // `detail.cost`.
  //
  // SEEK coherence (finding 1/3): while seeking, `liveFlow` IS the frozen snapshot row and the live
  // REST detail is withheld (`frozenDetail` is null) — so BOTH cost and its tag derive EXCLUSIVELY
  // from the frozen summary, never the live REST roll-up.
  const { cost, costConfidence } = useMemo<{ cost: number | null; costConfidence: CostConfidence }>(() => {
    const summary = frozenDetail;
    if (!liveFlow && !summary) return { cost: null, costConfidence: 'unavailable' };
    const detailRollup = isFiniteNumber(summary?.cost);
    const liveRollup = isFiniteNumber(liveFlow?.cost);
    // The tag is taken from the EXACT source that supplies the displayed cost value.
    const costConfidence: CostConfidence = detailRollup
      ? summary!.cost_confidence
      : liveRollup
        ? liveFlow!.cost_confidence
        : // usage×price fallback (no roll-up either side): freshest tag, mirroring the merged usage.
          liveFlow?.cost_confidence ?? summary?.cost_confidence ?? 'unavailable';
    const merged: FlowSummary = {
      ...(summary ?? {}),
      ...(liveFlow ?? {}),
      api_call_id: apiCallId,
      revision: liveFlow?.revision ?? summary?.revision ?? 0,
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
      cost_confidence: costConfidence,
    };
    return { cost: flowCost(merged), costConfidence };
  }, [liveFlow, frozenDetail, apiCallId, status]);
  // Gap 07 — the cumulative token usage for the breakdown row (freshest: live, then detail):
  // cached/reasoning may be UNREPORTED (null/absent) ⇒ `fmtTokens` renders `—`, never `0`.
  const usage = liveFlow?.usage ?? frozenDetail?.usage ?? null;
  const normalizedUsage = liveFlow?.normalized_usage ?? frozenDetail?.normalized_usage ?? null;
  const usageAnomalyCount = liveFlow?.usage_anomaly_count ?? frozenDetail?.usage_anomaly_count ?? 0;
  const calculationUsage = normalizedUsage ?? usage;
  // Gap 08 — the token-economics breakdown (cache-hit rate + "$ saved by cache"), MIRRORING the
  // FlowTable tokens-cell popover so the inspector line shows the SAME honest figures. Built from
  // the freshest usage + the served model (for the cached-price PRESENCE gate). `usage`/model are
  // resolved live-first (seek ⇒ frozen-only); the served model gates `$ saved` via the gap-07
  // `cached_price_configured` flag — never the defaulted numeric `0.0`.
  const econ = useMemo(() => {
    const model = liveFlow?.model_served ?? frozenDetail?.model_served ?? liveFlow?.model_requested ?? frozenDetail?.model_requested ?? null;
    const econFlow: FlowSummary = {
      api_call_id: apiCallId,
      revision: liveFlow?.revision ?? frozenDetail?.revision ?? 0,
      method: 'POST',
      uri: '',
      status: status ?? 'open',
      started_ms: liveFlow?.started_ms ?? frozenDetail?.started_ms ?? 0,
      cost_confidence: 'unavailable',
      usage: calculationUsage,
      normalized_usage: calculationUsage,
      cache_price_impact_usd: liveFlow?.cache_price_impact_usd ?? frozenDetail?.cache_price_impact_usd,
      model_served: model,
    };
    return tokenEconomics(econFlow);
  }, [liveFlow, frozenDetail, apiCallId, status, calculationUsage]);
  // Gap 09 — the context-window utilization for the gauge: the freshest usage (live, then detail)
  // against the SERVED model's `context_limit` (gap-06 nullable catalog). `null` limit (unknown
  // capacity) OR unreported usage ⇒ the gauge renders `—`, never a fabricated 0%/100%. The served
  // model (then requested) is the one actually run, so its window is the relevant ceiling.
  const contextUtil = useMemo<ContextUtilization>(() => {
    const persistedLimit = liveFlow?.effective_route_limit ?? frozenDetail?.effective_route_limit;
    const limit = status !== 'open'
      ? persistedLimit ?? null
      : persistedLimit ?? contextLimitFor(
          liveFlow?.model_served ?? frozenDetail?.model_served,
          liveFlow?.model_requested ?? frozenDetail?.model_requested,
          contextLimits,
        );
    return contextUtilization(calculationUsage, limit);
  }, [liveFlow, frozenDetail, calculationUsage, contextLimits, status]);
  // Gap 10 — the per-flow LATENCY BREAKDOWN (the phase waterfall + the Timing line). The PRIMARY
  // source is the gap-02 phase epochs + gap-03 served-attempt wire TTFB, read live-first
  // (`liveFlow`) then from the frozen detail (seek). When `first_content_delta_ms` is absent the
  // model falls back to the monitor `output` segments for a DERIVED first-visible-activity TTFT
  // (explicitly labelled). Both the spine fields AND the monitor join are already cut-bounded
  // upstream (seek coherence holds): `liveFlow` is the frozen row while seeking, and `join` is the
  // cut-bounded monitor join (finding 1). The merged spine prefers the live row's freshest phase
  // values, filling any gap from the frozen detail.
  const latency = useMemo<LatencyBreakdownModel>(() => {
    // Merge the freshest spine: live row first (its phase/attempt fields win), frozen detail fills
    // gaps. Both carry the same flattened `PhaseTimings` + `attempts`/`first_upstream_byte_ms`.
    const spine: SpineFlow | null = liveFlow || frozenDetail
      ? {
          started_ms: liveFlow?.started_ms ?? frozenDetail?.started_ms ?? 0,
          ingress_ms: liveFlow?.ingress_ms ?? frozenDetail?.ingress_ms,
          normalization_done_ms: liveFlow?.normalization_done_ms ?? frozenDetail?.normalization_done_ms,
          routing_decision_ms: liveFlow?.routing_decision_ms ?? frozenDetail?.routing_decision_ms,
          first_content_delta_ms: liveFlow?.first_content_delta_ms ?? frozenDetail?.first_content_delta_ms,
          stream_end_ms: liveFlow?.stream_end_ms ?? frozenDetail?.stream_end_ms,
          finalize_ms: liveFlow?.finalize_ms ?? frozenDetail?.finalize_ms,
          finished_ms: liveFlow?.finished_ms ?? frozenDetail?.finished_ms,
          elapsed_ms: liveFlow?.elapsed_ms ?? frozenDetail?.elapsed_ms,
          // `attempts` is an ARRAY — it must NOT use the scalar `live ?? rest` rule (gap 10b review
          // round 2). A live/snapshot row can carry `attempts: []` ("no attempt recorded yet"), and
          // `??` would treat that `[]` as authoritative, SUPPRESSING the populated REST detail trace
          // so the latency model's wire TTFB (served attempt) goes `unavailable`. `pickAttempts` lets
          // a NON-EMPTY list (either side) win and treats `[]` as absent for backfill.
          attempts: pickAttempts(liveFlow?.attempts, frozenDetail?.attempts),
          first_upstream_byte_ms: liveFlow?.first_upstream_byte_ms ?? frozenDetail?.first_upstream_byte_ms,
          usage,
        }
      : null;
    // The derived-TTFT fallback source: the cut-bounded monitor `output` segments for this flow.
    const outputs = join.segments.filter((s) => s.kind === 'output');
    return latencyBreakdown(spine, outputs);
  }, [liveFlow, frozenDetail, usage, join.segments]);

  // Gap 11 — the per-flow FAILOVER / attempt trace (the inspector-header stepper). Consumes the
  // gap-03 `attempts[]` projected onto the row + detail + live frame (gap 10b). Merged with the SAME
  // `pickAttempts` rule the latency model uses (a live/snapshot `attempts: []` is "no attempt yet"
  // and must NOT suppress a populated REST detail trace; a non-empty list — either side — wins).
  // A single attempt ⇒ a single node (no fake failover); ≥2 ⇒ the chain; absent ⇒ no stepper.
  const attempts = useMemo<AttemptTraceModel>(
    () => attemptTrace(pickAttempts(liveFlow?.attempts, frozenDetail?.attempts)),
    [liveFlow?.attempts, frozenDetail?.attempts],
  );

  // One source of truth for the three layers, so the zoomed render and the 3-pane row feed the
  // SAME props into JsonPane (search + operation explanations keep applying in focus mode), and the
  // scroll-sync ref indices stay stable whether or not the siblings are mounted.
  const panes = [
    {
      key: 'A' as const,
      label: 'A · inbound',
      stage: { step: 'A' as const, title: 'Client payload', subtitle: 'captured at ingress', nextLabel: 'canonical' },
      value: detail?.inbound_body,
      diff: diffAB,
      incomingChanges: undefined,
      outgoingChanges: changesAB,
      side: 'left' as const,
      index: 0,
    },
    {
      key: 'B' as const,
      label: 'B · normalized',
      stage: { step: 'B' as const, title: 'Gateway canonical', subtitle: 'after adapter + policies', nextLabel: 'upstream' },
      value: detail?.normalized,
      diff: diffBMiddle,
      incomingChanges: changesAB,
      outgoingChanges: changesBC,
      side: 'both' as const,
      index: 1,
    },
    {
      key: 'C' as const,
      label: 'C · upstream',
      stage: { step: 'C' as const, title: 'Provider payload', subtitle: 'captured at dispatch' },
      value: detail?.upstream_body,
      diff: diffBC,
      incomingChanges: changesBC,
      outgoingChanges: undefined,
      side: 'right' as const,
      index: 2,
    },
  ];
  const zoomedPane = zoom && zoom !== 'deltas' ? panes.find((p) => p.key === zoom) ?? null : null;
  const narrowPane = narrowTab === 'A' || narrowTab === 'B' || narrowTab === 'C'
    ? panes.find((pane) => pane.key === narrowTab) ?? null
    : null;
  const transformationBar = (
    <RequestTransformationBar
      query={query}
      onQueryChange={setQuery}
      mode={requestView}
      onModeChange={setRequestView}
      normalization={normalizationHop}
      lowering={loweringHop}
    />
  );

  return (
    <section
      ref={detailRef}
      tabIndex={-1}
      className="flex min-h-0 min-w-0 flex-1 flex-col bg-panel outline-none"
      data-testid="flow-detail"
      aria-label="flow detail"
    >
      <TopBar
        apiCallId={apiCallId}
        flow={liveFlow}
        detail={frozenDetail}
        seeking={seeking}
        isActive={isActive}
        mutationsEnabled={mutationsEnabled}
        killState={killState}
        onKill={() => kill(apiCallId)}
        onClose={onClose}
      />
      {detailLoading && (
        <div className="shrink-0 border-b border-line bg-panel px-3 py-2 text-xs text-text-muted" role="status" data-testid="detail-loading">
          Loading captured flow detail…
        </div>
      )}
      {detailQuery.isError && (
        <div className="flex shrink-0 items-center gap-3 border-b border-status-down/40 bg-status-down/10 px-3 py-2 text-xs" role="alert" data-testid="detail-load-error">
          <span>{detail ? 'Flow detail could not refresh. Showing the last captured version.' : 'Flow detail could not be loaded. Captured bodies are unavailable.'}</span>
          <button type="button" className="ml-auto text-accent underline" onClick={() => { void detailQuery.refetch(); }}>Retry</button>
        </div>
      )}
      {narrow ? (
        <div className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="flow-detail-narrow">
          <SummaryBand
            flow={liveFlow}
            detail={frozenDetail}
            cost={cost}
            costConfidence={costConfidence}
            usage={usage}
            normalizedUsage={normalizedUsage}
            usageAnomalyCount={usageAnomalyCount}
            econ={econ}
            contextUtil={contextUtil}
            latency={latency}
            attempts={attempts}
            seeking={seeking}
            seekAtMs={seekAtMs}
            collapsed={summaryCollapsed}
            onToggle={() => setSummaryCollapsed(!summaryCollapsed)}
          />
          <NarrowTabStrip active={narrowTab} onChange={setNarrowTab} />
          <div
            id={narrowPanelId(narrowTab)}
            role="tabpanel"
            aria-labelledby={narrowTabId(narrowTab)}
            tabIndex={0}
            className="flex min-h-0 min-w-0 flex-1 flex-col overflow-auto focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent"
            data-detail-scroll
            data-testid={`narrow-tabpanel-${narrowTab}`}
          >
            {narrowPane ? (
              <>
                {transformationBar}
                <JsonPane
                  label={narrowPane.label}
                  stage={narrowPane.stage}
                  value={narrowPane.value}
                  diff={narrowPane.diff}
                  incomingChanges={narrowPane.incomingChanges}
                  outgoingChanges={narrowPane.outgoingChanges}
                  side={narrowPane.side}
                  query={query}
                  changesOnly={requestView === 'changes'}
                  emptyLabel={bodyEmptyLabel}
                  scrollRef={sync.refFor(narrowPane.index)}
                  onScroll={sync.bind(narrowPane.index)}
                  className="min-h-0 flex-1"
                />
              </>
            ) : narrowTab === 'deltas' ? (
              <DeltasRail segments={segments} />
            ) : narrowTab === 'headers' ? (
              <HeadersTab headers={frozenDetail?.inbound_headers} />
            ) : narrowTab === 'captures' ? (
              <CapturedSectionsTab detail={frozenDetail} />
            ) : narrowTab === 'timeline' ? (
              <Timeline events={join.events} startedAtMs={liveFlow?.started_ms ?? frozenDetail?.started_ms} />
            ) : (
              <ErrorTab detail={frozenDetail} liveFlow={liveFlow} joinError={join.error} seeking={seeking} />
            )}
          </div>
        </div>
      ) : (
      /* Main region over the bottom tab drawer — a vertical splitter; the drawer collapses to
         the bare tab strip (rendered below the group), never hides entirely. */
      <Group
        orientation="vertical"
        id="flowdetail-vsplit"
        className="min-h-0 min-w-0 flex-1"
        defaultLayout={vsplit.defaultLayout}
        onLayoutChanged={vsplit.onLayoutChanged}
      >
        {/* Every panel below is FULL-RANGE collapsible: drag a splitter to the extreme and the
            panel in the way snaps out of view (collapsedSize 0); drag back and it returns. The
            summary band lives INSIDE the main panel, so a drawer dragged to the top swallows it
            too — the drawer reaches the drill-down's top bar. */}
        {/* U3: a %-only minimum let a persisted drawer layout crush the summary band (and the
            waterfall inside it) to a sliver. The pixel floor must hold the summary header +
            metric grid (~180px) PLUS the technical region's own 160px minimum (R2 — 240px could
            not, so a dragged minimum still clipped the waterfall behind overflow:hidden);
            dragging past the floor still snaps to the full collapse. */}
        <Panel
          id="detail-main"
          collapsible
          collapsedSize={0}
          minSize={384}
          className="flex min-h-0 min-w-0 flex-col"
          style={{ overflow: 'hidden' }}
        >
          <SummaryBand
            flow={liveFlow}
            detail={frozenDetail}
            cost={cost}
            costConfidence={costConfidence}
            usage={usage}
            normalizedUsage={normalizedUsage}
            usageAnomalyCount={usageAnomalyCount}
            econ={econ}
            contextUtil={contextUtil}
            latency={latency}
            attempts={attempts}
            seeking={seeking}
            seekAtMs={seekAtMs}
            collapsed={summaryCollapsed}
            onToggle={() => setSummaryCollapsed(!summaryCollapsed)}
          />
          {zoom ? (
            /* FOCUS MODE — the zoomed layer fills the whole main region; the others unmount
               (useScrollSync tolerates unmounted sibling refs). Esc or ⤢ restores. */
            <div className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="zoom-region" data-zoom={zoom}>
              {zoomedPane ? (
                <>
                  {transformationBar}
                  <JsonPane
                    label={zoomedPane.label}
                    stage={zoomedPane.stage}
                    value={zoomedPane.value}
                    diff={zoomedPane.diff}
                    incomingChanges={zoomedPane.incomingChanges}
                    outgoingChanges={zoomedPane.outgoingChanges}
                    side={zoomedPane.side}
                    query={query}
                    changesOnly={requestView === 'changes'}
                    emptyLabel={bodyEmptyLabel}
                    scrollRef={sync.refFor(zoomedPane.index)}
                    onScroll={sync.bind(zoomedPane.index)}
                    onZoom={() => setZoom(null)}
                    zoomed
                    className="min-h-0 flex-1"
                  />
                </>
              ) : (
                <DeltasRail segments={segments} zoomed onZoom={() => setZoom(null)} />
              )}
            </div>
          ) : (
            /* Panes column | deltas rail — a horizontal splitter; the rail collapses to a thin
               edge strip (24px), never hides entirely. */
            <Group
              orientation="horizontal"
              id="flowdetail-hsplit"
              className="min-h-0 min-w-0 flex-1"
              defaultLayout={hsplit.defaultLayout}
              onLayoutChanged={hsplit.onLayoutChanged}
            >
              <Panel
                id="detail-panes"
                collapsible
                collapsedSize={16}
                minSize="20%"
                panelRef={panesColRef}
                onResize={onPanesColResize}
                className="flex min-h-0 min-w-0 flex-col"
                style={{ overflow: 'hidden' }}
              >
                {panesColCollapsed ? (
                  <EdgeStrip label="layers" onExpand={() => { setPanesColCollapsed(false); panesColRef.current?.expand(); }} testid="panes-strip" />
                ) : (
                  <>
                    {transformationBar}
                    {/* 3 scroll-synced panes with their own splitters (widen one layer as needed).
                        Each pane collapses to a 16px labeled sliver, NOT 0 — a zero-width middle
                        pane stacks its two separators on the same pixel, which makes the
                        drag-to-re-expand grab the wrong one. The sliver keeps them apart and is
                        itself the click-to-restore affordance. */}
                    <Group
                      orientation="horizontal"
                      id="pane-row"
                      className="min-h-0 min-w-0 flex-1"
                      defaultLayout={abc.defaultLayout}
                      onLayoutChanged={abc.onLayoutChanged}
                    >
                      {panes.map((p, i) => (
                        <Fragment key={p.key}>
                          {i > 0 && (
                            <Separator
                              id={`split-${panes[i - 1]!.key.toLowerCase()}${p.key.toLowerCase()}`}
                              className={SPLIT_V}
                            />
                          )}
                          <Panel
                            id={`pane-${p.key.toLowerCase()}`}
                            collapsible
                            collapsedSize={16}
                            minSize="10%"
                            panelRef={paneRefs[p.key]}
                            onResize={() => onPaneResize(p.key)}
                            className="flex min-h-0 min-w-0 flex-col"
                            style={{ overflow: 'hidden' }}
                          >
                            {collapsedPanes[p.key] ? (
                              <EdgeStrip label={p.key} onExpand={() => expandPane(p.key)} testid={`pane-strip-${p.key.toLowerCase()}`} />
                            ) : (
                              <JsonPane
                                label={p.label}
                                stage={p.stage}
                                value={p.value}
                                diff={p.diff}
                                incomingChanges={p.incomingChanges}
                                outgoingChanges={p.outgoingChanges}
                                side={p.side}
                                query={query}
                                changesOnly={requestView === 'changes'}
                                emptyLabel={bodyEmptyLabel}
                                scrollRef={sync.refFor(p.index)}
                                onScroll={sync.bind(p.index)}
                                onZoom={() => setZoom(p.key)}
                                className="min-h-0 flex-1"
                              />
                            )}
                          </Panel>
                        </Fragment>
                      ))}
                    </Group>
                  </>
                )}
              </Panel>
              <Separator id="split-rail" className={SPLIT_V} />
              <Panel
                id="detail-rail"
                collapsible
                collapsedSize={24}
                minSize="12%"
                defaultSize="22%"
                panelRef={railRef}
                onResize={onRailResize}
                className="flex min-h-0 min-w-0 flex-col"
                style={{ overflow: 'hidden' }}
              >
                {railCollapsed ? (
                  <EdgeStrip label="deltas" onExpand={() => setRailCollapsed(false)} testid="deltas-strip" className="border-l border-line" />
                ) : (
                  <DeltasRail segments={segments} onZoom={() => setZoom('deltas')} onCollapse={() => setRailCollapsed(true)} />
                )}
              </Panel>
            </Group>
          )}
        </Panel>

        <Separator id="split-drawer" className={SPLIT_H} />
        {/* The drawer Panel is ALWAYS mounted with the tab strip at its top: collapsed means
            "exactly the 34px strip" (pixel collapsedSize), so the strip is never hidden, a drag
            below minSize snaps to it, and dragging the splitter back up re-opens it. Dragging it
            ALL THE WAY UP instead collapses `detail-main` out of view — the drawer fills the
            inspector. The tabpanel content unmounts while collapsed. */}
        <Panel
          id="detail-drawer"
          collapsible
          collapsedSize={34}
          defaultSize="25%"
          minSize="12%"
          panelRef={drawerRef}
          onResize={onDrawerResize}
          className="flex min-h-0 min-w-0 flex-col"
          style={{ overflow: 'hidden' }}
        >
          <TabStrip tab={tab} collapsed={drawerCollapsed} onTabClick={onTabClick} />
          {!drawerCollapsed && (
            <div
              id={drawerPanelId(tab)}
              className="min-h-0 flex-1 overflow-auto focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent"
              role="tabpanel"
              aria-labelledby={drawerTabId(tab)}
              tabIndex={0}
              data-testid={`tabpanel-${tab}`}
            >
              {/* Headers + Error read the FROZEN detail (null while seeking) so no live/post-cut
                  metadata leaks; Timeline reads the cut-bounded monitor join (finding 1). */}
              {tab === 'headers' && <HeadersTab headers={frozenDetail?.inbound_headers} />}
              {tab === 'captures' && <CapturedSectionsTab detail={frozenDetail} />}
              {tab === 'timeline' && <Timeline events={join.events} startedAtMs={liveFlow?.started_ms ?? frozenDetail?.started_ms} />}
              {tab === 'error' && <ErrorTab detail={frozenDetail} liveFlow={liveFlow} joinError={join.error} seeking={seeking} />}
            </div>
          )}
        </Panel>
      </Group>
      )}
    </section>
  );
}

/** The Headers/Timeline/Error tab strip. Rendered inside the drawer panel when expanded, or as
 * the bare strip below the split group when the drawer is collapsed (strip-only, never hidden). */
function TabStrip({ tab, collapsed, onTabClick }: { tab: Tab; collapsed: boolean; onTabClick: (t: Tab) => void }) {
  const refs = useRef<Array<HTMLButtonElement | null>>([]);
  const activate = (index: number) => {
    const normalized = (index + DRAWER_TABS.length) % DRAWER_TABS.length;
    const next = DRAWER_TABS[normalized];
    if (!next) return;
    onTabClick(next.id);
    refs.current[normalized]?.focus();
  };

  return (
    <div
      // Fixed 34px (border-box) — MUST match the drawer Panel's `collapsedSize={34}`, so the
      // collapsed drawer shows exactly the strip (no clipped strip / no content sliver).
      className="flex h-[34px] shrink-0 items-center gap-1 border-y border-line bg-panel-raised px-2"
      role="tablist"
      aria-label="Flow detail metadata"
      data-testid="detail-tabstrip"
      data-collapsed={collapsed ? 'true' : 'false'}
    >
      {DRAWER_TABS.map((item, index) => (
        <TabButton
          key={item.id}
          buttonRef={(node) => { refs.current[index] = node; }}
          id={item.id}
          active={tab}
          onClick={onTabClick}
          onKeyDown={(event) => {
            let next: number | null = null;
            if (event.key === 'ArrowRight' || event.key === 'ArrowDown') next = index + 1;
            else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') next = index - 1;
            else if (event.key === 'Home') next = 0;
            else if (event.key === 'End') next = DRAWER_TABS.length - 1;
            if (next === null) return;
            event.preventDefault();
            activate(next);
          }}
        >
          {item.label}
        </TabButton>
      ))}
      <span className="ml-auto hidden text-[9px] uppercase tracking-wide text-text-muted sm:inline">
        {collapsed ? 'click a tab to expand' : 'click the active tab to collapse'}
      </span>
    </div>
  );
}

function drawerTabId(tab: Tab): string {
  return `flow-detail-drawer-tab-${tab}`;
}

function drawerPanelId(tab: Tab): string {
  return `flow-detail-drawer-panel-${tab}`;
}

function narrowTabId(tab: NarrowTab): string {
  return `flow-detail-tab-${tab.toLowerCase()}`;
}

function narrowPanelId(tab: NarrowTab): string {
  return `flow-detail-panel-${tab.toLowerCase()}`;
}

/** Seven-surface, automatic-activation tablist used by the narrow single-pane inspector. */
function NarrowTabStrip({
  active,
  onChange,
}: {
  active: NarrowTab;
  onChange: (tab: NarrowTab) => void;
}) {
  const refs = useRef<Array<HTMLButtonElement | null>>([]);
  const activate = (index: number) => {
    const normalized = (index + NARROW_TABS.length) % NARROW_TABS.length;
    const next = NARROW_TABS[normalized];
    if (!next) return;
    onChange(next.id);
    refs.current[normalized]?.focus();
  };

  return (
    <div
      role="tablist"
      aria-label="Flow detail sections"
      className="flex shrink-0 gap-1 overflow-x-auto border-y border-line bg-panel-raised px-2 py-1.5"
      data-testid="narrow-detail-tablist"
    >
      {NARROW_TABS.map((item, index) => {
        const selected = item.id === active;
        return (
          <button
            key={item.id}
            ref={(node) => { refs.current[index] = node; }}
            id={narrowTabId(item.id)}
            type="button"
            role="tab"
            aria-selected={selected}
            aria-controls={narrowPanelId(item.id)}
            tabIndex={selected ? 0 : -1}
            onClick={() => onChange(item.id)}
            onKeyDown={(event) => {
              let next: number | null = null;
              if (event.key === 'ArrowRight' || event.key === 'ArrowDown') next = index + 1;
              else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') next = index - 1;
              else if (event.key === 'Home') next = 0;
              else if (event.key === 'End') next = NARROW_TABS.length - 1;
              if (next === null) return;
              event.preventDefault();
              activate(next);
            }}
            className={cn(
              'shrink-0 rounded-md px-2.5 py-1 text-xs transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent',
              selected
                ? 'bg-accent/20 text-text ring-1 ring-inset ring-accent/60'
                : 'text-text hover:bg-panel hover:text-text',
            )}
          >
            {item.label}
          </button>
        );
      })}
    </div>
  );
}

/** The deltas rail: header (zoom / collapse controls) + the live segment stream. Fills the rail
 * panel, or the whole main region when zoomed (focus mode). */
function DeltasRail({
  segments,
  zoomed = false,
  onZoom,
  onCollapse,
}: {
  segments: DebugSegment[];
  zoomed?: boolean;
  onZoom?: () => void;
  onCollapse?: () => void;
}) {
  return (
    <div className={cn('flex min-h-0 min-w-0 flex-1 flex-col', !zoomed && 'border-l border-line')} data-testid="deltas-rail">
      <div
        className="flex shrink-0 items-center justify-between border-b border-line bg-panel-raised px-3 py-1"
        // Double-clicking the header surface zooms — but not double-clicks landing on the
        // zoom/collapse buttons, whose single-click actions must not also toggle zoom.
        onDoubleClick={onZoom
          ? (e) => {
              if ((e.target as HTMLElement).closest('button')) return;
              onZoom();
            }
          : undefined}
      >
        <span className="text-[10px] uppercase tracking-wide text-text-muted">deltas</span>
        <span className="flex items-center gap-1">
          {onZoom && (
            <button
              type="button"
              onClick={onZoom}
              aria-label={zoomed ? 'restore deltas rail' : 'zoom deltas rail'}
              title={zoomed ? 'restore (Esc)' : 'zoom to fill the inspector'}
              className="rounded-sm px-1 text-[11px] leading-none text-text-muted transition-colors hover:text-accent"
              data-testid="deltas-zoom"
            >
              ⤢
            </button>
          )}
          {onCollapse && (
            <button
              type="button"
              onClick={onCollapse}
              aria-label="collapse deltas rail"
              title="collapse to edge strip"
              className="rounded-sm px-1 text-[11px] leading-none text-text-muted transition-colors hover:text-accent"
              data-testid="deltas-collapse-btn"
            >
              ⇥
            </button>
          )}
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto" tabIndex={0} role="region" aria-label="Streamed response deltas">
        <DeltasPanel segments={segments} />
      </div>
    </div>
  );
}

/** When seeking a historical cut, a missing body is explicitly evicted (D5 tradeoff). */
function emptyBodyLabel(seeking: boolean): string {
  return seeking ? 'body evicted (snapshot)' : 'body evicted';
}

/** A roll-up cost is "present" (drives the displayed value) only when it is a finite number —
 * mirrors `flowCost`'s `typeof === 'number' && Number.isFinite` guard, so the paired confidence
 * tag is chosen from the SAME source `flowCost` will actually read the cost from. */
function isFiniteNumber(v: number | null | undefined): v is number {
  return typeof v === 'number' && Number.isFinite(v);
}

/** The drill-down top bar: back-to-table, identity + status, seek badge, kill + close. */
function TopBar({
  apiCallId,
  flow,
  detail,
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
  seeking: boolean;
  isActive: boolean;
  mutationsEnabled: boolean;
  killState: KillState;
  onKill: () => void;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const status = flow?.status ?? detail?.status ?? 'open';
  // The request line lives on the row (`FlowSummary`) only — `/flows/:id` does not carry it.
  const method = flow?.method ?? '';
  const uri = flow?.uri ?? '';
  const copyId = async () => {
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(apiCallId);
      } else {
        const input = document.createElement('textarea');
        input.value = apiCallId;
        input.setAttribute('readonly', '');
        input.style.position = 'fixed';
        input.style.opacity = '0';
        document.body.append(input);
        input.select();
        const copied = document.execCommand('copy');
        input.remove();
        if (!copied) throw new Error('copy command rejected');
      }
      setCopied(true);
      globalThis.setTimeout(() => setCopied(false), 1_500);
    } catch {
      setCopied(false);
    }
  };
  return (
    <header className="flex min-w-0 shrink-0 items-center gap-2 border-b border-line bg-panel-raised px-2 py-2 sm:gap-3 sm:px-3">
      <button
        type="button"
        onClick={onClose}
        aria-label="back to flows"
        data-testid="detail-back"
        className="flex shrink-0 items-center gap-1.5 rounded-md border border-line px-2 py-1 text-xs text-text-muted transition-colors hover:border-accent/50 hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
      >
        <span aria-hidden="true">←</span> flows
      </button>
      <StatusChip status={status} terminalReason={flow?.terminal_reason ?? detail?.terminal_reason} />
      {(method || uri) && (
        <span className="hidden min-w-0 truncate text-xs font-medium text-text md:inline" title={`${method} ${uri}`}>
          {method} {uri}
        </span>
      )}
      <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-text-muted" title={apiCallId}>{apiCallId}</span>
      <button
        type="button"
        onClick={() => void copyId()}
        aria-label="Copy full request ID"
        title="Copy full request ID"
        className="min-h-8 shrink-0 rounded border border-line px-2 text-[11px] text-text-muted hover:border-accent/50 hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
        data-testid="copy-request-id"
      >
        {copied ? 'Copied' : 'Copy ID'}
      </button>
      {seeking && (
        <span className="rounded-sm bg-status-cooling/15 px-1.5 py-0.5 text-[10px] uppercase text-status-cooling" data-testid="seek-badge">
          snapshot
        </span>
      )}
      <span className="ml-auto hidden text-[10px] uppercase tracking-wide text-text-muted lg:inline">esc to dismiss</span>
      <div className="flex shrink-0 items-center gap-1 sm:gap-2">
        <KillControl isActive={isActive} mutationsEnabled={mutationsEnabled} seeking={seeking} killState={killState} onKill={onKill} />
        <button
          type="button"
          onClick={onClose}
          aria-label="close detail"
          className="rounded-md border border-transparent px-2 py-1 text-sm text-text-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
        >
          ✕
        </button>
      </div>
    </header>
  );
}

/**
 * The summary band under the top bar — the identity/cost/token facts (left), the context gauge +
 * latency waterfall (middle, the widest column), and the failover trace (right, only when a trace
 * was recorded). Every figure keeps its don't-lie-with-zeros contract from the old header: an
 * unreported class renders `—`, never a fabricated `0`.
 */
function SummaryBand({
  flow,
  detail,
  cost,
  costConfidence,
  usage,
  normalizedUsage,
  usageAnomalyCount,
  econ,
  contextUtil,
  latency,
  attempts,
  seeking,
  seekAtMs,
  collapsed,
  onToggle,
}: {
  flow: FlowSummary | null;
  detail: FlowDetailDto | null;
  cost: number | null;
  costConfidence: CostConfidence;
  usage: Usage | null;
  normalizedUsage: Usage | null;
  usageAnomalyCount: number;
  econ: ReturnType<typeof tokenEconomics>;
  contextUtil: ContextUtilization;
  latency: LatencyBreakdownModel;
  attempts: AttemptTraceModel;
  seeking: boolean;
  seekAtMs: number | null;
  collapsed: boolean;
  onToggle: () => void;
}) {
  const costView = costDisplay(cost, costConfidence);
  const modelReq = flow?.model_requested ?? detail?.model_requested;
  const modelServed = flow?.model_served ?? detail?.model_served;
  const upstream = flow?.upstream_target ?? detail?.upstream_target ?? null;
  const status = flow?.status ?? detail?.status ?? 'open';
  const elapsed = flow
    ? elapsedMs(flow, seeking ? seekAtMs ?? flow.started_ms : Date.now())
    : (seeking ? null : detail?.elapsed_ms ?? null);
  const totalMs = latency.total.valueMs ?? elapsed;
  const route = formatModelRoute(modelReq, modelServed);
  const failover = failoverSummary(attempts);

  return (
    <div className="shrink-0 border-b border-line bg-panel-raised/60" data-testid={collapsed ? 'summary-line' : 'request-summary'}>
      <div className="flex items-center justify-between gap-3 px-3 pt-2">
        <h2 tabIndex={-1} className="text-xs font-semibold text-text outline-none">Request summary</h2>
        <button
          type="button"
          onClick={onToggle}
          aria-expanded={!collapsed}
          aria-controls="request-technical-details"
          aria-label={collapsed ? 'Show timing and routing details' : 'Hide timing and routing details'}
          data-testid="summary-toggle"
          className="min-h-8 rounded px-2 text-[11px] text-text-muted transition-colors hover:bg-panel hover:text-text"
        >
          {collapsed ? 'Show technical details ▸' : 'Hide technical details ▾'}
        </button>
      </div>

      <div className="grid min-w-0 grid-cols-2 gap-px px-3 pb-3 sm:grid-cols-4 lg:grid-cols-6 xl:grid-cols-9" data-testid="request-summary-grid">
        <SummaryMetric label="Outcome" primary>
          <StatusChip status={status} terminalReason={flow?.terminal_reason ?? detail?.terminal_reason} />
        </SummaryMetric>
        <SummaryMetric label="Requested → served" className="col-span-2" primary title={route}>
          <span className="break-words font-mono">{route}</span>
        </SummaryMetric>
        <SummaryMetric label="Provider" primary title={upstream ?? undefined}>
          <span className="break-words font-mono">{upstream ?? 'Unavailable'}</span>
        </SummaryMetric>
        <SummaryMetric label="Total time" primary quality={totalMs === null ? 'unavailable' : 'measured'}>
          {summaryElapsed(totalMs, 'Unavailable')}
        </SummaryMetric>
        <SummaryMetric label="First token" primary quality={latency.ttft.quality} title={latency.ttft.detail}>
          {summaryElapsed(latency.ttft.valueMs, 'Not captured')}
        </SummaryMetric>
        <SummaryMetric label="Input / output" primary quality={usage ? 'measured' : 'unavailable'}>
          {summaryTokens(usage?.prompt)} / {summaryTokens(usage?.completion)}
        </SummaryMetric>
        <SummaryMetric label="Cost" primary quality={costConfidence === 'unavailable' ? 'unavailable' : costConfidence === 'estimated' ? 'estimated' : 'derived'}>
          <CostCell costView={costView} unavailableText="Unavailable" />
        </SummaryMetric>
        <SummaryMetric
          label="Failover"
          className={cn('col-span-2 sm:col-span-1', attempts.isFailover && 'border-status-down/40')}
          primary
          quality={attempts.hasTrace ? 'measured' : 'unavailable'}
        >
          <span className={attempts.isFailover ? 'text-status-cooling' : undefined}>{failover}</span>
        </SummaryMetric>
      </div>

      {!collapsed && (
        // U3: the latency waterfall + attempt trace are the most diagnostic content in the
        // detail view — a hard 8rem cap rendered them clipped mid-label on every open. Give
        // the region real room (bounded by viewport share so the panes keep space) and keep
        // overflow-auto as the safety valve on short viewports.
        <div id="request-technical-details" className="grid max-h-[38vh] min-h-40 gap-4 overflow-auto border-t border-line/60 px-3 py-3 xl:grid-cols-[minmax(24rem,2fr)_minmax(18rem,1fr)]" data-testid="request-technical-details" tabIndex={0} role="region" aria-label="Request timing, routing, and usage details">
          <section className="min-w-0" aria-labelledby="request-timing-title">
            <h3 id="request-timing-title" className="mb-2 text-xs font-medium text-text-muted">Timing and context</h3>
            <div className="mb-3"><LatencyBreakdown model={latency} /></div>
            <ContextGauge util={contextUtil} />
          </section>
          <section className="min-w-0" aria-labelledby="request-routing-title">
            <h3 id="request-routing-title" className="mb-2 text-xs font-medium text-text-muted">Routing, failover, and usage</h3>
            {attempts.hasTrace ? <AttemptTrace model={attempts} /> : <p className="mb-3 text-xs text-text-muted">Attempt trace was not captured.</p>}
            <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
              <dt className="text-text-muted">cached / reasoning</dt>
              <dd className="tabular-nums text-text" data-testid="usage-subcounts">
                <span title="cache-read prompt tokens; Not captured means the upstream did not report this class">{summaryTokens(usage?.cached)}</span>
                <span className="text-line"> / </span>
                <span title="reasoning tokens; Not captured means the upstream did not report this class">{summaryTokens(usage?.reasoning)}</span>
              </dd>
              <dt className="text-text-muted">cache hit / saved</dt>
              <dd className="tabular-nums text-text" data-testid="cache-economics">
                <span data-testid="cache-hit" data-quality={econ.cacheHit.quality} title="cache-hit rate cached/prompt (derived; — = cached unreported)">{econ.cacheHit.value}</span>
                <span className="text-line"> · </span>
                <span data-testid="cache-saved" data-quality={econ.saved.quality} title={econ.cachedPriceConfigured ? '$ saved by serving cached tokens at the cached rate (derived)' : '$ saved unavailable — no configured cached price for this model'}>{econ.saved.value}</span>
                {econ.saved.quality === 'derived' && (
                  <span className="ml-1.5 rounded-sm bg-accent/15 px-1 py-0.5 text-[10px] text-accent" data-testid="saved-derived" title="cache saving is a derived figure (input rate − cached rate)">derived</span>
                )}
              </dd>
              {usageAnomalyCount > 0 && (
                <>
                  <dt className="text-status-cooling">usage quality</dt>
                  <dd className="text-status-cooling" data-testid="usage-anomalies" data-quality="partial" title={normalizedUsage ? `calculation copy: ${JSON.stringify(normalizedUsage)}` : undefined}>
                    Partial · {usageAnomalyCount} normalized anomaly {usageAnomalyCount === 1 ? 'class' : 'classes'}
                  </dd>
                </>
              )}
            </dl>
          </section>
        </div>
      )}
    </div>
  );
}

function SummaryMetric({
  label,
  children,
  primary = false,
  className,
  quality,
  title,
}: {
  label: string;
  children: React.ReactNode;
  primary?: boolean;
  className?: string;
  quality?: string;
  title?: string;
}) {
  return (
    <div className={cn('min-w-0 border-l border-line/60 px-2 py-2 first:border-l-0', className)} data-quality={quality} title={title}>
      <div className="mb-1 text-[11px] text-text-muted">{label}</div>
      <div className={cn('min-w-0 tabular-nums text-text', primary ? 'text-sm font-semibold' : 'text-xs')}>{children}</div>
    </div>
  );
}

function summaryElapsed(value: number | null | undefined, missing: string): string {
  return value === null || value === undefined ? missing : fmtElapsed(value);
}

function summaryTokens(value: number | null | undefined): string {
  return value === null || value === undefined ? 'Not captured' : fmtTokens(value);
}

function formatModelRoute(requested: string | null | undefined, served: string | null | undefined): string {
  const req = requested?.trim();
  const actual = served?.trim();
  if (req && actual) return req === actual ? actual : `${req} → ${actual}`;
  return actual || req || 'Unavailable';
}

function failoverSummary(model: AttemptTraceModel): string {
  if (!model.hasTrace) return 'Not captured';
  if (!model.isFailover) return 'No failover';
  if (model.servedIndex === null) return `Failed · ${model.failedCount} attempts`;
  const served = model.nodes[model.servedIndex];
  return `${model.failedCount} failed → ${served?.provider ?? served?.step ?? 'served'}`;
}

/** The cost value + its confidence badge — ONE renderer, so the collapsed one-liner and the full
 * band show the IDENTICAL formatted pair (gap 07: the value and its tag must never desync). */
function CostCell({ costView, unavailableText }: { costView: ReturnType<typeof costDisplay>; unavailableText?: string }) {
  return (
    <>
      <span className="text-meta" data-testid="detail-cost" data-confidence={costView.confidence}>{costView.value === '—' && unavailableText ? unavailableText : costView.value}</span>
      {/* Gap 07: an `estimated` cost MUST be labelled (the cross-cutting rule) — a small
          tag so an operator never mistakes a best-effort figure for a confident one. An
          `unavailable` cost already reads as `—`; `confident` needs no badge. */}
      {costView.estimated && (
        <span
          className="ml-1.5 rounded-sm bg-status-cooling/15 px-1 py-0.5 text-[10px] uppercase tracking-wide text-status-cooling"
          data-testid="cost-confidence"
          data-confidence="estimated"
          title="cost is an estimate — a billed token class has no configured rate"
        >
          est
        </span>
      )}
    </>
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
    return <span className="text-xs text-status-down" role="status" aria-live="polite" data-testid="kill-forbidden">mutations disabled</span>;
  }
  if (killState.phase === 'killed') {
    return <span className="text-xs text-text-muted" data-testid="kill-done">killed</span>;
  }
  if (killState.phase === 'error') {
    return <span className="text-xs text-status-down" role="alert" data-testid="kill-error" title={killState.message}>kill failed: {killState.message}</span>;
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

function TabButton({
  buttonRef,
  id,
  active,
  onClick,
  onKeyDown,
  children,
}: {
  buttonRef: (node: HTMLButtonElement | null) => void;
  id: Tab;
  active: Tab;
  onClick: (t: Tab) => void;
  onKeyDown: React.KeyboardEventHandler<HTMLButtonElement>;
  children: React.ReactNode;
}) {
  const selected = id === active;
  return (
    <button
      ref={buttonRef}
      id={drawerTabId(id)}
      type="button"
      role="tab"
      aria-selected={selected}
      aria-controls={drawerPanelId(id)}
      tabIndex={selected ? 0 : -1}
      onClick={() => onClick(id)}
      onKeyDown={onKeyDown}
      className={cn(
        'rounded-md px-2.5 py-1 text-xs transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent',
        selected
          ? 'bg-accent/20 text-text ring-1 ring-inset ring-accent/60'
          : 'text-text hover:bg-panel hover:text-text',
      )}
    >
      {children}
    </button>
  );
}

function HeadersTab({ headers }: { headers?: Record<string, string> }) {
  if (headers === undefined) {
    return (
      <EmptyState kind="unavailable" testId="headers-unavailable">
        Header capture was not provided for this request. It may have been disabled or the detail may have been evicted.
      </EmptyState>
    );
  }
  const entries = headers ? Object.entries(headers) : [];
  if (entries.length === 0) {
    return <EmptyState kind="empty" testId="headers-empty">No inbound headers were present.</EmptyState>;
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

function CapturedSectionsTab({ detail }: { detail: FlowDetailDto | null }) {
  if (detail === null) {
    return (
      <EmptyState kind="unavailable">
        Captured I/O is unavailable in this view. Live detail may have been evicted or is not available for a historical cut.
      </EmptyState>
    );
  }
  const sections = detail?.captured_sections ?? [];
  if (sections.length === 0) {
    return (
      <EmptyState kind="disabled">
        Durable I/O capture was not enabled for this request.
      </EmptyState>
    );
  }
  return (
    <div className="space-y-2 p-3" data-testid="captured-sections-tab">
      {sections.map((section) => (
        <details key={section.name} className="rounded border border-line bg-panel-raised" open>
          <summary className="cursor-pointer px-3 py-2 text-xs font-medium text-text">
            {section.name} · {section.bytes.toLocaleString()} bytes{section.partial ? ' · partial' : ''}
          </summary>
          <pre className="max-h-80 overflow-auto border-t border-line p-3 font-mono text-[11px] text-text-muted">
            {typeof section.content === 'string' ? section.content : JSON.stringify(section.content, null, 2)}
          </pre>
        </details>
      ))}
    </div>
  );
}

/**
 * The Error tab (gap 14 enriched): the terminal reason + monitor error, PLUS the captured upstream
 * error BODY (gap 05's `upstream_response`, live-detail only) when present. The capture state is
 * EXPLICIT (spec 14 don't-lie-with-zeros): a captured body is shown (`measured`, truncation flagged);
 * when capture is OFF/no body, an ERROR flow shows an explicit `unavailable` line — NEVER a blank that
 * implies "no error".
 *
 * The "No error." empty state depends ONLY on `!isError` (review round-3 HIGH): a FAILED/error flow
 * must NEVER read "No error.", regardless of `seeking` or whether live-body capture is suppressed. So
 * the capture block renders whenever `isError`. The captured body itself is live-detail only (`detail`
 * is `frozenDetail`, `null` while seeking), so on a SEEKED historical flow it is honestly UNAVAILABLE
 * and labelled as the HISTORICAL state ("capture unavailable on historical view"), DISTINCT from the
 * live capture-disabled state — never "No error.".
 */
function ErrorTab({ detail, liveFlow, joinError, seeking }: { detail: FlowDetailDto | null; liveFlow: FlowSummary | null; joinError: string | null; seeking: boolean }) {
  const reason = liveFlow?.terminal_reason ?? detail?.terminal_reason ?? null;
  const status = liveFlow?.status ?? detail?.status ?? null;
  // A GENUINE error/failure — the flow FAILED, or a monitor error was reported. This (NOT a benign
  // `terminal_reason`, which a CLEAN completed flow also carries, e.g. `response.completed`) is what
  // warrants the upstream error-body capture block. Decoupled from `seeking`/capture suppression
  // (review round-3 HIGH): an error/failed flow ALWAYS renders the capture block + NEVER "No error.".
  const isError = status === 'failed' || !!joinError;
  const captured = capturedErrorBody(detail?.upstream_response);

  // "No error." ONLY when there is genuinely nothing to show: NOT an error AND no terminal reason to
  // display. A failed flow (`isError`) NEVER reaches here — regardless of `seeking` or whether the
  // live body is suppressed (review round-3 HIGH). A clean completed flow with a benign
  // `terminal_reason` shows that reason (below) but no capture block (it is not an error).
  if (!isError && !reason) {
    return <EmptyState kind="not-applicable" testId="error-empty">No error occurred for this request.</EmptyState>;
  }

  // The captured body is LIVE-detail only. While SEEKING a historical flow there is no live body, so
  // the unavailable reason is the HISTORICAL view (not "capture disabled") — labelled honestly so the
  // operator knows the body may exist live but is unavailable for this frozen cut.
  const captureUnavailableHistorical = seeking;
  return (
    <div className="px-3 py-2 text-xs" data-testid="error-tab">
      {reason && (
        <div className="mb-1">
          <span className="text-text-muted">terminal reason: </span>
          <span className="font-mono text-status-down">{reason}</span>
        </div>
      )}
      {joinError && (
        <div className="mb-1">
          <span className="text-text-muted">monitor error: </span>
          <span className="font-mono text-status-down">{joinError}</span>
        </div>
      )}
      {/* The upstream error-body capture block renders for a GENUINE error/failure ONLY (a benign
          completed `terminal_reason` shows above but warrants no error body). It ALWAYS renders for an
          error flow — captured (`measured`), the historical-view `unavailable` state while seeking, or
          the live capture-disabled `unavailable` state — NEVER omitted into a blank "no error". */}
      {isError && (
        <div className="mt-1 border-t border-line/60 pt-1.5" data-testid="error-capture" data-state={captured.state} data-quality={captured.quality}>
          <div className="mb-1 flex items-center gap-1.5">
            <span className="text-[10px] uppercase tracking-wide text-text-muted">upstream error body</span>
            {captured.state === 'captured' && captured.truncated && (
              <span className="rounded-sm bg-status-cooling/15 px-1 text-[9px] uppercase text-status-cooling" data-testid="error-capture-truncated" title="truncated by the capture cap — a prefix of the full body">
                truncated
              </span>
            )}
          </div>
          {captured.state === 'captured' ? (
            // The captured upstream error body — shown to the operator (an auth-gated DIAGNOSTIC
            // surface; showing the upstream error body is the WHOLE POINT of capturing it).
            <pre className="max-h-32 overflow-auto whitespace-pre-wrap break-words rounded-sm bg-bg/60 p-1.5 font-mono text-[10px] text-text" data-testid="error-capture-body" title={captured.detail}>
              {fmtCapturedBody(captured.body)}
            </pre>
          ) : captureUnavailableHistorical ? (
            // SEEKING a historical flow: the body is LIVE-only, so it is UNAVAILABLE for this frozen cut
            // (NOT "capture disabled" — it may exist live). Explicit, never a blank implying "no error".
            <div className="rounded-sm border border-dashed border-line/70 px-1.5 py-1 text-[10px] italic text-text-muted" data-testid="error-capture-historical" title="the captured upstream error body is live-detail only and is unavailable on a historical (seeked) view — not a claim that no error occurred">
              capture unavailable on historical view — the upstream error body is live-only. Not "no error".
            </div>
          ) : (
            // LIVE error flow, capture DISABLED / no body / evicted — explicit (NOT a blank).
            <div className="rounded-sm border border-dashed border-line/70 px-1.5 py-1 text-[10px] italic text-text-muted" data-testid="error-capture-disabled" title={captured.detail}>
              capture disabled — no upstream error body captured (set LLMCONDUIT_DASHBOARD_CAPTURE_UPSTREAM_RESPONSE=1). Not "no error".
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** Render a captured upstream error body (JSON value or string) as readable text for the tab. */
function fmtCapturedBody(body: unknown): string {
  if (typeof body === 'string') return body.length > 0 ? body : '(empty body)';
  try {
    return JSON.stringify(body, null, 2);
  } catch {
    return String(body);
  }
}
