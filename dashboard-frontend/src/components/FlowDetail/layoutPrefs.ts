/**
 * Persisted FlowDetail layout-chrome flags (collapse state for the summary band, the bottom tab
 * drawer, and the deltas rail). Splitter SIZES are owned by react-resizable-panels'
 * `useDefaultLayout` (localStorage keys `react-resizable-panels:argus-flowdetail-*`); these are
 * the booleans that library doesn't model. Same-prefix keys (`argus-flowdetail-*`) so the test
 * harness `resetWorld` clears both families between tests.
 */
import { useCallback, useState } from 'react';

const PREFIX = 'argus-flowdetail-';

function read(key: string, initial: boolean): boolean {
  try {
    const raw = window.localStorage.getItem(key);
    return raw === null ? initial : raw === '1';
  } catch {
    // Storage unavailable (private mode / quota) — layout chrome falls back to defaults.
    return initial;
  }
}

export function usePersistedFlag(name: string, initial: boolean): [boolean, (v: boolean) => void] {
  const key = PREFIX + name;
  const [value, setValue] = useState<boolean>(() => read(key, initial));
  const set = useCallback(
    (v: boolean) => {
      setValue(v);
      try {
        window.localStorage.setItem(key, v ? '1' : '0');
      } catch {
        // Best-effort persistence — the in-memory state still applies for this mount.
      }
    },
    [key],
  );
  return [value, set];
}
