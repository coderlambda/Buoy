import { defineConfig } from 'vite';
import { fileURLToPath, URL } from 'node:url';

export default defineConfig({
  // Mobile owns its document, navigation and visual system. It deliberately does not build the
  // desktop index.html; only the transport/session modules below the renderer are shared.
  root: fileURLToPath(new URL('./apps/mobile/ui', import.meta.url)),
  publicDir: fileURLToPath(new URL('./ui/vendor', import.meta.url)),
  clearScreen: false,
  server: {
    host: process.env.TAURI_DEV_HOST || '127.0.0.1',
    port: 5174,
    strictPort: true,
  },
  build: {
    outDir: fileURLToPath(new URL('./dist-mobile', import.meta.url)),
    emptyOutDir: true,
  },
});
