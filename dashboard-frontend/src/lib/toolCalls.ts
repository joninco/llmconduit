/**
 * Single source of truth for the backend's per-tool-call BOUNDARY rule, shared by every view that
 * renders streamed `tool` segments (the inspector's `DeltasPanel` and the theater's `riverModel`).
 *
 * A real function call streams its arguments as MANY adjacent `tool` fragments, and the backend
 * (`src/monitor.rs::append_function_call_delta`) coalesces same-kind text server-side AND introduces
 * each DISTINCT call with an explicit header line — `tool arguments <short_id>:` on its own line. So
 * a single coalesced `tool` run can hold several calls concatenated. Grouping by `kind` adjacency
 * alone therefore merges two back-to-back calls into one card; splitting on this boundary marker
 * yields one card PER call (D10 finding 5 / D12 R6). Both consumers MUST split on the same rule, so
 * the regex and the split live here once rather than being re-implemented per view.
 */

/**
 * The explicit per-call boundary the backend writes ahead of each distinct function call's argument
 * stream: `tool arguments <short_id>:` on its own line (monitor.rs). Splitting on it separates
 * concatenated calls into distinct entries.
 */
export const TOOL_CALL_BOUNDARY = /^tool arguments .*:\s*$/;

/**
 * Splits a coalesced `tool` text run into one entry per DISTINCT call, cutting on the backend's
 * `tool arguments <id>:` boundary line. The boundary line STARTS a new entry and is kept as that
 * entry's first line (so a label can still resolve the call id / the argument JSON that follows).
 * Text before the first boundary (a streamed call whose fragments carry no header) is its own entry,
 * so a single markerless tool run stays ONE entry — the split must not over-split (finding 2).
 *
 * Entries are trimmed; an all-whitespace run that produces no printable entry returns the original
 * text as a single entry so the run is never silently dropped.
 */
export function splitToolCallText(text: string): string[] {
  const lines = text.split('\n');
  const entries: string[] = [];
  let current: string[] = [];
  const flush = () => {
    if (current.length === 0) return;
    const joined = current.join('\n').trim();
    if (joined !== '') entries.push(joined);
    current = [];
  };
  for (const line of lines) {
    // A boundary line is the per-call header: it STARTS a new entry (flush the prior one) and is
    // kept as this entry's first line.
    if (TOOL_CALL_BOUNDARY.test(line)) flush();
    current.push(line);
  }
  flush();
  return entries.length > 0 ? entries : [text];
}
