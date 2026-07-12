import { describe, expect, it } from 'vitest';
import { makeFlow } from '../testHarness';
import { FLOW_SEARCH_MAX_CHARS, FLOW_SEARCH_MAX_TERMS, flowMatchesSearch } from './flowSearch';

describe('flowMatchesSearch — bounded multi-field flow lookup', () => {
  const flow = makeFlow({
    api_call_id: 'api_ABC123',
    response_id: 'resp_XYZ789',
    method: 'POST',
    uri: '/v1/messages',
    status: 'completed',
    model_requested: 'claude-sonnet',
    model_served: 'GLM-5.2-NVFP4',
    upstream_target: 'vllm-b',
    client_label: 'svc-checkout',
    client_source: 'configured_header',
    attempts: [
      {
        provider: 'vllm-a', model: 'GLM-primary', start_ms: 1, end_ms: 2,
        status: 'failed', error_class: 'timeout', failover_reason: 'provider_failed',
      },
      { provider: 'vllm-b', model: 'GLM-5.2-NVFP4', start_ms: 2, end_ms: 3, status: 'served' },
    ],
  });

  it('matches pasted request/response IDs case-insensitively', () => {
    expect(flowMatchesSearch(flow, 'abc123')).toBe(true);
    expect(flowMatchesSearch(flow, 'RESP_xyz789')).toBe(true);
  });

  it('ANDs terms across visible dimensions', () => {
    expect(flowMatchesSearch(flow, 'messages glm vllm-b checkout')).toBe(true);
    expect(flowMatchesSearch(flow, 'messages glm missing-provider')).toBe(false);
  });

  it('finds failed-primary and failover provenance, not only the provider that served', () => {
    expect(flowMatchesSearch(flow, 'vllm-a timeout provider_failed')).toBe(true);
    expect(flowMatchesSearch(flow, 'failover')).toBe(true);
  });

  it('supports the status words operators see and common aliases', () => {
    expect(flowMatchesSearch(flow, 'success')).toBe(true);
    expect(flowMatchesSearch(flow, '2xx')).toBe(true);
    expect(flowMatchesSearch(flow, '5xx')).toBe(false);
  });

  it('treats blank input as no-op and bounds query characters + term count', () => {
    expect(flowMatchesSearch(flow, '   ')).toBe(true);
    expect(flowMatchesSearch(flow, `${'x'.repeat(FLOW_SEARCH_MAX_CHARS)} abc123`)).toBe(false);
    expect(flowMatchesSearch(flow, `${Array(FLOW_SEARCH_MAX_TERMS).fill('glm').join(' ')} ignored`)).toBe(true);
  });
});
