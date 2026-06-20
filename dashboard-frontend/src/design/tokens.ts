/**
 * Centralized design tokens for the dark-ops UI — typography, spacing, motion, and the
 * palette/CSS-var derivation. The raw hex/font LITERALS live ONLY in ./palette.ts (a
 * DOM-free module shared with tailwind.config.ts under the Node tsconfig) — finding 8.
 * This module re-exports them as `colors`/`fonts`, derives the CSS vars, and owns the
 * DOM-touching helpers (`applyTokensToRoot`, `prefersReducedMotion`). No palette literals
 * are duplicated anywhere: index.css references the vars; Tailwind references the vars.
 *
 * Spec: D9 §"Design system tokens".
 */
import { PALETTE, FONTS, TAILWIND_COLOR_VARS } from './palette';

/** The palette token object (re-exported from the DOM-free single source). */
export const colors = PALETTE;

/**
 * Maps a provider health string (the wire `ProviderStatus`) to its token color.
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

export const fonts = FONTS;

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
 * THE mapping of CSS custom-property name → value, derived from the palette single source
 * (./palette.ts). `applyTokensToRoot` writes these vars at boot; Tailwind references them
 * via `var(--color-*)`; index.css carries no palette literals. Imperative viz (d3/uPlot/
 * canvas) that cannot use CSS vars reads the raw values from `colors` (same literals).
 */
export const cssVarMap: Record<string, string> = {
  '--color-bg': colors.bg,
  '--color-panel': colors.panel,
  '--color-panel-raised': colors.panelRaised,
  '--color-line': colors.line,
  '--color-status-healthy': colors.statusHealthy,
  '--color-status-cooling': colors.statusCooling,
  '--color-status-down': colors.statusDown,
  '--color-accent': colors.accent,
  '--color-meta': colors.meta,
  '--color-text': colors.text,
  '--color-text-muted': colors.textMuted,
  '--font-ui': fonts.ui,
  '--font-mono': fonts.mono,
};

/**
 * Tailwind color theme (Tailwind color KEY → `var(--color-*)`), re-exported from the
 * DOM-free palette module so tailwind.config.ts can import it under the Node tsconfig.
 */
export const tailwindColorVars = TAILWIND_COLOR_VARS;

/**
 * Writes every token CSS custom property onto a root element (default: documentElement)
 * BEFORE first paint. Called once in main.tsx before `render()`, so Tailwind's
 * `var(--color-*)` utilities resolve on the first frame. index.css holds only a single
 * `html{background}` anti-FOUC literal as a pre-JS fallback.
 */
export function applyTokensToRoot(root: HTMLElement = document.documentElement): void {
  for (const [name, value] of Object.entries(cssVarMap)) {
    root.style.setProperty(name, value);
  }
}

export const tokens = {
  colors,
  fonts,
  spacing,
  radii,
  SPACING_UNIT,
} as const;

export type Tokens = typeof tokens;
