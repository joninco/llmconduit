import type { Config } from 'tailwindcss';

/**
 * Tailwind theme derived from the design tokens. We mirror the hex values here
 * (rather than importing tokens.ts) because the Tailwind config is evaluated by
 * PostCSS in a plain-Node context; the canonical source remains src/design/tokens.ts
 * and the two are kept in lockstep. The `font-tabular` utility wires tabular-nums
 * for numeric chips (D9 acceptance criterion).
 */
const config: Config = {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        bg: '#0d0f12',
        panel: '#16191e',
        'panel-raised': '#1e2329',
        line: '#2a313a',
        'status-healthy': '#58d68d',
        'status-cooling': '#f6c453',
        'status-down': '#ff6b6b',
        accent: '#6bb6ff',
        meta: '#c58bd1',
        text: '#e6e9ef',
        'text-muted': '#8b93a1',
      },
      fontFamily: {
        ui: ['system-ui', '-apple-system', 'BlinkMacSystemFont', 'Segoe UI', 'Roboto', 'sans-serif'],
        mono: ['ui-monospace', 'JetBrains Mono', 'SF Mono', 'Menlo', 'Consolas', 'monospace'],
      },
      spacing: {
        // 4px grid is Tailwind's default scale; alias a few semantic steps.
        grid: '4px',
      },
    },
  },
  plugins: [
    ({ addUtilities }: { addUtilities: (u: Record<string, Record<string, string>>) => void }) => {
      addUtilities({
        '.tabular-nums': { 'font-variant-numeric': 'tabular-nums' },
      });
    },
  ],
};

export default config;
