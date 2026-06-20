/**
 * Centralized design tokens — the SINGLE source of truth for the dark-ops palette,
 * typography, spacing, and motion. Tailwind's theme (tailwind.config.ts) and the
 * runtime CSS variables (applyTokensToRoot) both derive from this file. Do NOT
 * hardcode hex colors anywhere else in the app.
 *
 * Spec: D9 §"Design system tokens".
 */

export const colors = {
  /** App background. */
  bg: '#0d0f12',
  /** Primary panel surface. */
  panel: '#16191e',
  /** Raised / nested panel surface. */
  panelRaised: '#1e2329',
  /** Hairline borders + grid lines. */
  line: '#2a313a',

  /** Provider status — healthy / serving. */
  statusHealthy: '#58d68d',
  /** Provider status — cooling / degraded. */
  statusCooling: '#f6c453',
  /** Provider status — down / error. */
  statusDown: '#ff6b6b',

  /** Primary accent (links, active, focus). */
  accent: '#6bb6ff',
  /** Meta / secondary accent (reasoning, cached, annotations). */
  meta: '#c58bd1',

  /** Foreground text. */
  text: '#e6e9ef',
  /** Muted / secondary text. */
  textMuted: '#8b93a1',

  /** Diff tints — additive (green-ish) and removed (red-ish) washes. */
  diffAddBg: 'rgba(88, 214, 141, 0.14)',
  diffAddText: '#9be8b8',
  diffRemoveBg: 'rgba(255, 107, 107, 0.14)',
  diffRemoveText: '#ff9d9d',
  diffContextBg: 'rgba(107, 182, 255, 0.07)',
} as const;

/**
 * Maps a provider health status string (the wire `ProviderStatus`) to its token color.
 * Centralized so every view colors status identically.
 */
export const STATUS_COLOR: Record<string, string> = {
  healthy: colors.statusHealthy,
  serving: colors.statusHealthy,
  cooling: colors.statusCooling,
  degraded: colors.statusCooling,
  down: colors.statusDown,
  error: colors.statusDown,
  unknown: colors.textMuted,
};

export const fonts = {
  /** System sans for the UI chrome. */
  ui: 'system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
  /** Monospace for payloads / JSON / code. */
  mono: 'ui-monospace, "JetBrains Mono", "SF Mono", Menlo, Consolas, monospace',
} as const;

/** 4px spacing grid. Index = multiples of the base unit. */
export const SPACING_UNIT = 4;
export const spacing = {
  px: '1px',
  0: '0px',
  1: '4px',
  2: '8px',
  3: '12px',
  4: '16px',
  5: '20px',
  6: '24px',
  8: '32px',
  10: '40px',
  12: '48px',
  16: '64px',
} as const;

export const radii = {
  sm: '4px',
  md: '6px',
  lg: '10px',
} as const;

/**
 * Returns true when the user has requested reduced motion. Views MUST consult this
 * (or the `prefers-reduced-motion` media query directly) to cut particles/animation.
 */
export function prefersReducedMotion(): boolean {
  if (typeof window === 'undefined' || !window.matchMedia) return false;
  return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/**
 * Flattens the token tree into CSS custom properties and applies them to a root
 * element (default: documentElement). index.css references these vars so Tailwind
 * utilities and raw CSS stay in sync with this file.
 */
export function applyTokensToRoot(root: HTMLElement = document.documentElement): void {
  const set = (k: string, v: string) => root.style.setProperty(k, v);
  set('--color-bg', colors.bg);
  set('--color-panel', colors.panel);
  set('--color-panel-raised', colors.panelRaised);
  set('--color-line', colors.line);
  set('--color-status-healthy', colors.statusHealthy);
  set('--color-status-cooling', colors.statusCooling);
  set('--color-status-down', colors.statusDown);
  set('--color-accent', colors.accent);
  set('--color-meta', colors.meta);
  set('--color-text', colors.text);
  set('--color-text-muted', colors.textMuted);
  set('--font-ui', fonts.ui);
  set('--font-mono', fonts.mono);
}

export const tokens = {
  colors,
  fonts,
  spacing,
  radii,
  SPACING_UNIT,
} as const;

export type Tokens = typeof tokens;
