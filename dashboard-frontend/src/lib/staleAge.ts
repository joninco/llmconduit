/**
 * Human duration for stale/idle ages (U8): `17m 52s`, `1h 2m`, `3d 4h`, `42s`. The previous
 * `MM:SS` clock read as a time-of-day ("last response 17:52 ago"), not a duration. Seconds are
 * dropped once hours are involved — nobody diagnoses with `1h 2m 3s` precision.
 */
export function formatStaleAge(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1_000));
  const seconds = total % 60;
  const minutes = Math.floor(total / 60) % 60;
  const hours = Math.floor(total / 3_600) % 24;
  const days = Math.floor(total / 86_400);
  if (days > 0) return `${days}d ${hours}h`;
  if (total >= 3_600) return `${hours}h ${minutes}m`;
  if (total >= 60) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}
