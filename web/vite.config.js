import { defineConfig } from 'vite';
import { readFileSync } from 'node:fs';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// audioMotion 4.5.4's click listener outlives destroy(); the viewer owns context resumption.
function patchAudioMotion(code) {
  const listener = 'window.addEventListener( EVENT_CLICK, unlockContext );';
  if (!code.includes(listener)) throw new Error('Recheck audioMotion context ownership for this version');
  return code.replace(listener, '// Modified by browser-wayland: playback owner resumes the shared context.');
}
export default defineConfig({
  plugins: [react(), tailwindcss(), {
    name: 'visualiser-notices',
    transform(code, id) {
      if (id.endsWith('/audiomotion-analyzer/src/audioMotion-analyzer.js')) return patchAudioMotion(code);
    },
    generateBundle() {
      const notice = readFileSync(new URL('../LICENSE', import.meta.url), 'utf8')
        + '\naudioMotion-analyzer 4.5.4: AGPL-3.0-or-later. Source and build instructions: https://github.com/ryanpetris/browser-wayland. The viewer build omits the library context-resumption click listener.\n\n'
        + readFileSync(new URL('node_modules/audiomotion-analyzer/LICENSE', import.meta.url), 'utf8');
      this.emitFile({ type: 'asset', fileName: 'THIRD_PARTY.txt', source: notice });
      this.emitFile({ type: 'asset', fileName: 'assets/license-notices.txt', source: notice });
    },
  }],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    modulePreload: false,
    rollupOptions: { output: { entryFileNames: 'app.js', chunkFileNames: 'assets/[name]-[hash].js', assetFileNames: 'app.[ext]' } },
  },
  server: { proxy: { '/ws': { target: 'ws://127.0.0.1:8080', ws: true }, '/api': 'http://127.0.0.1:8080', '/mcp': 'http://127.0.0.1:8080' } },
});
