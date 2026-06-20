import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { act, cleanup, fireEvent, waitFor, within } from '@testing-library/react';
import { FlowDetail } from './FlowDetail';
import { dashboardStore } from '../../store/dashboardStore';
import { authStore } from '../../store/authStore';
import { mockKillLog, MOCK_KILL_UNAUTHORIZED_ID } from '../../api/mock';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from '../testHarness';
import type { DebugWsMessage, FlowDetail as FlowDetailDto } from '../../api/types';

function noop() {}

/** Push a monitor message into the live ring, stamped with its arrival `monitor_seq`. */
function pushMonitor(msg: DebugWsMessage, seq = 0): void {
  act(() => dashboardStore.getState().pushMonitor(msg, seq));
}

describe('FlowDetail — 3-pane inspector (mock backend)', () => {
  beforeEach(() => {
    // Mock mode: mockFetch answers /flows/:id + the kill route.
    resetWorld({ mock: true });
    authStore.getState().setMutationsEnabled(true);
    authStore.getState().setCsrfToken('mock-csrf-token');
    mockKillLog.length = 0;
    // The live store knows api_001 (open, linked to resp_001) so the join + kill work.
    seedFlows([makeFlow({ api_call_id: 'api_001', response_id: 'resp_001', status: 'open', model_requested: 'gpt-4o', model_served: 'llama-3.1-70b', upstream_target: 'vllm-a', started_ms: 1_700_000_000_000 })]);
  });
  afterEach(cleanup);

  it('renders all 3 bodies and tints the diff between layers (A→B→C)', async () => {
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    // The /flows/:id query resolves the three bodies.
    await waitFor(() => expect(getByTestId('jsonpane-code-A · inbound').querySelectorAll('.json-line').length).toBeGreaterThan(0));
    const paneB = getByTestId('jsonpane-code-B · normalized');
    const paneC = getByTestId('jsonpane-code-C · upstream');
    expect(paneB.querySelectorAll('.json-line').length).toBeGreaterThan(0);
    expect(paneC.querySelectorAll('.json-line').length).toBeGreaterThan(0);
    // The mock bodies differ at $.model (gpt-4o → llama) and add $.stream on C — tinted lines exist.
    const tintedB = paneB.querySelectorAll('.json-line[data-diff]');
    const tintedC = paneC.querySelectorAll('.json-line[data-diff]');
    expect(tintedB.length + tintedC.length).toBeGreaterThan(0);
  });

  it('scroll-syncs the panes (scrolling A mirrors to B and C)', async () => {
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('jsonpane-scroll-A · inbound')).toBeTruthy());
    const a = getByTestId('jsonpane-scroll-A · inbound');
    const b = getByTestId('jsonpane-scroll-B · normalized');
    const c = getByTestId('jsonpane-scroll-C · upstream');
    a.scrollTop = 40;
    fireEvent.scroll(a);
    expect(b.scrollTop).toBe(40);
    expect(c.scrollTop).toBe(40);
  });

  it('Timeline tab populates from monitor event_append; deltas render output + tool card', async () => {
    const { getByTestId, getByRole } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('flow-detail')).toBeTruthy());

    // Stream a timeline event + segments (output + a tool call) for resp_001. The segments arrive
    // AFTER the mock REST replay's coverage — the replay's max delta `sequence` is 3, so the live
    // continuation carries higher `monitor_seq`s (4, 5) and is appended past the replay watermark
    // (deltas merge by MonitorHub SEQUENCE — finding 2).
    pushMonitor({ type: 'event_append', response_id: 'resp_001', event: { timestamp_ms: 1, kind: 'response.created', summary: 'created', images: [] } }, 4);
    pushMonitor({ type: 'segment_append', response_id: 'resp_001', segment: { timestamp_ms: 1, kind: 'output', text: 'Hello world' } }, 4);
    pushMonitor({ type: 'segment_append', response_id: 'resp_001', segment: { timestamp_ms: 2, kind: 'tool', text: JSON.stringify({ name: 'search' }) } }, 5);

    // Deltas sub-panel shows the output text + an expandable tool card.
    expect(getByTestId('deltas-panel').textContent).toContain('Hello world');
    const card = getByTestId('tool-card');
    expect(card.textContent).toContain('search');
    fireEvent.click(card.querySelector('button')!);
    expect(getByTestId('tool-card-body')).toBeTruthy();

    // Timeline tab populates.
    fireEvent.click(getByRole('tab', { name: 'Timeline' }));
    expect(within(getByTestId('tabpanel-timeline')).getByText('response.created')).toBeTruthy();
  });

  it('kill POSTs with the CSRF header and optimistically flips the row to cancelled', async () => {
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    fireEvent.click(getByTestId('kill-button'));

    // Optimistic: the live row flips to cancelled immediately.
    expect(dashboardStore.getState().flows.get('api_001')?.status).toBe('cancelled');
    // The POST carried the X-CSRF-Token (D7 double-submit).
    await waitFor(() => expect(mockKillLog.at(-1)?.id).toBe('api_001'));
    expect(mockKillLog.at(-1)?.csrf).toBe('mock-csrf-token');
    await waitFor(() => expect(getByTestId('kill-done')).toBeTruthy());
  });

  it('a server 403 (no CSRF) is handled: rolls back + shows mutations-disabled', async () => {
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    // Clear the token AFTER the connection seeded it, so the kill omits X-CSRF-Token → mock 403.
    // (getConnection re-seeds auth from the bootstrap on (re)build, so we override post-render.)
    act(() => authStore.getState().setCsrfToken(null));
    document.cookie = 'llmconduit_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT';
    fireEvent.click(getByTestId('kill-button'));
    // Forbidden state surfaces and the optimistic flip is rolled back to open.
    await waitFor(() => expect(getByTestId('kill-forbidden')).toBeTruthy());
    expect(dashboardStore.getState().flows.get('api_001')?.status).toBe('open');
  });

  it('a 401 kill lets teardown win: the store stays CLEARED, the prior row is NOT re-inserted (finding 1)', async () => {
    // Seed an OPEN flow whose kill route answers 401 (session raced expiry). A valid CSRF is
    // present, so the request reaches the server and comes back 401 → centralized teardown clears
    // the live store. The optimistic rollback must NOT re-insert the prior row afterwards.
    seedFlows([makeFlow({ api_call_id: MOCK_KILL_UNAUTHORIZED_ID, status: 'open', started_ms: 1_700_000_000_000 })]);
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId={MOCK_KILL_UNAUTHORIZED_ID} onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    expect(dashboardStore.getState().flows.get(MOCK_KILL_UNAUTHORIZED_ID)).toBeTruthy();

    fireEvent.click(getByTestId('kill-button'));

    // The 401 routed through teardown (stores reset); the rollback did not re-leak the row.
    await waitFor(() => expect(dashboardStore.getState().flows.size).toBe(0));
    expect(dashboardStore.getState().flows.get(MOCK_KILL_UNAUTHORIZED_ID)).toBeUndefined();
  });

  it('pane B marks a field present in B but dropped by C as removed (finding 4)', async () => {
    // A crafted 3-layer fixture where `b_only` exists in B (normalized) but is dropped in C
    // (upstream). Pane B must visibly tint `b_only` as REMOVED (it leaves on the way to C), which
    // the A→B-only diff never showed. `api_replay` is unknown to the mock so /flows/:id 404s and
    // does NOT overwrite the injected detail.
    seedFlows([makeFlow({ api_call_id: 'api_replay', response_id: 'resp_replay', status: 'completed', started_ms: 1_700_000_000_000 })]);
    const detail: FlowDetailDto = {
      flow_seq: 1, api_call_id: 'api_replay', response_id: 'resp_replay', status: 'completed',
      deltas: [], started_ms: 1_700_000_000_000,
      // `b_only` is present in A and B alike (unchanged A→B) but dropped by C — so the ONLY signal
      // for it is the B→C removal, which must surface in pane B (it would be invisible under the
      // A→B-only diff that pane B previously used).
      inbound_body: { model: 'gpt-4o', keep: 1, b_only: 'dropped-next' },
      normalized: { model: 'llama-3.1-70b', keep: 1, b_only: 'dropped-next' },
      upstream_body: { model: 'llama-3.1-70b', keep: 1 }, // b_only dropped here
    };
    const { getByTestId, queryClient } = renderWithQuery(<FlowDetail apiCallId="api_replay" onClose={noop} />);
    act(() => queryClient.setQueryData(['flows', 'api_replay'], detail));
    await waitFor(() => expect(getByTestId('jsonpane-code-B · normalized').querySelectorAll('.json-line').length).toBeGreaterThan(0));
    const paneB = getByTestId('jsonpane-code-B · normalized');
    const bOnlyLine = paneB.querySelector('.json-line[data-path="$.b_only"]') as HTMLElement | null;
    // The B-only field that C drops is tinted removed in pane B (combined middle diff).
    expect(bOnlyLine?.dataset.diff).toBe('removed');
  });

  it('replays REST detail.deltas into the deltas panel for a completed flow (finding 5)', async () => {
    // A completed flow loaded via REST has NO live monitor segments — its streamed output lives
    // only in `detail.deltas`. The deltas panel must replay them rather than show "no deltas".
    // `api_replay` is unknown to the mock so the 404 won't replace the injected detail.
    seedFlows([makeFlow({ api_call_id: 'api_replay', response_id: 'resp_replay', status: 'completed', started_ms: 1_700_000_000_000 })]);
    const detail: FlowDetailDto = {
      flow_seq: 1, api_call_id: 'api_replay', response_id: 'resp_replay', status: 'completed',
      started_ms: 1_700_000_000_000,
      inbound_body: { model: 'gpt-4o' }, normalized: { model: 'm' }, upstream_body: { model: 'm' },
      deltas: [
        { sequence: 1, kind: 'response.created', payload: {}, ts_ms: 1 }, // lifecycle → dropped
        { sequence: 2, kind: 'response.output_text.delta', payload: { text: 'Replayed ' }, ts_ms: 2 },
        { sequence: 3, kind: 'response.output_text.delta', payload: { text: 'output' }, ts_ms: 3 },
      ],
    };
    const { getByTestId, queryClient } = renderWithQuery(<FlowDetail apiCallId="api_replay" onClose={noop} />);
    act(() => queryClient.setQueryData(['flows', 'api_replay'], detail));
    // The replayed deltas render (coalesced) even with NO live monitor frames pushed.
    await waitFor(() => expect(getByTestId('deltas-panel').textContent).toContain('Replayed output'));
  });

  it('detail cost roll-up shows even when the live row carries no cost (finding 4)', async () => {
    // api_001's live row (seeded in beforeEach) has NO cost/usage; the mock /flows/:id detail
    // carries the server roll-up cost. The header must surface that roll-up, not "—".
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    // Wait for the detail query (with the roll-up cost) to resolve.
    await waitFor(() => expect(getByTestId('jsonpane-code-A · inbound').querySelectorAll('.json-line').length).toBeGreaterThan(0));
    // The seeded mock detail cost for api_001 is 0.0061 → formatted into the cost/elapsed cell.
    await waitFor(() => expect(getByTestId('flow-detail').textContent).toContain('$0.0061'));
  });

  it('kill button is gated OFF (disabled, no POST) when mutations are disabled', async () => {
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    // Disable mutations AFTER the connection seeded the bootstrap value (which is `true` in mock).
    act(() => authStore.getState().setMutationsEnabled(false));
    const btn = getByTestId('kill-button');
    expect(btn).toBeDisabled();
    expect(btn.getAttribute('title')).toBe('mutations disabled');
    // A disabled button cannot dispatch a click → no kill POST is ever attempted.
    fireEvent.click(btn);
    expect(mockKillLog).toHaveLength(0);
  });
});

describe('FlowDetail — time-travel seek + body eviction', () => {
  beforeEach(() => {
    resetWorld({ mock: true });
    // An id the mock does NOT know (so /flows/:id 404s and never overwrites our injected
    // body-free detail) — modeling a historical flow whose live body was evicted (D5).
    seedFlows([makeFlow({ api_call_id: 'api_evicted', response_id: 'resp_evicted', status: 'completed', started_ms: 1_700_000_000_000 })]);
  });
  afterEach(cleanup);

  it('shows the snapshot badge and "body evicted (snapshot)" when seeking with no body', async () => {
    // A detail with evicted bodies (absent fields) — what /flows/:id returns post-eviction.
    const evicted: FlowDetailDto = {
      flow_seq: 1, api_call_id: 'api_evicted', response_id: 'resp_evicted', status: 'completed',
      deltas: [], started_ms: 1_700_000_000_000,
      // inbound_body / normalized / upstream_body intentionally ABSENT (evicted).
    };
    const { queryClient } = renderWithQuery(<FlowDetail apiCallId="api_evicted" onClose={noop} />);
    // Seed the body-free detail (the 404 fetch error won't replace manually-set data).
    act(() => queryClient.setQueryData(['flows', 'api_evicted'], evicted));
    // Enter seek (D11 paused) — the store connection flips to 'seeking'.
    act(() => dashboardStore.getState().setConnection('seeking'));

    await waitFor(() => {
      expect(document.querySelector('[data-testid="seek-badge"]')).toBeTruthy();
    });
    // Each pane shows the evicted placeholder (with the snapshot qualifier) rather than JSON.
    const empties = document.querySelectorAll('[data-testid^="jsonpane-empty-"]');
    expect(empties.length).toBe(3);
    expect(empties[0]!.textContent).toContain('body evicted');
  });

  it('DISABLES the kill button while seeking — no mutation against the frozen cut (finding 2)', async () => {
    // A historically-OPEN flow in the frozen snapshot. While seeking, the kill must be disabled so
    // the optimistic patch cannot mutate the frozen store row (and no abort POST is sent).
    seedFlows([makeFlow({ api_call_id: 'api_open_hist', response_id: 'resp_open_hist', status: 'open', started_ms: 1_700_000_000_000 })]);
    authStore.getState().setMutationsEnabled(true);
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_open_hist" onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    // Enter seek.
    act(() => dashboardStore.getState().setConnection('seeking'));
    const btn = getByTestId('kill-button');
    expect(btn).toBeDisabled();
    expect(btn.getAttribute('title')).toBe('paused (time-travel)');
    // A disabled button cannot click; even a forced kill is a no-op (store row stays 'open').
    fireEvent.click(btn);
    expect(dashboardStore.getState().flows.get('api_open_hist')?.status).toBe('open');
  });

  it('derives cost + elapsed from the FROZEN cut while seeking — no Date.now / live cost (findings 1+6)', async () => {
    // The frozen snapshot row is OPEN with a server roll-up cost; the LIVE /flows/:id detail
    // carries a DIFFERENT (post-seek) cost and a long elapsed. While seeking, the header must show
    // the frozen row's cost and an elapsed derived from the cut `at_ms`, not wall-clock Date.now().
    const started = 1_700_000_000_000;
    seedFlows([makeFlow({
      api_call_id: 'api_frozen', response_id: 'resp_frozen', status: 'open',
      started_ms: started, cost: 0.1234,
    })]);
    const liveDetail: FlowDetailDto = {
      flow_seq: 1, api_call_id: 'api_frozen', response_id: 'resp_frozen', status: 'open',
      deltas: [], started_ms: started,
      inbound_body: { model: 'gpt-4o' }, normalized: { model: 'm' }, upstream_body: { model: 'm' },
      cost: 0.9999, elapsed_ms: 999_000, // live values that must NOT bleed into the frozen view
    };
    const { getByTestId, queryClient } = renderWithQuery(<FlowDetail apiCallId="api_frozen" onClose={noop} />);
    act(() => queryClient.setQueryData(['flows', 'api_frozen'], liveDetail));
    // Enter seek with a KNOWN cut 5s after the flow started → deterministic frozen elapsed.
    act(() => dashboardStore.getState().enterSeek(started + 5_000));
    await waitFor(() => expect(document.querySelector('[data-testid="seek-badge"]')).toBeTruthy());

    const text = getByTestId('flow-detail').textContent ?? '';
    // Frozen roll-up cost shown; the live REST cost is NOT used.
    expect(text).toContain('$0.1234');
    expect(text).not.toContain('$1.00'); // 0.9999 would round to $1.00
    // Elapsed for the OPEN frozen flow = at_ms - started_ms = 5000ms → 5.0s. NOT the live detail's
    // 999000ms (→ 16m39s) and NOT a wall-clock Date.now() tick.
    expect(text).toContain('5.0s');
    expect(text).not.toContain('16m');
  });

  it('while seeking, an in-cut flow does NOT leak live REST deltas/headers; only the cut-bounded monitor stream shows (finding 1)', async () => {
    // The frozen cut is at monitor_seq=2. A live /flows/:id replay (post-cut) carries deltas + an
    // auth header that MUST NOT render while seeking. A monitor segment stamped at seq=2 is in the
    // cut (shows); one stamped at seq=3 is POST-cut (hidden).
    const started = 1_700_000_000_000;
    seedFlows([makeFlow({ api_call_id: 'api_cut', response_id: 'resp_cut', status: 'open', started_ms: started })]);
    // Cursor reflects the live monitor_seq the cut will freeze at.
    act(() => dashboardStore.getState().setCursor('monitor_seq', 2));
    // Two monitor segments: one AT the cut (seq 2, shown), one AFTER (seq 3, hidden).
    act(() => dashboardStore.getState().pushMonitor({ type: 'segment_append', response_id: 'resp_cut', segment: { timestamp_ms: 10, kind: 'output', text: 'IN-CUT' } }, 2));
    act(() => dashboardStore.getState().pushMonitor({ type: 'segment_append', response_id: 'resp_cut', segment: { timestamp_ms: 20, kind: 'output', text: 'POST-CUT' } }, 3));
    const liveDetail: FlowDetailDto = {
      flow_seq: 1, api_call_id: 'api_cut', response_id: 'resp_cut', status: 'open',
      started_ms: started, inbound_headers: { authorization: 'Bearer LEAK' },
      inbound_body: { model: 'gpt-4o' }, normalized: { model: 'm' }, upstream_body: { model: 'm' },
      deltas: [{ sequence: 1, kind: 'response.output_text.delta', payload: { text: 'REST-LEAK' }, ts_ms: 5 }],
    };
    const { getByTestId, getByRole, queryClient } = renderWithQuery(<FlowDetail apiCallId="api_cut" onClose={noop} />);
    act(() => queryClient.setQueryData(['flows', 'api_cut'], liveDetail));
    act(() => dashboardStore.getState().enterSeek(started + 1_000));
    await waitFor(() => expect(document.querySelector('[data-testid="seek-badge"]')).toBeTruthy());

    // Deltas: the in-cut monitor segment shows; the post-cut segment and the REST replay do NOT.
    const deltas = getByTestId('deltas-panel').textContent ?? '';
    expect(deltas).toContain('IN-CUT');
    expect(deltas).not.toContain('POST-CUT');
    expect(deltas).not.toContain('REST-LEAK');
    // Headers: the live REST auth header is withheld while seeking (frozen cut has no headers).
    fireEvent.click(getByRole('tab', { name: 'Headers' }));
    expect(getByTestId('headers-empty')).toBeTruthy();
  });

  it('while seeking, an OUT-of-cut selection fetches no detail and shows evicted panes (finding 1)', async () => {
    // A selection NOT present in the frozen snapshot rows must not fetch /flows/:id at all. The
    // mock answers api_001's detail; selecting it while seeking with an EMPTY cut must still show
    // the evicted placeholders (no live body), proving the detail query was gated off.
    act(() => dashboardStore.getState().enterSeek(1_700_000_000_000));
    const { queryClient } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    // Even after a tick, no detail data lands (the query is disabled out-of-cut while seeking).
    await act(async () => { await Promise.resolve(); });
    expect(queryClient.getQueryData(['flows', 'api_001'])).toBeUndefined();
    const empties = document.querySelectorAll('[data-testid^="jsonpane-empty-"]');
    expect(empties.length).toBe(3);
  });

  it('a kill in-flight when seek begins does NOT roll back into the frozen store (finding 2)', async () => {
    // Dispatch a kill that will FAIL (403, no CSRF) so `onError` fires. BEFORE it resolves, enter
    // seek and swap in a frozen snapshot cut. The optimistic rollback must be SKIPPED (the live
    // epoch changed) — otherwise it would re-insert the killed flow's prior 'open' row into the
    // frozen cut, leaking live data past the seek boundary.
    const started = 1_700_000_000_000;
    resetWorld({ mock: true });
    authStore.getState().setMutationsEnabled(true);
    authStore.getState().setCsrfToken('mock-csrf-token');
    seedFlows([makeFlow({ api_call_id: 'api_001', response_id: 'resp_001', status: 'open', started_ms: started })]);
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    // Clear the CSRF AFTER the connection seeded it (getConnection re-seeds on build), so the kill
    // omits X-CSRF-Token → the mock answers 403 → the mutation's onError path runs.
    act(() => authStore.getState().setCsrfToken(null));
    document.cookie = 'llmconduit_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT';

    // Fire the kill (optimistic flip to cancelled in onMutate), THEN immediately enter seek + swap
    // the store to a frozen snapshot cut while the 403 is still in flight.
    fireEvent.click(getByTestId('kill-button'));
    expect(dashboardStore.getState().flows.get('api_001')?.status).toBe('cancelled');
    const frozenRow = makeFlow({ api_call_id: 'api_snap', status: 'completed', started_ms: started });
    act(() => dashboardStore.getState().applySnapshot({ cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 }, flows: [frozenRow], metrics: null, topology: null }));
    act(() => dashboardStore.getState().enterSeek(started + 1_000));

    // Let the 403 resolve. The epoch guard skips the rollback: the frozen cut keeps ONLY api_snap;
    // the killed flow's prior 'open' row is NOT re-inserted.
    await waitFor(() => expect(getByTestId('kill-forbidden')).toBeTruthy());
    expect(dashboardStore.getState().flows.has('api_snap')).toBe(true);
    expect(dashboardStore.getState().flows.has('api_001')).toBe(false);
    // Still frozen + only the snapshot row present.
    expect(dashboardStore.getState().connection).toBe('seeking');
    expect(dashboardStore.getState().flows.size).toBe(1);
  });

  it('a kill failing AFTER a live→seek→live round-trip does NOT re-insert the stale optimistic row (finding 1)', async () => {
    // THE case the connection STRING could not catch: a kill is dispatched while LIVE; before the
    // 403 resolves the app seeks then returns to LIVE. The string epoch is `'live'` at BOTH dispatch
    // and resolve, so the old guard would have re-run `upsertFlow(prev)` — re-inserting the killed
    // flow's prior 'open' row into the NEW live store. The monotonic generation advanced across the
    // round-trip, so the rollback is correctly skipped.
    const started = 1_700_000_000_000;
    resetWorld({ mock: true });
    authStore.getState().setMutationsEnabled(true);
    authStore.getState().setCsrfToken('mock-csrf-token');
    seedFlows([makeFlow({ api_call_id: 'api_001', response_id: 'resp_001', status: 'open', started_ms: started })]);
    const { getByTestId } = renderWithQuery(<FlowDetail apiCallId="api_001" onClose={noop} />);
    await waitFor(() => expect(getByTestId('kill-button')).toBeTruthy());
    // Drop the CSRF so the kill 403s and onError runs.
    act(() => authStore.getState().setCsrfToken(null));
    document.cookie = 'llmconduit_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT';

    // Fire the kill (optimistic flip), THEN round-trip live→seek→live while the 403 is in flight.
    fireEvent.click(getByTestId('kill-button'));
    expect(dashboardStore.getState().flows.get('api_001')?.status).toBe('cancelled');
    act(() => dashboardStore.getState().setConnection('seeking'));
    // Return to LIVE via a fresh snapshot that does NOT contain api_001 (it was killed server-side),
    // then flip back to 'live' — exactly the socket's resume path (commitSnapshot → setConnection).
    act(() => {
      dashboardStore.getState().applySnapshot({ cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 }, flows: [], metrics: null, topology: null });
      dashboardStore.getState().setConnection('live');
    });
    expect(dashboardStore.getState().connection).toBe('live');

    // Let the 403 resolve: the generation guard skips the rollback, so api_001 is NOT re-inserted.
    await waitFor(() => expect(getByTestId('kill-forbidden')).toBeTruthy());
    expect(dashboardStore.getState().flows.has('api_001')).toBe(false);
    expect(dashboardStore.getState().flows.size).toBe(0);
  });
});
