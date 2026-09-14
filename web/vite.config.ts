import { fileURLToPath, URL } from 'node:url'

import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

// In production the built app is served by `chibidb serve` from the same origin
// as the SQL endpoints, so no CORS is involved anywhere. `base: './'` keeps the
// asset URLs relative, which is what lets the console sit at `/`.
const apiTarget = process.env.VITE_API_TARGET ?? 'http://127.0.0.1:8080'

// Development only: the dev server proxies to the engine so the browser still
// sees a single origin. `/session` is included because the console fetches its
// session id from there before running anything.
const proxied = ['/query', '/health', '/session', '/api']

export default defineConfig({
  base: './',
  plugins: [vue()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  server: {
    proxy: Object.fromEntries(proxied.map((path) => [path, { target: apiTarget }])),
  },
})
