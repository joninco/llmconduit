import { act, cleanup, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { ScopeBar } from './ScopeBar';
import { flowFilterStore } from '../store/flowFilterStore';
import { dashboardStore } from '../store/dashboardStore';
import { makeFlow, renderWithQuery, resetWorld, seedFlows } from './testHarness';

beforeEach(() => {
  resetWorld();
  seedFlows([
    makeFlow({ api_call_id: 'a', status: 'completed' }),
    makeFlow({ api_call_id: 'b', status: 'failed' }),
  ]);
});
afterEach(cleanup);

describe('ScopeBar announcements', () => {
  it('announces filter completion but not unrelated live-row updates', async () => {
    const { getByTestId } = renderWithQuery(<ScopeBar />);
    expect(getByTestId('scope-announcement').textContent).toBe('');

    act(() => flowFilterStore.getState().setFilters({ status: 'failed', model: null, upstream: null, client: null }));
    await waitFor(() => expect(getByTestId('scope-announcement').textContent).toContain('Showing 1 of 1 flows'));
    const filterMessage = getByTestId('scope-announcement').textContent;

    act(() => dashboardStore.getState().upsertFlow(makeFlow({ api_call_id: 'c', status: 'failed' })));
    expect(getByTestId('scope-announcement').textContent).toBe(filterMessage);
  });
});
