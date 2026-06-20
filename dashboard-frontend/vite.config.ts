/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';

// The Rust host (D8) embeds `dist/` via include_dir! and serves the SPA at `/dashboard`
// with static assets under `/dashboard/assets/*`. `base: '/dashboard/'` makes the built
// `index.html` reference absolute `/dashboard/assets/...` URLs that resolve under that
// mount regardless of the route hash (finding 1). A relative base would resolve against
// the current path (e.g. `#/topology`) and 404.
export default defineConfig({
  base: '/dashboard/',
  plugins: [react()],
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
  },
  server: {
    port: 5273,
  },
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./vitest.setup.ts'],
    css: false,
  },
});
