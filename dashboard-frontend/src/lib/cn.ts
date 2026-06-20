/** Tiny classnames joiner (shadcn-style `cn`) — no dependency needed for the scaffold. */
export function cn(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(' ');
}
