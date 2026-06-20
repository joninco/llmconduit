import type { Config } from 'tailwindcss';
import { TAILWIND_COLOR_VARS, FONTS } from './src/design/palette';

/**
 * Tailwind theme derived from the palette single source (src/design/palette.ts — DOM-free
 * so it imports cleanly under the Node tsconfig). Colors reference `var(--color-*)` (no hex
 * here; `applyTokensToRoot` defines the vars at boot), so there is no palette duplication
 * (finding 8).
 */
const config: Config = {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: TAILWIND_COLOR_VARS,
      fontFamily: {
        ui: FONTS.ui.split(',').map((s) => s.trim()),
        mono: FONTS.mono.split(',').map((s) => s.trim()),
      },
      spacing: {
        // 4px grid is Tailwind's default scale; alias a semantic step used by chips.
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
