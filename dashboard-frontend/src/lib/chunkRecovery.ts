const RECOVERY_KEY = 'argus:chunk-recovery:v1';
const MARKER_TTL_MS = 5 * 60_000;

const CHUNK_PATTERNS = [
  /chunkloaderror/i,
  /loading (?:css )?chunk [\w-]+ failed/i,
  /failed to fetch dynamically imported module/i,
  /error loading dynamically imported module/i,
  /importing a module script failed/i,
  /unable to preload css/i,
  /dynamically imported module/i,
];

export function isChunkLoadError(error: unknown): boolean {
  const candidate = error as { name?: unknown; message?: unknown } | null;
  const text = [candidate?.name, candidate?.message, String(error ?? '')]
    .filter((value): value is string => typeof value === 'string')
    .join(' ');
  return CHUNK_PATTERNS.some((pattern) => pattern.test(text));
}

interface RecoveryMarker {
  href: string;
  at: number;
}

function marker(storage: Storage, now: number): RecoveryMarker | null {
  try {
    const parsed = JSON.parse(storage.getItem(RECOVERY_KEY) ?? 'null') as Partial<RecoveryMarker> | null;
    if (!parsed || typeof parsed.href !== 'string' || typeof parsed.at !== 'number') return null;
    if (now - parsed.at > MARKER_TTL_MS) {
      storage.removeItem(RECOVERY_KEY);
      return null;
    }
    return { href: parsed.href, at: parsed.at };
  } catch {
    storage.removeItem(RECOVERY_KEY);
    return null;
  }
}

/** Returns true only for the first chunk failure at this exact route/query URL. */
export function armChunkRecovery(storage: Storage, href: string, now = Date.now()): boolean {
  const previous = marker(storage, now);
  if (previous?.href === href) return false;
  try {
    storage.setItem(RECOVERY_KEY, JSON.stringify({ href, at: now } satisfies RecoveryMarker));
    return true;
  } catch {
    return false;
  }
}

/** Clear the loop guard only after the lazy route has committed successfully. */
export function clearChunkRecovery(storage: Storage): void {
  try {
    storage.removeItem(RECOVERY_KEY);
  } catch {
    // Storage can be unavailable in privacy-hardened contexts; recovery still degrades safely.
  }
}

export const chunkRecoveryStorageKey = RECOVERY_KEY;
