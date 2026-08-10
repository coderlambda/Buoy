import { defineConfig } from 'vite';

export default defineConfig({
  root: 'ui',
  publicDir: 'vendor',
  clearScreen: false,
  build: {
    outDir: '../dist',
    emptyOutDir: true,
  },
});
