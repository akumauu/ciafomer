import * as esbuild from 'esbuild';
import { cpSync } from 'fs';

// Bundle TypeScript entry points
await esbuild.build({
  entryPoints: ['src/main.ts'],
  bundle: true,
  outfile: 'dist/main.js',
  format: 'iife',
  target: 'es2020',
  minify: false,
  sourcemap: true,
  external: [],
  define: {
    'window.__TAURI__': 'window.__TAURI__',
  },
});

await esbuild.build({
  entryPoints: ['src/mode-panel.ts'],
  bundle: true,
  outfile: 'dist/mode-panel.js',
  format: 'iife',
  target: 'es2020',
  minify: false,
  sourcemap: true,
  external: [],
  define: {
    'window.__TAURI__': 'window.__TAURI__',
  },
});

await esbuild.build({
  entryPoints: ['src/result-panel.ts'],
  bundle: true,
  outfile: 'dist/result-panel.js',
  format: 'iife',
  target: 'es2020',
  minify: false,
  sourcemap: true,
  external: [],
  define: {
    'window.__TAURI__': 'window.__TAURI__',
  },
});

// Copy static files
cpSync('src/index.html', 'dist/index.html');
cpSync('src/mode-panel.html', 'dist/mode-panel.html');
cpSync('src/result-panel.html', 'dist/result-panel.html');
cpSync('src/style.css', 'dist/style.css');

console.log('Build complete.');
