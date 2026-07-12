import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import {
  DEFAULT_SCOPE,
  navigate,
  readHashScope,
  resetHashScope,
  useHashDetail,
  useHashRoute,
} from './useHashRoute';
import { readFlowViewState, updateFlowViewState } from './flowViewState';

afterEach(() => {
  cleanup();
  resetHashScope();
});

describe('dashboard hash routing and shared scope', () => {
  it('round-trips window and every filter while preserving them on a detail deep link', () => {
    const scope = {
      window: 'h1' as const,
      status: 'failed' as const,
      model: 'model / unicode λ',
      upstream: 'provider?west',
      client: 'key-a&b',
    };

    act(() => navigate('flows', 'call/%?# λ', scope));
    expect(readHashScope()).toEqual(scope);
    expect(renderHook(() => useHashRoute()).result.current).toBe('flows');
    expect(renderHook(() => useHashDetail()).result.current).toBe('call/%?# λ');

    act(() => navigate('overview'));
    expect(readHashScope()).toEqual(scope);
    expect(renderHook(() => useHashRoute()).result.current).toBe('overview');
  });

  it('defaults unknown and empty routes to Overview and safely rejects malformed detail escapes', () => {
    act(() => {
      window.location.hash = '#/not-a-route?window=bogus&status=bogus';
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    expect(renderHook(() => useHashRoute()).result.current).toBe('overview');
    expect(readHashScope()).toEqual(DEFAULT_SCOPE);

    act(() => {
      window.location.hash = '#/flows/%E0%A4%A';
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    expect(renderHook(() => useHashRoute()).result.current).toBe('flows');
    expect(renderHook(() => useHashDetail()).result.current).toBeNull();
  });

  it('normalizes malformed query encoding and unknown scope enums without throwing', () => {
    act(() => {
      window.location.hash = '#/flows?window=forever&status=bogus&model=%E0%A4%A';
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    const scope = readHashScope();
    expect(scope.window).toBe('m1');
    expect(scope.status).toBeNull();
    // URLSearchParams may replace malformed bytes, but the parser stays total and never admits an
    // out-of-contract window/status value.
    expect(typeof scope.model === 'string' || scope.model === null).toBe(true);
  });

  it('persists flow search and full-population sorting in the URL across detail navigation', () => {
    act(() => navigate('flows'));
    act(() => updateFlowViewState({ q: 'api 123', sort: 'latency', direction: 'asc' }));
    expect(readFlowViewState()).toEqual({ q: 'api 123', sort: 'latency', direction: 'asc' });
    expect(window.location.hash).toContain('q=api+123');
    expect(window.location.hash).toContain('sort=latency');
    act(() => navigate('flows', 'api/123'));
    expect(readFlowViewState()).toEqual({ q: 'api 123', sort: 'latency', direction: 'asc' });
  });
});
