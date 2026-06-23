import { describe, it, expect } from 'vitest';
import {
  failureTaxonomy,
  capturedErrorBody,
  fmtFailureRate,
  UNAVAILABLE,
  UNCLASSIFIED_REASON_KEY,
} from './failureTaxonomy';
import type { Attempt, FlowSummary, FlowUpstreamResponse } from '../../api/types';

/** A minimal FlowSummary; override per-test. Defaults to a COMPLETED (non-failing) flow. */
function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: 'api_x',
    method: 'POST',
    uri: '/v1/responses',
    model_requested: 'gpt-4o',
    model_served: 'gpt-4o',
    upstream_target: 'openai',
    status: 'completed',
    started_ms: 1_000,
    cost_confidence: 'confident',
    ...over,
  };
}

const failedAttempt = (over: Partial<Attempt> = {}): Attempt => ({
  provider: 'openai',
  model: 'gpt-4o',
  start_ms: 10,
  end_ms: 20,
  status: 'failed',
  error_class: 'http_status',
  failover_reason: 'terminal_no_failover',
  ...over,
});

describe('fmtFailureRate (don\'t-lie-with-zeros)', () => {
  it('renders a MEASURED-base 0 as 0% (distinct from the unavailable —)', () => {
    expect(fmtFailureRate(0)).toBe('0%');
    expect(fmtFailureRate(4.2)).toBe('4.2%');
    expect(fmtFailureRate(33.333)).toBe('33%');
    expect(fmtFailureRate(null)).toBe(UNAVAILABLE);
    expect(fmtFailureRate(Number.NaN)).toBe(UNAVAILABLE);
  });
});

describe('failureTaxonomy — empty window (don\'t-lie-with-zeros)', () => {
  it('a zero-sample window is UNAVAILABLE — every rate is —, NEVER 0%', () => {
    const t = failureTaxonomy([]);
    expect(t.available).toBe(false);
    expect(t.totalFlows).toBe(0);
    expect(t.totalFailed).toBe(0);
    expect(t.overallErrorRateText).toBe(UNAVAILABLE);
    expect(t.overallErrorRateText).not.toBe('0%');
    expect(t.overallQuality).toBe('unavailable');
    expect(t.groups).toEqual([]);
  });

  it('null/undefined input is treated as empty (unavailable)', () => {
    expect(failureTaxonomy(null).available).toBe(false);
    expect(failureTaxonomy(undefined).available).toBe(false);
  });
});

describe('failureTaxonomy — observed-but-no-failures (MEASURED 0%, distinct from unavailable)', () => {
  it('all-success window ⇒ available with a derived 0% overall and NO groups', () => {
    const t = failureTaxonomy([flow(), flow({ api_call_id: 'api_y' })]);
    expect(t.available).toBe(true);
    expect(t.totalFlows).toBe(2);
    expect(t.totalFailed).toBe(0);
    expect(t.overallErrorRatePct).toBe(0);
    expect(t.overallErrorRateText).toBe('0%'); // a measured-base zero — NOT —
    expect(t.overallQuality).toBe('derived');
    // The panel lists what is FAILING; a no-failure window has no groups (not a fabricated row).
    expect(t.groups).toEqual([]);
  });
});

describe('failureTaxonomy — grouping by reason × model/provider with a derived rate', () => {
  it('groups failures by provider|model and derives the error rate over the observed total', () => {
    const flows: FlowSummary[] = [
      // openai/gpt-4o: 1 failed (http_status) of 2 observed ⇒ 50%.
      flow({ api_call_id: 'a1', status: 'failed', terminal_reason: 'upstream 503', attempts: [failedAttempt({ error_class: 'http_status' })] }),
      flow({ api_call_id: 'a2', status: 'completed' }),
      // vllm-a/llama: 2 failed of 2 observed ⇒ 100%, two distinct BOUNDED gap-03 reasons.
      flow({ api_call_id: 'b1', status: 'failed', upstream_target: 'vllm-a', model_served: 'llama', terminal_reason: 'timeout', attempts: [failedAttempt({ provider: 'vllm-a', model: 'llama', error_class: 'timeout' })] }),
      flow({ api_call_id: 'b2', status: 'failed', upstream_target: 'vllm-a', model_served: 'llama', terminal_reason: 'upstream 500', attempts: [failedAttempt({ provider: 'vllm-a', model: 'llama', error_class: 'http_status' })] }),
    ];
    const t = failureTaxonomy(flows);
    expect(t.totalFlows).toBe(4);
    expect(t.totalFailed).toBe(3);
    expect(t.overallErrorRatePct).toBe(75);
    expect(t.overallErrorRateText).toBe('75%');

    // Ordered by failed-count desc: vllm-a (2) before openai (1).
    expect(t.groups.map((g) => g.key)).toEqual(['vllm-a|llama', 'openai|gpt-4o']);

    const vllm = t.groups[0]!;
    expect(vllm.provider).toBe('vllm-a');
    expect(vllm.model).toBe('llama');
    expect(vllm.total).toBe(2);
    expect(vllm.failed).toBe(2);
    expect(vllm.errorRatePct).toBe(100);
    expect(vllm.errorRateText).toBe('100%');
    // BOUNDED keys only (gap-03 classes) — never the free-form terminal_reason strings.
    expect(vllm.reasons.map((r) => r.key).sort()).toEqual(['class:http_status', 'class:timeout']);
    expect(vllm.reasons.every((r) => r.source === 'error_class')).toBe(true);

    const oai = t.groups[1]!;
    expect(oai.total).toBe(2);
    expect(oai.failed).toBe(1);
    expect(oai.errorRatePct).toBe(50);
    expect(oai.reasons).toEqual([
      { key: 'class:http_status', label: 'http status', source: 'error_class', count: 1 },
    ]);
  });

  it('coalesces identical BOUNDED reasons within a group and counts them (count >= 1)', () => {
    const flows: FlowSummary[] = [
      flow({ api_call_id: 'a1', status: 'failed', attempts: [failedAttempt({ error_class: 'http_status' })] }),
      flow({ api_call_id: 'a2', status: 'failed', attempts: [failedAttempt({ error_class: 'http_status' })] }),
      flow({ api_call_id: 'a3', status: 'failed', attempts: [failedAttempt({ error_class: 'timeout' })] }),
    ];
    const t = failureTaxonomy(flows);
    const g = t.groups[0]!;
    // Ordered by count desc: the doubled http_status first.
    expect(g.reasons).toEqual([
      { key: 'class:http_status', label: 'http status', source: 'error_class', count: 2 },
      { key: 'class:timeout', label: 'timeout', source: 'error_class', count: 1 },
    ]);
  });
});

describe('failureTaxonomy — BOUNDED reason resolution (review HIGH: no free-form key ever)', () => {
  it('derives the reason from the BOUNDED gap-03 error_class FIRST, even when terminal_reason is a free-form string', () => {
    // terminal_reason is a raw/free-form string AND a bounded attempt class exists ⇒ the CLASS wins
    // (never the arbitrary string as a key).
    const flows: FlowSummary[] = [
      flow({ api_call_id: 'a1', status: 'failed', terminal_reason: 'Service Unavailable: pool exhausted', attempts: [failedAttempt({ error_class: 'http_status' })] }),
    ];
    const g = failureTaxonomy(flows).groups[0]!;
    expect(g.reasons).toEqual([
      { key: 'class:http_status', label: 'http status', source: 'error_class', count: 1 },
    ]);
  });

  it('an UNKNOWN/free-form terminal_reason (no attempt class) groups under __unclassified__, NOT its own key (cardinality guard)', () => {
    const flows: FlowSummary[] = [
      // Two DIFFERENT free-form strings — they must COLLAPSE into one __unclassified__ bucket, not 2 keys.
      flow({ api_call_id: 'a1', status: 'failed', upstream_target: 'openai', model_served: 'gpt-4o', terminal_reason: 'upstream 503 body: foo', attempts: [] }),
      flow({ api_call_id: 'a2', status: 'failed', upstream_target: 'openai', model_served: 'gpt-4o', terminal_reason: 'connection reset by peer xyz', attempts: [] }),
    ];
    const g = failureTaxonomy(flows).groups[0]!;
    expect(g.reasons.length).toBe(1); // collapsed — NOT one key per arbitrary string
    expect(g.reasons[0]!.key).toBe(UNCLASSIFIED_REASON_KEY);
    expect(g.reasons[0]!.source).toBe('unclassified');
    expect(g.reasons[0]!.count).toBe(2);
    // The raw strings never appear as a key.
    expect(g.reasons.some((r) => r.key.includes('upstream 503') || r.key.includes('connection reset'))).toBe(false);
  });

  it('uses a WHITELISTED bounded terminal_reason (content_filter) when there is no attempt class', () => {
    const flows: FlowSummary[] = [
      flow({ api_call_id: 'a1', status: 'failed', terminal_reason: 'content_filter', attempts: [] }),
    ];
    const g = failureTaxonomy(flows).groups[0]!;
    expect(g.reasons).toEqual([
      { key: 'terminal:content_filter', label: 'content filter', source: 'terminal_reason', count: 1 },
    ]);
  });

  it('uses the LAST failed attempt class (the leaf that terminated the turn)', () => {
    const flows: FlowSummary[] = [
      flow({
        api_call_id: 'a1',
        status: 'failed',
        terminal_reason: null,
        attempts: [
          failedAttempt({ provider: 'vllm-b', error_class: 'http_status' }),
          failedAttempt({ provider: 'openai', error_class: 'timeout' }),
        ],
      }),
    ];
    const g = failureTaxonomy(flows).groups[0]!;
    expect(g.reasons[0]!.label).toBe('timeout');
    expect(g.reasons[0]!.source).toBe('error_class');
  });

  it('buckets a failure with NO terminal reason and no attempt class as "unclassified" (never invented)', () => {
    const flows: FlowSummary[] = [flow({ api_call_id: 'a1', status: 'failed', terminal_reason: null, attempts: [] })];
    const g = failureTaxonomy(flows).groups[0]!;
    expect(g.reasons[0]!.key).toBe(UNCLASSIFIED_REASON_KEY);
    expect(g.reasons[0]!.label).toBe('unclassified');
  });

  it('ignores a non-failing flow\'s reason — only FAILED flows contribute reasons', () => {
    const flows: FlowSummary[] = [
      flow({ api_call_id: 'a1', status: 'completed', terminal_reason: 'response.completed' }),
      flow({ api_call_id: 'a2', status: 'cancelled', terminal_reason: 'client hung up' }),
    ];
    const t = failureTaxonomy(flows);
    // 2 observed, 0 FAILED (completed/cancelled are not `failed`) ⇒ a real measured 0% overall, no groups.
    expect(t.totalFlows).toBe(2);
    expect(t.totalFailed).toBe(0);
    expect(t.overallErrorRateText).toBe('0%');
    expect(t.groups).toEqual([]);
  });

  it('falls back to model_requested / — sentinels when served identity / provider is blank', () => {
    const flows: FlowSummary[] = [
      flow({ api_call_id: 'a1', status: 'failed', terminal_reason: 'x', model_served: null, upstream_target: null }),
    ];
    const g = failureTaxonomy(flows).groups[0]!;
    expect(g.provider).toBe(UNAVAILABLE); // no upstream_target ⇒ — sentinel
    expect(g.model).toBe('gpt-4o'); // falls back to model_requested
  });
});

describe('capturedErrorBody — capture-on vs capture-disabled (don\'t-lie-with-zeros)', () => {
  it('ABSENT upstream_response ⇒ unavailable ("capture disabled" — NOT "no error")', () => {
    const c = capturedErrorBody(undefined);
    expect(c.state).toBe('unavailable');
    expect(c.quality).toBe('unavailable');
    expect(c.body).toBeUndefined();
    expect(c.truncated).toBe(false);
    expect(c.detail).toMatch(/capture disabled|capture is OFF/i);
    // The wording explicitly distinguishes "capture disabled / unavailable" from "no error".
    expect(c.detail).toMatch(/NOT "no error"/);
  });

  it('null upstream_response is also unavailable', () => {
    expect(capturedErrorBody(null).state).toBe('unavailable');
  });

  it('PRESENT body ⇒ captured (measured), truncation flagged honestly', () => {
    const upstream: FlowUpstreamResponse = { body: { error: { message: 'rate limited' } }, truncated: false };
    const c = capturedErrorBody(upstream);
    expect(c.state).toBe('captured');
    expect(c.quality).toBe('measured');
    expect(c.body).toEqual({ error: { message: 'rate limited' } });
    expect(c.truncated).toBe(false);

    const truncated = capturedErrorBody({ body: 'partial…', truncated: true });
    expect(truncated.truncated).toBe(true);
    expect(truncated.detail).toMatch(/truncat/i);
  });

  it('an EMPTY captured body ("") is CAPTURED (present), distinct from absent', () => {
    const c = capturedErrorBody({ body: '', truncated: false });
    expect(c.state).toBe('captured');
    expect(c.body).toBe('');
  });

  it('a malformed object falls back to unavailable (never trusts a bad shape as "no error")', () => {
    // Missing `truncated` / missing `body` ⇒ not a valid FlowUpstreamResponse ⇒ unavailable.
    expect(capturedErrorBody({ body: 'x' } as unknown as FlowUpstreamResponse).state).toBe('unavailable');
    expect(capturedErrorBody({ truncated: true } as unknown as FlowUpstreamResponse).state).toBe('unavailable');
  });
});
