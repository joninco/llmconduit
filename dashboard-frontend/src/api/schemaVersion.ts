/** Dashboard wire-version negotiation shared by bootstrap, REST, and WS. */

export const DASHBOARD_SCHEMA_VERSION = 3;
export const DASHBOARD_SCHEMA_HEADER = 'X-LLMConduit-Dashboard-Schema';

const RELOAD_MARKER = 'llmconduit.dashboard.schema-reload';

interface ReloadMarker {
  expected: number;
  received: string;
  source: string;
}

export class DashboardSchemaMismatchError extends Error {
  constructor(
    readonly received: string,
    readonly source: string,
    readonly reloadRequested: boolean,
  ) {
    super(
      reloadRequested
        ? `dashboard schema changed (${received}); reloading`
        : `dashboard upgrade required: expected schema ${DASHBOARD_SCHEMA_VERSION}, received ${received} from ${source}`,
    );
    this.name = 'DashboardSchemaMismatchError';
  }
}

/**
 * Verify one server version. The first mismatch in a browser session requests a
 * hard reload so a newly-deployed embedded bundle can replace the stale SPA. If
 * the same mismatch survives that reload, fail explicitly instead of looping.
 */
export function assertDashboardSchemaVersion(
  received: unknown,
  source: string,
  reload: () => void = () => window.location.reload(),
): void {
  const value = String(received ?? 'missing');
  if (value === String(DASHBOARD_SCHEMA_VERSION)) {
    // Do not let an unrelated compatible surface erase evidence that another endpoint caused the
    // hard reload. Example: REST /metrics mismatches, reload → compatible HTML bootstrap → the same
    // REST mismatch. Clearing on the bootstrap would turn that into an infinite reload loop. A
    // successful check of the SAME source proves the transient mismatch is gone; a different
    // expected version proves a newly loaded SPA superseded the old marker.
    const prior = readReloadMarker();
    if (prior && (prior.expected !== DASHBOARD_SCHEMA_VERSION || prior.source === source)) {
      safeSessionRemove(RELOAD_MARKER);
    }
    return;
  }

  const marker: ReloadMarker = {
    expected: DASHBOARD_SCHEMA_VERSION,
    received: value,
    source,
  };
  const prior = readReloadMarker();
  if (!prior || prior.expected !== marker.expected || prior.received !== marker.received || prior.source !== marker.source) {
    safeSessionSet(RELOAD_MARKER, JSON.stringify(marker));
    reload();
    throw new DashboardSchemaMismatchError(value, source, true);
  }
  throw new DashboardSchemaMismatchError(value, source, false);
}

function readReloadMarker(): ReloadMarker | null {
  const raw = safeSessionGet(RELOAD_MARKER);
  if (!raw) return null;
  try {
    const marker = JSON.parse(raw) as Partial<ReloadMarker>;
    return typeof marker.expected === 'number'
      && typeof marker.received === 'string'
      && typeof marker.source === 'string'
      ? marker as ReloadMarker
      : null;
  } catch {
    return null;
  }
}

function safeSessionGet(key: string): string | null {
  try {
    return typeof sessionStorage === 'undefined' ? null : sessionStorage.getItem(key);
  } catch {
    return null;
  }
}

function safeSessionSet(key: string, value: string): void {
  try {
    if (typeof sessionStorage !== 'undefined') sessionStorage.setItem(key, value);
  } catch {
    // Storage can be disabled; the explicit thrown error still prevents a bad connection.
  }
}

function safeSessionRemove(key: string): void {
  try {
    if (typeof sessionStorage !== 'undefined') sessionStorage.removeItem(key);
  } catch {
    // No-op when storage is unavailable.
  }
}
