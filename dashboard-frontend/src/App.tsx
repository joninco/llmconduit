/**
 * App shell (D9 §"Hash router + layout shell"): stats-strip slot (top), scrubber slot
 * (under it), view router with the four routes. Gated by auth — the login shell renders
 * when unauthenticated; a 401 anywhere routes through `teardownSession()` (cache cleared,
 * stores reset, WS closed) which flips auth off and returns to the login shell.
 *
 * Everything below the nav bar is one vertical split group: the chrome band (stats strip +
 * scrubber) is a collapsible pixel-sized panel over the view, so ANY view (and any drill-down
 * pane inside it) can reclaim the full height up to the nav — the nav bar is the only row that
 * never collapses. The band collapses to a thin labeled strip, never hides entirely.
 */
import {
  Component,
  Suspense,
  lazy,
  useCallback,
  useEffect,
  useRef,
  useState,
  type ComponentType,
  type ErrorInfo,
  type ReactNode,
} from 'react';
import { Group, Panel, Separator, useDefaultLayout, usePanelRef } from 'react-resizable-panels';
import { getConnection, teardownSession } from './api/connection';
import { useAuth, useDashboard } from './store/hooks';
import { LoginShell } from './components/LoginShell';
import { NavTabs } from './components/NavTabs';
import { Scrubber } from './components/Scrubber/Scrubber';
import { EdgeStrip } from './components/ui/EdgeStrip';
import { useHashRoute, useHashScope } from './router/useHashRoute';
import { VIEW_BY_ROUTE } from './views/registry';
import { flowFilterStore } from './store/flowFilterStore';
import { authStore } from './store/authStore';
import { ScopeBar } from './components/ScopeBar';
import { useMediaQuery } from './lib/useMediaQuery';
import { DurabilityBanner } from './components/DurabilityBanner';

const StatsStrip = lazy(() => import('./components/StatsStrip/StatsStrip').then((module) => ({ default: module.StatsStrip })));
const CompactStatsStrip = lazy(() => import('./components/StatsStrip/StatsStrip').then((module) => ({ default: module.CompactStatsStrip })));

function LazyStatsStrip({ onCompact }: { onCompact?: () => void }) {
  return (
    <Suspense fallback={<div className="m-2 h-24 animate-pulse rounded-md border border-line bg-panel sm:m-4" role="status" aria-label="Loading gateway metrics" />}>
      <StatsStrip onCompact={onCompact} />
    </Suspense>
  );
}

/**
 * U4 — the full metrics strip costs ~30% of every viewport but only Overview needs it at full
 * size. Non-Overview tabs default to the one-line compact strip; expanding pins the full strip
 * (persisted), compacting unpins. Overview always renders full.
 */
const STRIP_PIN_KEY = 'argus:metrics-strip:pinned-full';

function readStripPin(): boolean {
  try { return window.localStorage.getItem(STRIP_PIN_KEY) === '1'; } catch { return false; }
}

function writeStripPin(pinned: boolean) {
  try {
    if (pinned) window.localStorage.setItem(STRIP_PIN_KEY, '1');
    else window.localStorage.removeItem(STRIP_PIN_KEY);
  } catch { /* storage unavailable */ }
}

export function App() {
  const authed = useAuth((s) => s.authenticated);
  const fatalError = useDashboard((s) => s.fatalError);
  const { client, mock } = getConnection();

  if (fatalError) {
    return (
      <main className="grid h-full place-items-center bg-bg p-6 text-text" role="alert">
        <div className="max-w-lg rounded-lg border border-status-down/50 bg-panel p-6">
          <h1 className="font-ui text-lg font-semibold">Dashboard upgrade required</h1>
          <p className="mt-2 text-sm text-text-muted">{fatalError}</p>
          <button className="mt-5 rounded-md bg-accent px-4 py-2 text-sm text-bg" onClick={() => window.location.reload()}>
            Reload dashboard
          </button>
        </div>
      </main>
    );
  }
  if (!authed) {
    return (
      <LoginShell
        client={client}
        // The real host must reload its server-authored bootstrap. The in-browser mock has no
        // server shell to rehydrate, so retain its explicit test/dev-only state transition.
        onAuthenticated={mock ? () => authStore.getState().setAuthenticated(true) : undefined}
      />
    );
  }
  return <Dashboard />;
}

function Dashboard() {
  const { client, socket } = getConnection();
  const route = useHashRoute();
  const scope = useHashScope();
  const ActiveView = VIEW_BY_ROUTE[route];
  const narrow = useMediaQuery('(max-width: 1023px)');
  const [stripPinnedFull, setStripPinnedFull] = useState(readStripPin);
  // R2: the compact shell has no Scrubber, so time travel must force the full band — otherwise
  // non-Overview tabs can neither enter a seek nor show the LIVE/adjust controls mid-seek.
  const seeking = useDashboard((s) => s.connection === 'seeking');
  const compactStrip = route !== 'overview' && !stripPinnedFull && !seeking;

  useEffect(() => {
    flowFilterStore.getState().hydrate({
      status: scope.status,
      model: scope.model,
      upstream: scope.upstream,
      client: scope.client,
    });
  }, [scope.status, scope.model, scope.upstream, scope.client]);

  // Chrome-band collapse state (mirrors the FlowDetail pattern): the persisted %-layout is
  // owned by useDefaultLayout; the boolean re-syncs from `isCollapsed()` on every resize.
  const chrome = useDefaultLayout({ id: 'argus-shell-vsplit', panelIds: ['shell-chrome', 'shell-view'] });
  const chromeRef = usePanelRef();
  const [chromeCollapsed, setChromeCollapsed] = useState(false);
  const onChromeResize = useCallback(() => {
    const c = chromeRef.current?.isCollapsed() ?? false;
    setChromeCollapsed((prev) => (prev === c ? prev : c));
  }, [chromeRef]);
  useEffect(() => {
    const h = chromeRef.current;
    if (!h) return;
    if (chromeCollapsed && !h.isCollapsed()) h.collapse();
    else if (!chromeCollapsed && h.isCollapsed()) h.expand();
  }, [chromeCollapsed, chromeRef]);

  const pinStrip = useCallback((pinned: boolean) => {
    writeStripPin(pinned);
    setStripPinnedFull(pinned);
    // R2: "Expand metrics" must land on the full band — a stale collapsed chrome flag would
    // otherwise remount the group showing only the drag-collapse edge strip.
    if (pinned) setChromeCollapsed(false);
  }, []);

  // Open the WS once on mount; close on unmount. StrictMode double-mounts in dev — the
  // socket.connect()/disconnect() pair is idempotent so no duplicate pipe leaks.
  useEffect(() => {
    socket.connect();
    return () => socket.disconnect();
  }, [socket]);

  async function onLogout() {
    try {
      await client.logout();
    } finally {
      // Centralized teardown: clears the REST cache + resets both stores + closes the WS,
      // so no session-scoped data survives logout (finding 1).
      teardownSession();
    }
  }

  return (
    <div className="flex h-full flex-col bg-bg text-text">
      <NavTabs active={route} onLogout={onLogout} />
      <DurabilityBanner />
      <DashboardAnnouncements />
      <ScopeBar />
      {narrow ? (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden" data-testid="mobile-shell">
          {/* R2/U4: the metrics disclosure defaults OPEN only on Overview; other tabs start
              with their content, not a ~42vh metrics band. `key` remounts per route so the
              per-route default applies even after a manual toggle. */}
          <details key={route} className="shrink-0 border-b border-line bg-panel" open={route === 'overview'}>
            <summary className="cursor-pointer px-4 py-2 text-xs font-medium uppercase tracking-[0.14em] text-text-muted">
              Metrics · timeline
            </summary>
            <div className="max-h-[42vh] overflow-auto pb-2">
              <LazyStatsStrip />
              <Scrubber socket={socket} />
            </div>
          </details>
          <main className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
            <RouteContent ActiveView={ActiveView} routeKey={route} />
          </main>
        </div>
      ) : compactStrip ? (
        <div className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="compact-shell">
          <Suspense fallback={<div className="m-2 h-9 animate-pulse rounded-md border border-line bg-panel sm:m-4 sm:mb-0" role="status" aria-label="Loading gateway status" />}>
            <CompactStatsStrip onExpand={() => pinStrip(true)} />
          </Suspense>
          <main className="mt-2 flex min-h-0 min-w-0 flex-1 overflow-hidden sm:mt-3">
            <RouteContent ActiveView={ActiveView} routeKey={route} />
          </main>
        </div>
      ) : (
        <Group
        orientation="vertical"
        id="argus-shell"
        className="min-h-0 min-w-0 flex-1"
        defaultLayout={chrome.defaultLayout}
        onLayoutChanged={chrome.onLayoutChanged}
      >
        {/* Chrome band: PIXEL-sized (its content has intrinsic height, not a share of the
            viewport) and pixel-preserving on window resize. Drag the splitter up to collapse it
            to the labeled strip; click the strip (or drag back down) to restore. */}
        <Panel
          id="shell-chrome"
          collapsible
          collapsedSize={14}
          defaultSize={242}
          minSize={112}
          maxSize={420}
          groupResizeBehavior="preserve-pixel-size"
          panelRef={chromeRef}
          onResize={onChromeResize}
          className="flex min-h-0 min-w-0 flex-col"
          style={{ overflow: 'hidden' }}
        >
          {chromeCollapsed ? (
            <EdgeStrip
              label="metrics · timeline"
              onExpand={() => setChromeCollapsed(false)}
              testid="shell-chrome-strip"
              orientation="horizontal"
              className="border-b border-line"
            />
          ) : (
            <>
              {/* stats-strip slot */}
              <LazyStatsStrip onCompact={route !== 'overview' ? () => pinStrip(false) : undefined} />
              {/* scrubber slot */}
              <Scrubber socket={socket} />
            </>
          )}
        </Panel>
        <Separator
          id="split-shell"
          className="h-px bg-line outline-none transition-colors hover:bg-accent/70 focus-visible:bg-accent"
        />
        <Panel id="shell-view" className="flex min-h-0 min-w-0" style={{ overflow: 'hidden' }}>
          {/* view router */}
          <main className="flex min-h-0 min-w-0 flex-1 overflow-hidden">
            <RouteContent ActiveView={ActiveView} routeKey={route} />
          </main>
        </Panel>
      </Group>
      )}
    </div>
  );
}

export function RouteContent({ ActiveView, routeKey }: { ActiveView: ComponentType; routeKey: string }) {
  return (
    <RouteErrorBoundary key={routeKey}>
      <Suspense
        fallback={
          <div className="flex min-h-0 flex-1 items-center justify-center text-sm text-text-muted" role="status">
            Loading view…
          </div>
        }
      >
        <ActiveView />
        <RouteLoaded />
      </Suspense>
    </RouteErrorBoundary>
  );
}

function RouteLoaded() {
  useEffect(() => {
    try { window.sessionStorage.removeItem('argus:chunk-recovery:v1'); } catch { /* storage unavailable */ }
  }, []);
  return null;
}

export class RouteErrorBoundary extends Component<
  { children: ReactNode },
  { error: Error | null; chunkFailure: boolean }
> {
  state = { error: null as Error | null, chunkFailure: false };

  static getDerivedStateFromError(error: Error) {
    return { error, chunkFailure: false };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('[ARGUS] route render failed', {
      kind: 'render-or-chunk-load',
      message: error.message,
      route: window.location.href,
      componentStack: info.componentStack,
    });
    void import('./lib/chunkRecovery').then(({ armChunkRecovery, isChunkLoadError }) => {
      if (!isChunkLoadError(error)) return;
      this.setState({ chunkFailure: true });
      if (armChunkRecovery(window.sessionStorage, window.location.href)) {
        console.warn('[ARGUS] dashboard assets changed; reloading this route once');
        window.location.reload();
      }
    });
  }

  render() {
    if (!this.state.error) return this.props.children;
    const title = this.state.chunkFailure ? 'ARGUS was updated while this view was loading' : 'This view could not be rendered';
    const explanation = this.state.chunkFailure
      ? 'The dashboard tried a fresh reload, but this view still could not load. Your route, filters, and query are preserved.'
      : 'An unexpected rendering error occurred in this view.';
    return (
      <section className="m-auto max-w-lg rounded-lg border border-status-down/50 bg-panel p-6" role="alert">
        <h1 className="font-ui text-lg font-semibold">{title}</h1>
        <p className="mt-2 text-sm text-text-muted">{explanation}</p>
        <button
          type="button"
          className="mt-4 rounded bg-accent px-3 py-2 text-sm text-bg focus-visible:ring-2 focus-visible:ring-accent"
          onClick={() => window.location.reload()}
        >
          {this.state.chunkFailure ? 'Reload Dashboard' : 'Retry'}
        </button>
        <details className="mt-4 text-xs text-text-muted">
          <summary className="cursor-pointer select-none text-accent">Technical details</summary>
          <code className="mt-2 block break-all rounded border border-line bg-bg p-2">
            {this.state.error.message || 'Unexpected route error.'}
          </code>
        </details>
      </section>
    );
  }
}

/** Polite, event-level announcements only—never token/usage deltas. */
export function DashboardAnnouncements() {
  const connection = useDashboard((state) => state.connection);
  const flows = useDashboard((state) => state.flows);
  const hasData = useDashboard((state) =>
    state.metrics !== null || state.flows.size > 0 || state.topologyNodes.length > 0,
  );
  const previous = useRef<Map<string, string> | null>(null);
  const [message, setMessage] = useState('');

  useEffect(() => {
    const label = {
      idle: 'Dashboard idle.',
      connecting: hasData ? 'Dashboard reconnecting. Displayed data may be stale.' : 'Dashboard connecting.',
      live: 'Dashboard connection live.',
      seeking: 'Historical dashboard view active.',
      closed: 'Dashboard connection closed.',
      error: 'Dashboard transport failed.',
    }[connection];
    setMessage(label);
  }, [connection, hasData]);

  useEffect(() => {
    const next = new Map<string, string>();
    for (const [id, flow] of flows) next.set(id, flow.status);
    if (previous.current) {
      for (const [id, status] of next) {
        const before = previous.current.get(id);
        if (before === status) continue;
        if (before === undefined && status === 'open') setMessage(`Stream ${id} started.`);
        else if (status !== 'open') setMessage(`Stream ${id} ${status}.`);
        break;
      }
    }
    previous.current = next;
  }, [flows]);

  return <div className="sr-only" aria-live="polite" aria-atomic="true">{message}</div>;
}
