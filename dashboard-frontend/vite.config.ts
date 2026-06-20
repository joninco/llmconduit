/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';

// The Rust host embeds `dist/` via include_dir! and serves the SPA from `/dashboard`.
// `base: './'` keeps asset URLs relative so they resolve under that mount point.
export default defineConfig({
  base: './',
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
