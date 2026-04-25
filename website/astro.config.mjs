import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

async function loadCargoToml() {
  // Local / monorepo build: read from sibling Cargo.toml.
  try {
    const here = dirname(fileURLToPath(import.meta.url));
    return readFileSync(resolve(here, '../Cargo.toml'), 'utf8');
  } catch {
    // Vercel / out-of-tree build: fetch the canonical version from main.
    const url = 'https://raw.githubusercontent.com/nyxCore-Systems/LIP/main/Cargo.toml';
    const res = await fetch(url);
    if (!res.ok) throw new Error(`astro.config: fetch ${url} → ${res.status}`);
    return await res.text();
  }
}

const cargoToml = await loadCargoToml();
const match = cargoToml.match(/^\s*\[workspace\.package\][^\[]*?^\s*version\s*=\s*"([^"]+)"/ms);
if (!match) {
  throw new Error('astro.config: could not parse [workspace.package].version from Cargo.toml');
}
const version = match[1];

export default defineConfig({
  site: 'https://lip.dev',
  integrations: [mdx()],
  vite: {
    define: {
      __LIP_VERSION__: JSON.stringify(version),
    },
  },
});
