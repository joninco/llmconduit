/// <reference types="vitest/config" />
import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { configDefaults } from 'vitest/config';

/**
 * Fontsource's compatibility CSS lists WOFF2 followed by a legacy WOFF fallback. The dashboard's
 * browser baseline supports WOFF2, so retaining both doubles the embedded font payload. Strip only
 * the Fontsource WOFF fallback at transform time; all weights/subsets and their unicode ranges stay
 * exactly as authored by Fontsource.
 */
function woff2OnlyFontsource(): Plugin {
  const legacyWoff = /,\s*url\([^)]*\.woff\)\s*format\(['"]woff['"]\)/g;
  return {
    name: 'llmconduit-fontsource-woff2-only',
    enforce: 'pre',
    transform(code, id) {
      const cleanId = id.split('?', 1)[0] ?? id;
      if (!cleanId.includes('/node_modules/@fontsource/') || !cleanId.endsWith('.css')) return null;
      return { code: code.replace(legacyWoff, ''), map: null };
    },
  };
}

// The Rust host (D8) embeds `dist/` via include_dir! and serves the SPA at `/dashboard`
// with static assets under `/dashboard/assets/*`. `base: '/dashboard/'` makes the built
// `index.html` reference absolute `/dashboard/assets/...` URLs that resolve under that
// mount regardless of the route hash (finding 1). A relative base would resolve against
// the current path (e.g. `#/topology`) and 404.
export default defineConfig({
  base: '/dashboard/',
  plugins: [woff2OnlyFontsource(), react()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // CSP forbids inline scripts (script-src 'self'); never inline assets as data: URIs.
    assetsInlineLimit: 0,
    sourcemap: false,
    // The post-build budget checker treats this as a hard ceiling. Keep Vite's own warning aligned.
    chunkSizeWarningLimit: 500,
    rollupOptions: {
      output: {
        manualChunks(id) {
          // The Rust-generated standalone validator is intentionally eager (every REST/WS root
          // is checked before use), but it is large generated code. Give it a stable boundary so
          // neither it nor the application shell exceeds the per-chunk budget. Vite records this
          // static dependency as a modulepreload, so the aggregate initial-gzip budget still
          // accounts for the validator rather than hiding its transfer cost.
          if (id.includes('/src/api/generated/validators-initial.js')) return 'dashboard-contracts';
          if (id.includes('/src/api/generated/validators-rest.js')) return 'dashboard-rest-contracts';
          return undefined;
        },
      },
    },
  },
  server: {
    port: 5273,
  },
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./vitest.setup.ts'],
    css: false,
    // Playwright owns both mock and real-host suites; keep Vitest from grabbing them.
    exclude: [...configDefaults.exclude, 'e2e/**', 'e2e-real/**'],
  },
});
