/** Pure, bounded, view-local search over the body-free flow rows already loaded for Flows. */
import type { FlowSummary, FlowStatus } from '../../api/types';

/** Keep pasted/automated input from turning one keystroke into unbounded matching work. */
export const FLOW_SEARCH_MAX_CHARS = 256;
/** Multi-term lookup stays useful while bounding the per-row comparison fan-out. */
export const FLOW_SEARCH_MAX_TERMS = 8;

const STATUS_ALIASES: Record<FlowStatus, string> = {
  open: 'open running streaming live',
  completed: 'completed complete success successful 2xx',
  failed: 'failed failure error 5xx',
  cancelled: 'cancelled canceled cancel 499',
};

function searchTerms(query: string): string[] {
  return query
    .slice(0, FLOW_SEARCH_MAX_CHARS)
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, FLOW_SEARCH_MAX_TERMS);
}

/**
 * Search the identifiers operators paste plus the dimensions they can see in the table/detail.
 * Every term must match, but terms may match different fields (`api_123 timeout vllm-a`). Attempt
 * fields make failed-primary/failover requests findable even when another provider ultimately served.
 */
export function flowMatchesSearch(flow: FlowSummary, query: string): boolean {
  const terms = searchTerms(query);
  if (terms.length === 0) return true;

  const fields = [
    flow.api_call_id,
    flow.response_id,
    flow.method,
    flow.uri,
    flow.status,
    STATUS_ALIASES[flow.status],
    flow.model_requested,
    flow.model_served,
    flow.upstream_target,
    flow.client_label,
    flow.client_source,
    flow.terminal_reason,
    flow.model_requested && flow.model_served && flow.model_requested !== flow.model_served
      ? 'failover fo'
      : null,
    ...(flow.attempts ?? []).flatMap((attempt) => [
      attempt.provider,
      attempt.model,
      attempt.status,
      attempt.error_class,
      attempt.failover_reason,
    ]),
  ]
    .filter((value): value is string => typeof value === 'string' && value.length > 0)
    .map((value) => value.toLowerCase());

  return terms.every((term) => fields.some((field) => field.includes(term)));
}
