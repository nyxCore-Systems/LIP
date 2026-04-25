import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const cargoToml = readFileSync(resolve(here, '../Cargo.toml'), 'utf8');
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
