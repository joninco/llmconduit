/** Fixed-width stale clock (`MM:SS`, then `HH:MM:SS`, with a day prefix when needed). */
export function formatStaleAge(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1_000));
  const seconds = total % 60;
  const minutes = Math.floor(total / 60) % 60;
  const hours = Math.floor(total / 3_600) % 24;
  const days = Math.floor(total / 86_400);
  const pad = (value: number) => String(value).padStart(2, '0');
  const clock = total < 3_600
    ? `${pad(Math.floor(total / 60))}:${pad(seconds)}`
    : `${pad(hours)}:${pad(minutes)}:${pad(seconds)}`;
  return days > 0 ? `${days}d ${clock}` : clock;
}
