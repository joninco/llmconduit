import { act, render, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { App, DashboardAnnouncements, RouteContent, RouteErrorBoundary } from './App';
import { dashboardStore } from './store/dashboardStore';
import { makeFlow, resetWorld } from './components/testHarness';

function Bomb(): never {
  throw new Error('route exploded');
}

function SafeRoute() {
  return <div>safe route</div>;
}

beforeEach(() => resetWorld());
afterEach(() => vi.restoreAllMocks());

describe('route-level failure handling', () => {
  it('contains a render failure and presents an explicit retry surface', () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const { getByRole } = render(
      <RouteErrorBoundary><Bomb /></RouteErrorBoundary>,
    );
    expect(getByRole('alert').textContent).toContain('route exploded');
    expect(getByRole('button', { name: 'Retry' })).toBeTruthy();
  });

  it('resets a failed boundary when navigation changes the route key', () => {
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const rendered = render(<RouteContent ActiveView={Bomb} routeKey="flows" />);
    expect(rendered.getByRole('alert')).toBeTruthy();
    rendered.rerender(<RouteContent ActiveView={SafeRoute} routeKey="topology" />);
    expect(rendered.getByText('safe route')).toBeTruthy();
  });

  it('renders the explicit upgrade screen for a fatal root contract', () => {
    act(() => dashboardStore.getState().setFatalError('dashboard contract validation failed: /metrics'));
    const { getByRole } = render(<App />);
    expect(getByRole('alert').textContent).toContain('Dashboard upgrade required');
    expect(getByRole('alert').textContent).toContain('contract validation failed');
  });
});

describe('restrained dashboard announcements', () => {
  it('distinguishes initial connection from stale reconnect and announces stream boundaries', async () => {
    const { getByText } = render(<DashboardAnnouncements />);
    act(() => dashboardStore.getState().setConnection('connecting'));
    await waitFor(() => expect(getByText('Dashboard connecting.')).toBeTruthy());

    act(() => {
      dashboardStore.getState().upsertFlow(makeFlow({ api_call_id: 'api_live', status: 'open' }));
      dashboardStore.getState().setConnection('live');
    });
    await waitFor(() => expect(getByText('Stream api_live started.')).toBeTruthy());
    act(() => dashboardStore.getState().setConnection('connecting'));
    await waitFor(() => expect(getByText('Dashboard reconnecting. Displayed data may be stale.')).toBeTruthy());

    act(() => dashboardStore.getState().upsertFlow(makeFlow({ revision: 2, api_call_id: 'api_live', status: 'completed' })));
    await waitFor(() => expect(getByText('Stream api_live completed.')).toBeTruthy());
  });
});
