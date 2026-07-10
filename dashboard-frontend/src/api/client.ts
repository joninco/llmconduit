/**
 * Typed REST client for the D13 endpoints. Cross-cutting behavior:
 *  - All reads carry the session cookie automatically (`credentials: 'include'`).
 *  - The kill POST attaches `X-CSRF-Token` (D7 double-submit; value from bootstrap/cookie).
 *  - A 401 on ANY fetch fires the `onUnauthorized` signal so the shell bounces to login.
 *
 * The client is transport-agnostic: pass a `fetchImpl` (default `globalThis.fetch`) and
 * a `csrfToken` getter so the mock backend + tests can inject their own.
 */
import type {
  CatalogEntry,
  FlowDetail,
  FlowsQuery,
  FlowsResponse,
  KillResponse,
  LoginRequest,
  MetricsResponse,
  OverviewQuery,
  OverviewResponse,
  SnapshotResponse,
  TopologyResponse,
} from './types';
import type { ContractValidator } from './contractValidator';
import {
  assertDashboardSchemaVersion,
  DashboardSchemaMismatchError,
  DASHBOARD_SCHEMA_HEADER,
} from './schemaVersion';
import { assertContract, DashboardContractError } from './validation';

export type FetchImpl = typeof fetch;
type RestValidatorName =
  | 'validateCatalog'
  | 'validateFlowDetail'
  | 'validateFlows'
  | 'validateKill'
  | 'validateMetrics'
  | 'validateOverview'
  | 'validateSnapshot'
  | 'validateTopology';
type RestValidatorsModule = typeof import('./generated/validators-rest');

let restValidatorsPromise: Promise<RestValidatorsModule> | null = null;

async function loadRestValidator(name: RestValidatorName): Promise<ContractValidator<unknown>> {
  // Share one fully-evaluated module promise across concurrent startup queries. Selecting each
  // named export explicitly also keeps Vite/Rollup from trying to infer a computed namespace read
  // while route chunks race to import this large generated module.
  const validators = await (restValidatorsPromise ??= import('./generated/validators-rest'));
  let validator: ContractValidator<unknown>;
  switch (name) {
    case 'validateCatalog': validator = validators.validateCatalog; break;
    case 'validateFlowDetail': validator = validators.validateFlowDetail; break;
    case 'validateFlows': validator = validators.validateFlows; break;
    case 'validateKill': validator = validators.validateKill; break;
    case 'validateMetrics': validator = validators.validateMetrics; break;
    case 'validateOverview': validator = validators.validateOverview; break;
    case 'validateSnapshot': validator = validators.validateSnapshot; break;
    case 'validateTopology': validator = validators.validateTopology; break;
  }
  if (typeof validator !== 'function') {
    throw new DashboardContractError(name, [`generated validator export ${name} is unavailable`]);
  }
  return validator;
}

/** Raised when a fetch returns 401; the shell listens for this to bounce to login. */
export class UnauthorizedError extends Error {
  constructor() {
    super('unauthorized');
    this.name = 'UnauthorizedError';
  }
}

export interface DashboardClientOptions {
  /** Base path the API is mounted under. Default `/dashboard/api`. */
  basePath?: string;
  /** Injected fetch (mock/test). Default `globalThis.fetch`. */
  fetchImpl?: FetchImpl;
  /** Returns the current double-submit CSRF token (read from cookie/bootstrap). */
  getCsrfToken?: () => string | null;
  /** Fired on ANY 401 so the app can bounce to the login shell. */
  onUnauthorized?: () => void;
  /** Fired when a persistent schema/root-contract mismatch must replace normal query UI. */
  onFatal?: (error: DashboardSchemaMismatchError | DashboardContractError) => void;
}

export class DashboardClient {
  private readonly basePath: string;
  private readonly fetchImpl: FetchImpl;
  private readonly getCsrfToken: () => string | null;
  private readonly onUnauthorized: (() => void) | undefined;
  private readonly onFatal: DashboardClientOptions['onFatal'];

  constructor(opts: DashboardClientOptions = {}) {
    this.basePath = opts.basePath ?? '/dashboard/api';
    // Bind to globalThis so the default impl isn't called with a `this` of the class.
    this.fetchImpl = opts.fetchImpl ?? ((...a: Parameters<FetchImpl>) => globalThis.fetch(...a));
    this.getCsrfToken = opts.getCsrfToken ?? (() => null);
    this.onUnauthorized = opts.onUnauthorized;
    this.onFatal = opts.onFatal;
  }

  private async request<T>(
    path: string,
    validatorName: RestValidatorName,
    init?: RequestInit,
  ): Promise<T> {
    const res = await this.fetchImpl(`${this.basePath}${path}`, {
      credentials: 'include',
      ...init,
    });
    if (res.status === 401) {
      // Bounce-to-login signal: notify, then throw so callers stop.
      this.onUnauthorized?.();
      throw new UnauthorizedError();
    }
    if (!res.ok) {
      throw new Error(`${init?.method ?? 'GET'} ${path} failed: ${res.status}`);
    }
    try {
      assertDashboardSchemaVersion(res.headers.get(DASHBOARD_SCHEMA_HEADER), `REST ${path}`);
    } catch (error) {
      // The first mismatch already requested a hard reload. Only a persistent mismatch replaces
      // the shell with the explicit upgrade screen.
      if (error instanceof DashboardSchemaMismatchError && !error.reloadRequested) {
        this.onFatal?.(error);
      }
      throw error;
    }
    // 204/empty bodies decode to `undefined as T` at the call sites that allow it.
    const text = await res.text();
    let parsed: unknown;
    try {
      parsed = text ? JSON.parse(text) : undefined;
    } catch (error) {
      const contractError = new DashboardContractError(path, [
        error instanceof Error ? error.message : 'invalid JSON',
      ]);
      this.onFatal?.(contractError);
      throw contractError;
    }
    // REST validation is still mandatory before cache/store entry, but its generated code is
    // loaded on first API use rather than inflating the HTML's initial module graph.
    try {
      const validator = await loadRestValidator(validatorName);
      return assertContract(path, validator, parsed) as T;
    } catch (error) {
      if (error instanceof DashboardContractError) this.onFatal?.(error);
      throw error;
    }
  }

  // -- Auth -----------------------------------------------------------------

  /** `POST /dashboard/login` — note: login lives at /dashboard, NOT under /api. */
  async login(body: LoginRequest): Promise<void> {
    const res = await this.fetchImpl('/dashboard/login', {
      method: 'POST',
      credentials: 'include',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      throw new Error(`login failed: ${res.status}`);
    }
  }

  /** `POST /dashboard/logout` — clears the session cookie. */
  async logout(): Promise<void> {
    await this.fetchImpl('/dashboard/logout', { method: 'POST', credentials: 'include' });
  }

  /**
   * Lightweight protected-endpoint probe (finding 7): a cheap GET used by the WS layer
   * after a transient drop to decide reconnect-vs-logout. Returns `true` if the session is
   * still valid, `false` ONLY on a `401`. It does NOT fire `onUnauthorized` itself (the
   * caller decides); a non-401 error (network) resolves `true` so a blip reconnects rather
   * than logging the user out. Probes `/metrics` (a small, always-present read).
   */
  async probeAuth(): Promise<boolean> {
    try {
      const res = await this.fetchImpl(`${this.basePath}/metrics`, { credentials: 'include' });
      return res.status !== 401;
    } catch {
      // Network failure ≠ auth failure: stay logged in, let the socket reconnect.
      return true;
    }
  }

  // -- Reads (cursor-bearing) ----------------------------------------------

  flows(query: FlowsQuery = {}): Promise<FlowsResponse> {
    const qs = buildQuery(query);
    return this.request<FlowsResponse>(`/flows${qs}`, 'validateFlows');
  }

  flowDetail(id: string): Promise<FlowDetail> {
    return this.request<FlowDetail>(`/flows/${encodeURIComponent(id)}`, 'validateFlowDetail');
  }

  metrics(): Promise<MetricsResponse> {
    return this.request<MetricsResponse>('/metrics', 'validateMetrics');
  }

  overview(query: OverviewQuery): Promise<OverviewResponse> {
    return this.request<OverviewResponse>(`/overview${buildQuery(query)}`, 'validateOverview');
  }

  topology(): Promise<TopologyResponse> {
    return this.request<TopologyResponse>('/topology', 'validateTopology');
  }

  /** Bare array — no cursor (D13: static-ish catalog read). */
  catalog(): Promise<CatalogEntry[]> {
    return this.request<CatalogEntry[]>('/catalog', 'validateCatalog');
  }

  snapshot(atMs: number): Promise<SnapshotResponse> {
    return this.request<SnapshotResponse>(
      `/snapshot?at=${encodeURIComponent(String(atMs))}`,
      'validateSnapshot',
    );
  }

  // -- Mutation (CSRF-gated) ------------------------------------------------

  /** `POST /flows/:id/kill` — attaches `X-CSRF-Token` (D7). */
  kill(id: string): Promise<KillResponse> {
    const csrf = this.getCsrfToken();
    const headers: Record<string, string> = {};
    if (csrf) headers['X-CSRF-Token'] = csrf;
    return this.request<KillResponse>(
      `/flows/${encodeURIComponent(id)}/kill`,
      'validateKill',
      {
        method: 'POST',
        headers,
      },
    );
  }
}

/** Serializes a flows query into a `?a=b&c=d` string, dropping undefined values. */
function buildQuery(query: FlowsQuery | OverviewQuery): string {
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v !== undefined && v !== null) params.set(k, String(v));
  }
  const s = params.toString();
  return s ? `?${s}` : '';
}

/**
 * Reads the double-submit CSRF token from a non-HttpOnly cookie (D7). The Rust shell
 * sets `csrf_token` in both a cookie and the SPA bootstrap; this reads the cookie form.
 */
export function readCsrfCookie(cookieName = 'llmconduit_csrf'): string | null {
  if (typeof document === 'undefined') return null;
  const match = document.cookie.split('; ').find((c) => c.startsWith(`${cookieName}=`));
  return match ? decodeURIComponent(match.slice(cookieName.length + 1)) : null;
}
