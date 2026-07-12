/**
 * TheaterView (D12) — the "wow": a fullscreen-capable dark grid of live "rivers", one per active
 * stream plus the last completed response while idle. Each river streams its
 * output/reasoning/tool deltas (from the store's incremental
 * `riverFold`, fed per `segment_append` at arrival — ring-eviction-proof), with a per-river
 * tokens/sec meter + a blinking cursor. The grid
 * auto-sizes: 1 river → big, 2 → split, 3-6 → a 3-wide multi-grid. A fullscreen toggle expands the
 * theater over the whole viewport.
 *
 * SEEK (D11) — durable SQLite cuts carry a monitor cursor and persisted transcript. Theater folds
 * those messages into frozen rivers when available and falls back to terminal summaries for legacy
 * or incomplete history. Leaving seek returns to the live rivers.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { River } from '../components/viz/River';
import { gridColumns } from '../components/viz/riverModel';
import { useLingeringRivers } from '../components/viz/useLingeringRivers';
import { useLastTerminalRiver, useLiveRivers } from '../components/viz/useLiveRivers';
import { useDashboard } from '../store/hooks';
import type { FlowSummary, TheaterRiver } from '../api/types';
import { formatStaleAge } from '../lib/staleAge';
import { getConnection, queryKeys } from '../api/connection';
import type { River as RiverData } from '../components/viz/riverModel';
import { navigate } from '../router/useHashRoute';

/** Convert the permanent backend projection into the same presentational model as live deltas. */
function restoredRiver(value: TheaterRiver | null | undefined): RiverData | null {
  if (!value) return null;
  return {
    id: value.id,
    model: value.model,
    status: value.status === 'completed' ? 'completed' : 'failed',
    startedAtMs: value.started_at_ms,
    error: null,
    output: value.output,
    reasoning: value.reasoning,
    tools: value.tools,
    truncated: value.truncated,
    firstMs: value.started_at_ms,
    lastMs: value.terminal_at_ms,
    terminalAtMs: value.terminal_at_ms,
    approxTokens: value.approx_tokens,
    tokensPerSec: value.tokens_per_sec,
  };
}

export function TheaterView() {
  const seeking = useDashboard((s) => s.connection === 'seeking');
  return seeking ? <HistoricalTheater /> : <LiveTheater />;
}

/**
 * Live rivers (one per `response_id`), auto-gridded, fullscreen-toggleable. Rivers come from the
 * store's INCREMENTAL fold (`useLiveRivers`), not a rebuild off the capped monitor ring — so a long
 * stream keeps its full text (the ring's eviction used to visibly delete tokens from the top and
 * drop the reasoning channel, which streams first).
 */
function LiveTheater() {
  const [fullscreen, setFullscreen] = useState(false);
  const dialogRef = useRef<HTMLDialogElement>(null);
  const fullscreenToggleRef = useRef<HTMLButtonElement>(null);
  const wasFullscreenRef = useRef(false);
  const liveRivers = useLiveRivers();
  const liveLastTerminal = useLastTerminalRiver();
  const { client, queryClient } = getConnection();
  const archive = useQuery({
    queryKey: queryKeys.theater(),
    queryFn: () => client.theater(),
    gcTime: Infinity,
  }, queryClient);
  const archivedLastTerminal = useMemo(
    () => restoredRiver(archive.data?.last_terminal),
    [archive.data],
  );
  const lastTerminal = liveLastTerminal ?? archivedLastTerminal;
  const activeCount = liveRivers.filter((river) => river.status === 'running').length;
  const [theaterNow, setTheaterNow] = useState(() => Date.now());
  useEffect(() => {
    if (activeCount === 0) return;
    const id = globalThis.setInterval(() => setTheaterNow(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, [activeCount]);
  // Older terminated tiles still linger/fade, but the newest terminal response is retained while
  // no stream is active — including after monitor eviction. The hook owns boundary timers.
  const rivers = useLingeringRivers(liveRivers, { retainLast: lastTerminal });
  const flowMap = useDashboard((state) => state.flows);
  const connection = useDashboard((state) => state.connection);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [pinnedId, setPinnedId] = useState<string | null>(null);
  const [paused, setPaused] = useState(false);
  const [followTail, setFollowTail] = useState(true);
  const [jumpToken, setJumpToken] = useState(0);
  const [query, setQuery] = useState('');
  const [modelFilter, setModelFilter] = useState('');
  const [providerFilter, setProviderFilter] = useState('');
  const [clientFilter, setClientFilter] = useState('');
  const [statusFilter, setStatusFilter] = useState('');
  const pausedRivers = useRef(rivers);
  if (!paused) pausedRivers.current = rivers;
  const availableRivers = paused ? pausedRivers.current : rivers;
  const flowByResponse = useMemo(() => new Map(
    [...flowMap.values()].filter((flow) => flow.response_id).map((flow) => [flow.response_id!, flow]),
  ), [flowMap]);
  const flowFor = (river: RiverData) => flowByResponse.get(river.id);
  const models = [...new Set(availableRivers.map((river) => river.model).filter(Boolean) as string[])].sort();
  const providers = [...new Set(availableRivers.map((river) => flowFor(river)?.upstream_target).filter(Boolean) as string[])].sort();
  const clients = [...new Set(availableRivers.map((river) => flowFor(river)?.client_label).filter(Boolean) as string[])].sort();
  const normalizedQuery = query.trim().toLowerCase();
  const visibleRivers = availableRivers.filter((river) => {
    const flow = flowFor(river);
    if (modelFilter && river.model !== modelFilter) return false;
    if (providerFilter && flow?.upstream_target !== providerFilter) return false;
    if (clientFilter && flow?.client_label !== clientFilter) return false;
    if (statusFilter && river.status !== statusFilter) return false;
    if (normalizedQuery && !`${river.id} ${river.model ?? ''} ${river.output} ${river.reasoning} ${river.tools.join(' ')}`.toLowerCase().includes(normalizedQuery)) return false;
    return true;
  });
  const effectiveSelectedId = pinnedId ?? selectedId ?? visibleRivers[0]?.id ?? null;
  const selectedRiver = availableRivers.find((river) => river.id === effectiveSelectedId) ?? null;
  const selectedFlow = selectedRiver ? flowFor(selectedRiver) : undefined;
  const lastResponseAtMs = activeCount === 0
    ? lastTerminal?.terminalAtMs ?? lastTerminal?.lastMs ?? null
    : null;
  const cols = gridColumns(visibleRivers.length);

  useEffect(() => {
    if (!fullscreen) {
      if (wasFullscreenRef.current) {
        requestAnimationFrame(() => fullscreenToggleRef.current?.focus({ preventScroll: true }));
      }
      wasFullscreenRef.current = false;
      return;
    }

    wasFullscreenRef.current = true;
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (typeof dialog.showModal === 'function') {
      if (!dialog.open) dialog.showModal();
    } else {
      // jsdom and a few older embedded engines expose <dialog> without showModal(). Keeping the
      // open attribute makes the fallback usable; supported browsers still get native modality.
      dialog.setAttribute('open', '');
    }
    requestAnimationFrame(() => fullscreenToggleRef.current?.focus({ preventScroll: true }));

    return () => {
      if (typeof dialog.close === 'function' && dialog.open) dialog.close();
      else dialog.removeAttribute('open');
    };
  }, [fullscreen]);

  const content = (
    <>
      <header className="mb-3 flex flex-wrap items-center gap-3">
        <h2 id="theater-title" className="text-base font-semibold text-text">Theater</h2>
        <p className="text-sm text-text-muted">live streams · {activeCount} active</p>
        <TheaterState
          connection={connection}
          activeCount={activeCount}
          selected={selectedRiver}
          lastResponseAtMs={lastResponseAtMs}
          stalled={liveRivers.some((river) => {
            const lastActivity = river.lastMs ?? river.startedAtMs ?? river.firstMs;
            return river.status === 'running' && lastActivity != null && theaterNow - lastActivity > 30_000;
          })}
        />
        <button
          ref={fullscreenToggleRef}
          type="button"
          onClick={() => setFullscreen((value) => !value)}
          aria-pressed={fullscreen}
          className="ml-auto rounded-md border border-line px-2.5 py-1 text-xs text-text-muted transition-colors hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
          data-testid="theater-fullscreen-toggle"
        >
          {fullscreen ? 'exit fullscreen' : 'fullscreen'}
        </button>
      </header>
      <div className="mb-3 flex flex-wrap items-center gap-2 rounded border border-line bg-panel px-2 py-2" aria-label="Theater controls">
        <label className="text-[10px] text-text-muted">stream <select className="ml-1 h-7 max-w-48 rounded border border-line bg-bg px-1 text-xs text-text" value={effectiveSelectedId ?? ''} onChange={(event) => setSelectedId(event.target.value || null)}>{availableRivers.map((river) => <option key={river.id} value={river.id}>{river.model ?? river.id} · {river.status}</option>)}</select></label>
        <button type="button" className="rounded border border-line px-2 py-1 text-xs text-text-muted" aria-pressed={pinnedId !== null} disabled={!effectiveSelectedId} onClick={() => setPinnedId((id) => id ? null : effectiveSelectedId)}>{pinnedId ? 'Unpin' : 'Pin'}</button>
        <button type="button" className="rounded border border-line px-2 py-1 text-xs text-text-muted" aria-pressed={paused} onClick={() => setPaused((value) => !value)}>{paused ? 'Resume updates' : 'Pause updates'}</button>
        <button type="button" className="rounded border border-line px-2 py-1 text-xs text-text-muted" aria-pressed={followTail} onClick={() => setFollowTail((value) => !value)}>{followTail ? 'Following tail' : 'Follow tail'}</button>
        <button type="button" className="rounded border border-line px-2 py-1 text-xs text-text-muted" onClick={() => { setFollowTail(true); setJumpToken((value) => value + 1); }}>Jump latest</button>
        <button type="button" className="rounded border border-line px-2 py-1 text-xs text-text-muted" disabled={!selectedRiver} onClick={() => { if (selectedRiver) void navigator.clipboard?.writeText(`${selectedRiver.reasoning}${selectedRiver.reasoning ? '\n\n' : ''}${selectedRiver.output}${selectedRiver.tools.length ? `\n\n${selectedRiver.tools.join('\n')}` : ''}`); }}>Copy transcript</button>
        <button type="button" className="rounded border border-line px-2 py-1 text-xs text-accent disabled:text-text-muted" disabled={!selectedFlow} onClick={() => { if (selectedFlow) navigate('flows', selectedFlow.api_call_id); }}>Open request</button>
        <input type="search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search streams" className="h-7 min-w-40 flex-1 rounded border border-line bg-bg px-2 text-xs text-text" />
        <select aria-label="Filter theater by model" className="h-7 rounded border border-line bg-bg px-1 text-xs" value={modelFilter} onChange={(event) => setModelFilter(event.target.value)}><option value="">all models</option>{models.map((value) => <option key={value}>{value}</option>)}</select>
        <select aria-label="Filter theater by provider" className="h-7 rounded border border-line bg-bg px-1 text-xs" value={providerFilter} onChange={(event) => setProviderFilter(event.target.value)}><option value="">all providers</option>{providers.map((value) => <option key={value}>{value}</option>)}</select>
        <select aria-label="Filter theater by client" className="h-7 rounded border border-line bg-bg px-1 text-xs" value={clientFilter} onChange={(event) => setClientFilter(event.target.value)}><option value="">all clients</option>{clients.map((value) => <option key={value}>{value}</option>)}</select>
        <select aria-label="Filter theater by status" className="h-7 rounded border border-line bg-bg px-1 text-xs" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value)}><option value="">all status</option><option value="running">running</option><option value="completed">completed</option><option value="failed">failed</option></select>
        <button type="button" className="rounded px-2 py-1 text-xs text-accent" onClick={() => { setQuery(''); setModelFilter(''); setProviderFilter(''); setClientFilter(''); setStatusFilter(''); }}>Clear</button>
      </div>
      {availableRivers.length === 0 ? (
        <div className="flex flex-1 items-center justify-center text-sm text-text-muted" data-testid="theater-empty">
          No responses yet. Live output appears here as requests stream.
        </div>
      ) : visibleRivers.length === 0 ? (
        <div className="flex flex-1 items-center justify-center text-sm text-text-muted">No streams match the current controls.</div>
      ) : (
        <div
          className="grid min-h-0 flex-1 gap-3"
          style={{ gridTemplateColumns: `repeat(${cols}, minmax(0, 1fr))` }}
          data-testid="theater-grid"
          data-cols={cols}
        >
          {visibleRivers.map((river) => (
            <River key={river.id} river={river} exiting={river.exiting} retained={river.retained} selected={river.id === effectiveSelectedId} onSelect={() => setSelectedId(river.id)} followTail={followTail} jumpToken={jumpToken} onFollowChange={setFollowTail} />
          ))}
        </div>
      )}
    </>
  );

  if (fullscreen) {
    return (
      <dialog
        ref={dialogRef}
        aria-labelledby="theater-title"
        className="fixed inset-0 z-40 m-0 hidden h-[100dvh] max-h-none w-screen max-w-none flex-col border-0 bg-bg p-4 text-text backdrop:bg-black/70 open:flex"
        data-testid="theater-view"
        data-fullscreen="true"
        onCancel={(event) => {
          event.preventDefault();
          setFullscreen(false);
        }}
      >
        {content}
      </dialog>
    );
  }

  return (
    <div
      className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg p-4"
      data-testid="theater-view"
    >
      {content}
    </div>
  );
}

/** Same fixed-width, once-per-second stale clock used by the Stats strip. */
function TheaterState({ lastResponseAtMs, activeCount, connection, selected, stalled }: { lastResponseAtMs: number | null; activeCount: number; connection: string; selected: RiverData | null; stalled: boolean }) {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    setNowMs(Date.now());
    const id = globalThis.setInterval(() => setNowMs(Date.now()), 1_000);
    return () => globalThis.clearInterval(id);
  }, [lastResponseAtMs]);

  const age = lastResponseAtMs === null ? null : formatStaleAge(nowMs - lastResponseAtMs);
  const disconnected = connection === 'closed' || connection === 'error';
  const label = disconnected ? 'disconnected' : stalled ? 'stalled' : activeCount > 0 ? 'active' : age ? 'idle' : 'no responses yet';
  const detail = [selected ? `selected · ${selected.model ?? selected.id}` : null, age ? `last response ${age} ago` : null].filter(Boolean).join(' · ');
  return (
    <span
      className="flex shrink-0 items-baseline gap-2 rounded border border-line bg-panel px-2.5 py-1 text-text-muted"
      role="status"
      aria-label={`Theater ${label}${detail ? `, ${detail}` : ''}`}
      data-testid="theater-state"
      data-state={label.replaceAll(' ', '-')}
    >
      <span className="text-[10px] font-bold uppercase tracking-[0.14em]">{label}</span>
      {detail && <span className="text-[10px] text-text-muted" data-testid={age ? 'theater-idle-age' : undefined}>{detail}</span>}
    </span>
  );
}

/**
 * The frozen-seek theater replays persisted monitor messages when present and otherwise renders
 * the snapshot's terminal summaries with an explicit unavailable affordance.
 */
function HistoricalTheater() {
  const flows = useDashboard((s) => s.flows);
  const summaries = useMemo(() => [...flows.values()], [flows]);
  const rivers = useLiveRivers();
  const seekCutId = useDashboard((s) => s.seekCutId);
  const { client, queryClient } = getConnection();
  const archive = useQuery({
    queryKey: queryKeys.theater(seekCutId ?? undefined),
    queryFn: () => client.theater(seekCutId ?? undefined),
    enabled: seekCutId !== null,
    gcTime: Infinity,
  }, queryClient);
  const archivedRiver = useMemo(
    () => seekCutId === null ? null : restoredRiver(archive.data?.last_terminal),
    [archive.data, seekCutId],
  );
  const historicalRivers = rivers.length > 0 ? rivers : archivedRiver ? [archivedRiver] : [];

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-bg p-4" data-testid="theater-view-historical">
      <header className="mb-3 flex items-center gap-3">
        <h2 className="text-base font-semibold text-text">Theater</h2>
        <span
          className="rounded-sm border border-status-cooling/40 bg-status-cooling/10 px-2 py-0.5 text-[11px] text-status-cooling"
          data-testid="theater-historical-banner"
        >
          historical — persisted transcript; deltas not replayed when unavailable
        </span>
      </header>
      {historicalRivers.length > 0 ? (
        <div
          className="grid min-h-0 flex-1 gap-3"
          style={{ gridTemplateColumns: `repeat(${gridColumns(historicalRivers.length)}, minmax(0, 1fr))` }}
          data-testid="theater-historical-rivers"
        >
          {historicalRivers.map((river) => (
            <River key={river.id} river={river} exiting={false} retained={river.id === archivedRiver?.id} />
          ))}
        </div>
      ) : summaries.length === 0 ? (
        <div className="flex flex-1 items-center justify-center text-sm text-text-muted" data-testid="theater-historical-empty">
          No flows in this snapshot.
        </div>
      ) : (
        <div className="grid min-h-0 flex-1 auto-rows-min grid-cols-1 gap-2 overflow-auto sm:grid-cols-2 lg:grid-cols-3" data-testid="theater-historical-grid">
          {summaries.map((flow) => (
            <TerminalSummaryCard key={flow.api_call_id} flow={flow} />
          ))}
        </div>
      )}
    </div>
  );
}

/** A single frozen-flow card: model, status, token totals — the snapshot's terminal summary. */
function TerminalSummaryCard({ flow }: { flow: FlowSummary }) {
  const tokens = flow.usage?.total ?? 0;
  return (
    <div className="rounded-md border border-line bg-panel p-3" data-testid="theater-summary-card" data-flow-id={flow.api_call_id}>
      <div className="flex items-center gap-2">
        <span className="truncate font-mono text-xs text-text" title={flow.api_call_id}>
          {flow.model_served ?? flow.model_requested ?? flow.api_call_id}
        </span>
        <span className="ml-auto text-[11px] uppercase tracking-wide text-text-muted">{flow.status}</span>
      </div>
      <p className="mt-1 tabular-nums text-[11px] text-text-muted">
        {tokens} tokens{flow.terminal_reason ? ` · ${flow.terminal_reason}` : ''}
      </p>
    </div>
  );
}
