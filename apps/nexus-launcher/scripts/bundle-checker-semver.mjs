// Rebuild the checked-in checker dependency from the lockfile-installed tools.
// This command never installs packages or fetches registry data.
import { createRequire } from 'node:module';
import { readFileSync, copyFileSync, mkdirSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const require = createRequire(import.meta.url);
const resolve = createRequire(require.resolve('electron-builder'));
const entry = resolve.resolve('semver');
if (JSON.parse(readFileSync(path.join(path.dirname(entry), 'package.json'))).version !== '7.7.4') {
  throw Error('Review the semver version and license before updating the checker bundle');
}
const out = fileURLToPath(new URL('../../../crates/nexus-agent/src/vendor/', import.meta.url));
mkdirSync(out, { recursive: true });
require(require.resolve('esbuild', { paths: [require.resolve('vite')] })).buildSync({
  entryPoints: [entry], bundle: true, platform: 'node', format: 'cjs', minify: true,
  outfile: path.join(out, 'semver.cjs'),
  banner: { js: '// node-semver 7.7.4 (ISC). Bundled with esbuild; see semver.LICENSE. No network access.' },
});
copyFileSync(path.join(path.dirname(entry), 'LICENSE'), path.join(out, 'semver.LICENSE'));
