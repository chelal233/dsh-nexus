// Build-only adapter: run the pinned upstream assembler and its complete smoke.
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
const [source, assets] = process.argv.slice(2);
globalThis.fetch = async () => { throw Error('Desktop assembly must use verified offline assets'); };
process.env.PYTHONDONTWRITEBYTECODE = '1';
const app = path.join(source, 'apps/desktop');
const { resolveDesktopTargetBuildPaths } = await import(pathToFileURL(path.join(app, 'scripts/desktop-build-paths.mjs')));
const paths = resolveDesktopTargetBuildPaths();
fs.mkdirSync(paths.downloads, { recursive: true });
for (const name of fs.readdirSync(assets)) fs.copyFileSync(path.join(assets, name), path.join(paths.downloads, name));
const { preparePrimaryRuntime, smokePrimaryRuntime } = await import(pathToFileURL(path.join(app, 'scripts/prepare-primary-runtime.ts')));
await preparePrimaryRuntime({ deferSmoke: true });
smokePrimaryRuntime(path.join(paths.runtime, 'primary-runtime'));
console.log('Official primary runtime assembly and full smoke passed.');
