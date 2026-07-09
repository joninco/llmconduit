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
import { useCallback, useEffect, useState } from 'react';
import { Group, Panel, Separator, useDefaultLayout, usePanelRef } from 'react-resizable-panels';
import { getConnection, teardownSession } from './api/connection';
import { useAuth } from './store/hooks';
import { LoginShell } from './components/LoginShell';
import { NavTabs } from './components/NavTabs';
import { StatsStrip } from './components/StatsStrip/StatsStrip';
import { Scrubber } from './components/Scrubber/Scrubber';
import { EdgeStrip } from './components/ui/EdgeStrip';
import { useHashRoute } from './router/useHashRoute';
import { VIEW_BY_ROUTE } from './views/registry';

export function App() {
  const authed = useAuth((s) => s.authenticated);
  const { client } = getConnection();

  if (!authed) {
    return <LoginShell client={client} />;
  }
  return <Dashboard />;
}

function Dashboard() {
  const { client, socket } = getConnection();
  const route = useHashRoute();
  const ActiveView = VIEW_BY_ROUTE[route];

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
          defaultSize={176}
          minSize={80}
          maxSize={260}
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
              <StatsStrip />
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
            <ActiveView />
          </main>
        </Panel>
      </Group>
    </div>
  );
}
