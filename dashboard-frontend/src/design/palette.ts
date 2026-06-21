/**
 * THE single source of the raw palette + font literals (finding 8). DOM-FREE (no
 * `window`/`document`/`HTMLElement`) so it imports cleanly into BOTH the app (tokens.ts,
 * browser tsconfig) AND tailwind.config.ts (PostCSS/Node tsconfig, no DOM lib).
 *
 * Colors are defined ONCE as RGB CHANNEL TRIPLES (e.g. `'107 182 255'`). From those we
 * derive: hex (for JS/canvas/d3 that can't use CSS vars), the CSS channel variables
 * (`--color-accent: 107 182 255`), and the Tailwind color map
 * (`accent: 'rgb(var(--color-accent) / <alpha-value>)'`). The `<alpha-value>` placeholder
 * is what makes Tailwind's opacity utilities (`bg-accent/15`, `border-accent/40`) generate
 * correctly (finding 9) — a plain `var(--color-*)` would NOT.
 */

/** Canonical RGB channel triples ("R G B"), the ONLY place colors are literally defined. */
// "Night Watch" — Argus, the hundred-eyed sentinel, rendered as a real-time telemetry
// instrument: a deep indigo observatory ground, an iris-violet brand accent ("the Eye"),
// and amber as the twin attention-signal alongside the semantic status traffic-lights.
export const CHANNELS = {
  bg: '11 13 23',
  panel: '20 24 38',
  panelRaised: '28 33 51',
  line: '42 50 78',
  statusHealthy: '93 226 162',
  statusCooling: '244 193 82',
  statusDown: '255 110 110',
  accent: '143 139 255',
  meta: '226 138 196',
  text: '231 233 243',
  textMuted: '139 147 173',
} as const;

type ChannelKey = keyof typeof CHANNELS;

/** "R G B" → "#rrggbb" (for JS consumers: STATUS_COLOR, d3/uPlot/canvas). */
function channelToHex(triple: string): string {
  const [r, g, b] = triple.split(' ').map((n) => Number(n));
  const hx = (n: number) => (n ?? 0).toString(16).padStart(2, '0');
  return `#${hx(r ?? 0)}${hx(g ?? 0)}${hx(b ?? 0)}`;
}

/** Hex palette derived from the channels (single source stays `CHANNELS`). */
export const PALETTE: Record<ChannelKey, string> & {
  diffAddBg: string;
  diffAddText: string;
  diffRemoveBg: string;
  diffRemoveText: string;
  diffContextBg: string;
} = {
  bg: channelToHex(CHANNELS.bg),
  panel: channelToHex(CHANNELS.panel),
  panelRaised: channelToHex(CHANNELS.panelRaised),
  line: channelToHex(CHANNELS.line),
  statusHealthy: channelToHex(CHANNELS.statusHealthy),
  statusCooling: channelToHex(CHANNELS.statusCooling),
  statusDown: channelToHex(CHANNELS.statusDown),
  accent: channelToHex(CHANNELS.accent),
  meta: channelToHex(CHANNELS.meta),
  text: channelToHex(CHANNELS.text),
  textMuted: channelToHex(CHANNELS.textMuted),
  // Diff tints derived from the status/accent channels at fixed alphas.
  diffAddBg: `rgba(${CHANNELS.statusHealthy.split(' ').join(', ')}, 0.14)`,
  diffAddText: '#9be8b8',
  diffRemoveBg: `rgba(${CHANNELS.statusDown.split(' ').join(', ')}, 0.14)`,
  diffRemoveText: '#ff9d9d',
  diffContextBg: `rgba(${CHANNELS.accent.split(' ').join(', ')}, 0.07)`,
};

export const FONTS = {
  // Space Grotesk: the display/UI voice — geometric grotesk with technical character.
  // IBM Plex Mono: the data voice — every id, model, token count, latency reads as telemetry.
  ui: '"Space Grotesk", system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
  mono: '"IBM Plex Mono", ui-monospace, "JetBrains Mono", "SF Mono", Menlo, Consolas, monospace',
} as const;

/** CSS variable name → channel string ("R G B"). Written to :root by applyTokensToRoot. */
export const CSS_CHANNEL_VARS: Record<string, string> = {
  '--color-bg': CHANNELS.bg,
  '--color-panel': CHANNELS.panel,
  '--color-panel-raised': CHANNELS.panelRaised,
  '--color-line': CHANNELS.line,
  '--color-status-healthy': CHANNELS.statusHealthy,
  '--color-status-cooling': CHANNELS.statusCooling,
  '--color-status-down': CHANNELS.statusDown,
  '--color-accent': CHANNELS.accent,
  '--color-meta': CHANNELS.meta,
  '--color-text': CHANNELS.text,
  '--color-text-muted': CHANNELS.textMuted,
};

/**
 * Tailwind color KEY → `rgb(var(--color-*) / <alpha-value>)`. The `<alpha-value>` token
 * lets Tailwind substitute the opacity from `/15`, `/40`, etc. (finding 9).
 */
export const TAILWIND_COLOR_VARS: Record<string, string> = {
  bg: 'rgb(var(--color-bg) / <alpha-value>)',
  panel: 'rgb(var(--color-panel) / <alpha-value>)',
  'panel-raised': 'rgb(var(--color-panel-raised) / <alpha-value>)',
  line: 'rgb(var(--color-line) / <alpha-value>)',
  'status-healthy': 'rgb(var(--color-status-healthy) / <alpha-value>)',
  'status-cooling': 'rgb(var(--color-status-cooling) / <alpha-value>)',
  'status-down': 'rgb(var(--color-status-down) / <alpha-value>)',
  accent: 'rgb(var(--color-accent) / <alpha-value>)',
  meta: 'rgb(var(--color-meta) / <alpha-value>)',
  text: 'rgb(var(--color-text) / <alpha-value>)',
  'text-muted': 'rgb(var(--color-text-muted) / <alpha-value>)',
};
