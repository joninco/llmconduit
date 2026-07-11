import { describe, expect, it } from 'vitest';
import type { BackendProviderMetrics } from '../../api/types';
import { ENGINE_DASH, enginePercent, engineStatusText, engineValue } from './backendMetrics';

function metrics(status: BackendProviderMetrics['status']): BackendProviderMetrics {
  return {
    engine_kind: 'vllm', status, coverage: 'full', last_success_ms: 5_000,
    instant: {},
    windows: {
      m1: { samples: 0, histograms: {} },
      m5: { samples: 0, histograms: {} },
      h1: { samples: 0, histograms: {} },
    },
  };
}

describe('backend metrics presentation', () => {
  it('keeps missing and warming values unavailable', () => {
    expect(engineValue(undefined)).toBe(ENGINE_DASH);
    expect(enginePercent(null)).toBe(ENGINE_DASH);
    expect(engineStatusText(metrics('warming'), 10_000)).toBe('warming');
  });

  it('labels stale values with their age and bounded errors', () => {
    expect(engineStatusText(metrics('stale'), 17_500)).toBe('stale · 12s old');
    expect(engineStatusText({ ...metrics('error'), last_error_class: 'timeout' }, 10_000)).toBe('error · timeout');
  });

  it('distinguishes partial fresh coverage', () => {
    expect(engineStatusText({ ...metrics('fresh'), coverage: 'partial' }, 10_000)).toBe('fresh · partial');
    expect(enginePercent(0)).toBe('0.0%');
  });
});
