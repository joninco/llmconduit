import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  assertDashboardSchemaVersion,
  DashboardSchemaMismatchError,
  DASHBOARD_SCHEMA_VERSION,
} from './schemaVersion';

describe('dashboard schema negotiation', () => {
  beforeEach(() => sessionStorage.clear());

  it('requests exactly one hard reload for a persistent mismatch, then fails explicitly', () => {
    const reload = vi.fn();
    expect(() => assertDashboardSchemaVersion(1, 'test', reload)).toThrow(DashboardSchemaMismatchError);
    expect(reload).toHaveBeenCalledOnce();

    let repeated: DashboardSchemaMismatchError | null = null;
    try {
      assertDashboardSchemaVersion(1, 'test', reload);
    } catch (error) {
      repeated = error as DashboardSchemaMismatchError;
    }
    expect(reload).toHaveBeenCalledOnce();
    expect(repeated?.reloadRequested).toBe(false);
    expect(repeated?.message).toContain('upgrade required');
  });

  it('clears the mismatch marker after a compatible server is reached', () => {
    const reload = vi.fn();
    expect(() => assertDashboardSchemaVersion(1, 'test', reload)).toThrow();
    assertDashboardSchemaVersion(DASHBOARD_SCHEMA_VERSION, 'test', reload);
    expect(() => assertDashboardSchemaVersion(1, 'test', reload)).toThrow();
    expect(reload).toHaveBeenCalledTimes(2);
  });

  it('does not let a compatible bootstrap erase a repeated REST mismatch', () => {
    const reload = vi.fn();
    expect(() => assertDashboardSchemaVersion(1, 'REST /metrics', reload)).toThrow();
    expect(reload).toHaveBeenCalledOnce();

    // The reload necessarily validates the HTML bootstrap before REST starts. That unrelated
    // success must preserve the REST marker so the repeated mismatch renders the upgrade error.
    assertDashboardSchemaVersion(DASHBOARD_SCHEMA_VERSION, 'HTML bootstrap', reload);
    let repeated: DashboardSchemaMismatchError | null = null;
    try {
      assertDashboardSchemaVersion(1, 'REST /metrics', reload);
    } catch (error) {
      repeated = error as DashboardSchemaMismatchError;
    }
    expect(reload).toHaveBeenCalledOnce();
    expect(repeated?.reloadRequested).toBe(false);
  });
});
