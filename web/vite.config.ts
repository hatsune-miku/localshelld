import { resolve } from 'node:path'
import { defineConfig } from 'vite'

import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    rollupOptions: {
      input: {
        main: resolve(import.meta.dirname, 'index.html'),
        consent: resolve(import.meta.dirname, 'consent.html'),
      },
    },
  },
  server: {
    proxy: {
      '/health': 'http://127.0.0.1:32119',
      '/v1': 'http://127.0.0.1:32119',
    },
  },
})
