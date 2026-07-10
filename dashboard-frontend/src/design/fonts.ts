/**
 * Self-hosted webfonts — bundled woff2 served from 'self' (CSP-safe: `font-src 'self'`,
 * no external CDN). Weights mirror how FONTS (palette.ts) are used: Space Grotesk 400–700
 * (display/UI) + IBM Plex Mono 400–600 (data). The Vite Fontsource transform removes each CSS
 * rule's legacy WOFF fallback while preserving every WOFF2 subset/unicode-range, so the embedded
 * dashboard carries one modern font representation rather than two.
 */
import '@fontsource/space-grotesk/400.css';
import '@fontsource/space-grotesk/500.css';
import '@fontsource/space-grotesk/600.css';
import '@fontsource/space-grotesk/700.css';
import '@fontsource/ibm-plex-mono/400.css';
import '@fontsource/ibm-plex-mono/500.css';
import '@fontsource/ibm-plex-mono/600.css';
