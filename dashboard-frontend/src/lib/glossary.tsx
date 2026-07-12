/**
 * Glossary (U10) — the single source of definitions for the dashboard's operator jargon.
 * Dense provenance vocabulary (M1, terminal, server cut, seq, MEASURED/DERIVED…) is
 * load-bearing but has no on-ramp; every wired term gets a hover/focus definition via the
 * `<Term>` component below (an `<abbr>` — keyboard focusable, exposed to assistive tech).
 */
import type { ReactNode } from 'react';

export const GLOSSARY = {
  m1: 'The 1-minute analytics window. m5 = 5 minutes, h1 = 1 hour. Rollups aggregate terminal flows inside this window.',
  m5: 'The 5-minute analytics window.',
  h1: 'The 1-hour analytics window.',
  terminal: 'A flow that reached a final outcome — succeeded, failed, or cancelled. Open (in-flight) flows are not terminal and are excluded from terminal rollups.',
  rollup: 'A server-computed aggregate over the terminal flows in the selected window and scope (never recomputed browser-side).',
  'server cut': 'The server-authored instant this aggregate was computed at. All numbers on the surface share this one cut — they are mutually consistent as of that moment.',
  seq: 'The monotonic publication sequence number of this aggregate — later cuts have higher seq. Useful when correlating with logs.',
  measured: 'Directly counted from observed traffic — no modelling involved.',
  derived: 'Computed from measured samples or counter deltas (e.g. percentiles, rates).',
  estimated: 'Modelled via the configured price table or another approximation — labelled so it is never mistaken for a measurement.',
  unavailable: 'Not measurable in this window (zero denominator). Rendered as “—”, never a fabricated 0.',
  attempt: 'One dispatch to one provider. A flow with failover records several attempts; flow rollups count final client outcomes, provider attempts count every dispatch.',
  flow: 'One end-to-end client request through the gateway, from accept to terminal outcome.',
} as const;

export type GlossaryTerm = keyof typeof GLOSSARY;

/**
 * Inline glossary term: renders children (or the term itself) with a dotted underline and the
 * definition as a native tooltip + accessible description. Focusable, so keyboard users get it.
 */
export function Term({ term, children }: { term: GlossaryTerm; children?: ReactNode }) {
  // aria-label is ALWAYS derived (falling back to the term key for non-string children) so a
  // keyboard/screen-reader user gets the definition on focus even when the child is an element
  // (review MED — the native title alone never surfaces on keyboard focus).
  const label = typeof children === 'string' ? children : term;
  return (
    <abbr
      className="cursor-help no-underline [text-decoration-line:underline] [text-decoration-style:dotted] [text-underline-offset:2px] decoration-text-muted/60"
      title={GLOSSARY[term]}
      aria-label={`${label}: ${GLOSSARY[term]}`}
      tabIndex={0}
      data-testid={`term-${term.replace(/\s+/g, '-')}`}
    >
      {children ?? term}
    </abbr>
  );
}
