/**
 * `pickAttempts` — the SHARED "non-empty attempts wins, else backfill" merge rule.
 *
 * `attempts[]` (gap 03 / 11) CANNOT be merged with the scalar `fresh ?? fallback` rule the phase
 * epochs use, because an EMPTY array is the SERIALIZATION for "no attempt recorded yet": a snapshot
 * summary (and the store's `flow_status` projection) emits `attempts: []` rather than OMITTING the
 * field. `??` only falls back on `null`/`undefined`, so it would treat that `[]` as authoritative
 * and DROP a populated trace coming from the other source (gap 10b review round 2):
 *  - useFlowRows merge: a live/snapshot row's `[]` would block the REST `/flows` backfill.
 *  - FlowDetail spine merge: a live `[]` would suppress the populated REST detail attempts.
 *  - patchFlowStatus reducer: a LATER frame's `[]` would erase an earlier-known non-empty trace.
 *
 * Honest semantics (NON-EMPTY wins, otherwise backfill; both empty/absent ⇒ honestly absent):
 *  - a NON-EMPTY `fresh` list is the freshest authoritative trace ⇒ it wins.
 *  - else (`fresh` absent OR `[]`) fall back to `fallback` — which may be the populated trace.
 *  - if BOTH are empty/absent ⇒ `undefined` (truly no attempts: the stepper renders nothing / the
 *    latency model's wire TTFB is `unavailable`, per spec 11) — NEVER a fabricated attempt entry.
 *
 * Normalizing `[]`/`null` → `undefined` matches the "unmeasured ⇒ absent" convention the spine
 * fields follow, so a merged row's `attempts` is either a non-empty list or absent.
 */
import type { Attempt } from './types';

export function pickAttempts(
  fresh: Attempt[] | null | undefined,
  fallback: Attempt[] | null | undefined,
): Attempt[] | undefined {
  if (fresh && fresh.length > 0) return fresh;
  if (fallback && fallback.length > 0) return fallback;
  return undefined;
}

/**
 * EMPTY and ABSENT are the SAME "no trace" state for an equality check (a snapshot's `[]` and a
 * normalized `undefined` must not count as a difference, else a referential-stability check churns
 * a new object every render). Otherwise compare by REFERENCE: a backfilled non-empty list is a new
 * reference vs the prior `[]`/absent, so a real trace change yields `false` (a re-render is needed).
 */
export function sameAttempts(a: Attempt[] | null | undefined, b: Attempt[] | null | undefined): boolean {
  const aEmpty = !a || a.length === 0;
  const bEmpty = !b || b.length === 0;
  if (aEmpty && bEmpty) return true;
  return a === b;
}
