/**
 * THE single source of the raw palette + font literals (finding 8). This module is
 * intentionally DOM-FREE (no `window`/`document`/`HTMLElement`) so it can be imported by
 * BOTH the app (tokens.ts, via the browser tsconfig) AND tailwind.config.ts (evaluated by
 * PostCSS under the Node tsconfig, which has no DOM lib). The hex/font strings appear ONLY
 * here; tokens.ts re-exports them as `colors`/`fonts` and derives CSS vars, and Tailwind
 * references `var(--color-*)`.
 */

export const PALETTE = {
  bg: '#0d0f12',
  panel: '#16191e',
  panelRaised: '#1e2329',
  line: '#2a313a',
  statusHealthy: '#58d68d',
  statusCooling: '#f6c453',
  statusDown: '#ff6b6b',
  accent: '#6bb6ff',
  meta: '#c58bd1',
  text: '#e6e9ef',
  textMuted: '#8b93a1',
  diffAddBg: 'rgba(88, 214, 141, 0.14)',
  diffAddText: '#9be8b8',
  diffRemoveBg: 'rgba(255, 107, 107, 0.14)',
  diffRemoveText: '#ff9d9d',
  diffContextBg: 'rgba(107, 182, 255, 0.07)',
} as const;

export const FONTS = {
  ui: 'system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
  mono: 'ui-monospace, "JetBrains Mono", "SF Mono", Menlo, Consolas, monospace',
} as const;

/** Tailwind color KEY → `var(--color-*)` reference (NOT hex). */
export const TAILWIND_COLOR_VARS: Record<string, string> = {
  bg: 'var(--color-bg)',
  panel: 'var(--color-panel)',
  'panel-raised': 'var(--color-panel-raised)',
  line: 'var(--color-line)',
  'status-healthy': 'var(--color-status-healthy)',
  'status-cooling': 'var(--color-status-cooling)',
  'status-down': 'var(--color-status-down)',
  accent: 'var(--color-accent)',
  meta: 'var(--color-meta)',
  text: 'var(--color-text)',
  'text-muted': 'var(--color-text-muted)',
};
