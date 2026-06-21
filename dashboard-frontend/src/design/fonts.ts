/**
 * Self-hosted webfonts — bundled woff2 served from 'self' (CSP-safe: `font-src 'self'`,
 * no external CDN). Weights mirror how FONTS (palette.ts) are used: Space Grotesk 400–700
 * (display/UI) + IBM Plex Mono 400–600 (data). Vite emits the woff2 as hashed assets under
 * dist/assets (assetsInlineLimit: 0), so the embedded dashboard serves them from itself.
 */
import '@fontsource/space-grotesk/400.css';
import '@fontsource/space-grotesk/500.css';
import '@fontsource/space-grotesk/600.css';
import '@fontsource/space-grotesk/700.css';
import '@fontsource/ibm-plex-mono/400.css';
import '@fontsource/ibm-plex-mono/500.css';
import '@fontsource/ibm-plex-mono/600.css';
