import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Fixed output names: the server embeds dist/index.html, dist/app.js and dist/app.css with include_str!.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    modulePreload: false,
    rollupOptions: { output: { entryFileNames: 'app.js', chunkFileNames: 'app.js', assetFileNames: 'app.[ext]' } },
  },
  server: { proxy: { '/ws': { target: 'ws://127.0.0.1:8080', ws: true }, '/api': 'http://127.0.0.1:8080', '/mcp': 'http://127.0.0.1:8080' } },
});
