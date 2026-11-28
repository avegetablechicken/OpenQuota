import { configDefaults, defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { readFileSync } from 'node:fs';

const packageVersion = (
  JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')) as {
    version: string;
  }
).version;

const windows = process.env.TAURI_ENV_PLATFORM
  ? process.env.TAURI_ENV_PLATFORM === 'windows'
  : process.platform === 'win32';

export default defineConfig({
  publicDir: windows ? 'assets/windows' : 'public',
  define: {
    'import.meta.env.APP_VERSION': JSON.stringify(packageVersion),
  },
  plugins: [
    svelte(),
    windows && {
      name: 'windows-proxy-flags',
      transformIndexHtml: () => [
        { tag: 'link', attrs: { rel: 'stylesheet', href: '/fonts/proxy-flags.css' } },
      ],
    },
  ],
  resolve: {
    conditions: ['browser'],
  },
  clearScreen: false,
  server: {
    strictPort: true,
    watch: {
      ignored: ['**/src-tauri/**'],
    },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari13',
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: Boolean(process.env.TAURI_ENV_DEBUG),
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    css: true,
    // Agent worktrees under .claude/ are separate checkouts with their own tests.
    exclude: [...configDefaults.exclude, '.local/**', '.claude/**'],
  },
});
