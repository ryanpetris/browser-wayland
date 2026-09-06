import { defineConfig } from 'vite';
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// The server embeds every emitted asset. Disabled builds discard the renderer import at compile time.
// audioMotion 4.5.4's click listener outlives destroy(); the viewer owns context resumption.
function patchAudioMotion(code) {
  const listener = 'window.addEventListener( EVENT_CLICK, unlockContext );';
  if (!code.includes(listener)) throw new Error('Recheck audioMotion context ownership for this version');
  return code.replace(listener, '// Modified by browser-wayland: playback owner resumes the shared context.');
}
const visualiser = process.env.BW_VISUALISER !== '0';
export default defineConfig({
  define: { __BW_VISUALISER__: JSON.stringify(visualiser) },
  plugins: [react(), tailwindcss(), {
    name: 'visualiser-notices',
    buildStart() {
      writeFileSync(new URL('node_modules/.bw-visualiser', import.meta.url), visualiser ? '1\n' : '0\n');
    },
    transform(code, id) {
      if (id.endsWith('/audiomotion-analyzer/src/audioMotion-analyzer.js')) return patchAudioMotion(code);
    },
    generateBundle() {
      const notice = readFileSync(new URL('../LICENSE', import.meta.url), 'utf8') + (visualiser
        ? '\nOptional audioMotion-analyzer 4.5.4: AGPL-3.0-or-later. Source and a marked context-ownership modification are included in the viewer source download.\n\n' + readFileSync(new URL('node_modules/audiomotion-analyzer/LICENSE', import.meta.url), 'utf8')
        : '\nThis build excludes the optional audio renderer.\n');
      this.emitFile({ type: 'asset', fileName: 'THIRD_PARTY.txt', source: notice });
      this.emitFile({ type: 'asset', fileName: 'assets/license-notices.txt', source: notice });
      if (!visualiser) return;
      this.emitFile({ type: 'asset', fileName: 'assets/viewer-source.tar.gz', source: execFileSync('tar', [
        '--owner=0', '--group=0', '--numeric-owner', '-czf', '-', 'Cargo.toml', 'Cargo.lock', 'LICENSE', 'Makefile', 'Dockerfile', 'README.md',
        'crates', 'docs', 'packaging', '.github', 'skills', '.dockerignore', 'web/src', 'web/index.html',
        'web/package.json', 'web/package-lock.json', 'web/vite.config.js', 'web/checks',
        'web/node_modules/audiomotion-analyzer/src', 'web/node_modules/audiomotion-analyzer/LICENSE',
      ], { cwd: new URL('..', import.meta.url), maxBuffer: 20 * 1024 * 1024 }) });
      for (const [fileName, path] of [
        ['assets/audiomotion-LICENSE.txt', 'node_modules/audiomotion-analyzer/LICENSE'],
        ['assets/audiomotion-source.js', 'node_modules/audiomotion-analyzer/src/audioMotion-analyzer.js'],
      ]) this.emitFile({ type: 'asset', fileName, source: fileName.endsWith('.js') ? patchAudioMotion(readFileSync(new URL(path, import.meta.url), 'utf8')) : readFileSync(new URL(path, import.meta.url)) });
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
