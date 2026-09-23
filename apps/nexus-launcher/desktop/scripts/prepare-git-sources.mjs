import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export async function digest(file) {
  const hash = createHash('sha256');
  for await (const chunk of fs.createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}
export function validateSources(manifest) {
  if (manifest.schema !== 1 || manifest.complete !== true || (manifest.remaining?.length ?? 0) !== 0 || !Array.isArray(manifest.files) || !manifest.files.length) throw new Error('Git source materials are not cleared');
  const names = new Set();
  for (const entry of manifest.files) {
    if (!/^[a-zA-Z0-9][a-zA-Z0-9_.+~-]*$/.test(entry.file) || names.has(entry.file.toLowerCase()) || !/^[a-f0-9]{64}$/.test(entry.sha256)
      || !Number.isSafeInteger(entry.size) || entry.size <= 0 || new URL(entry.url).protocol !== 'https:') throw new Error('Invalid Git source inventory');
    names.add(entry.file.toLowerCase());
  }
}
async function main() {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../..');
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'docs/audits/git-redistribution-2026-09-23/source-materials.json')));
  validateSources(manifest);
  const version = JSON.parse(fs.readFileSync(path.join(root, 'apps/nexus-launcher/package.json'))).version;
  const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
  const output = path.join(root, 'target/git-source-assets');
  fs.mkdirSync(output, { recursive: true });
  if (fs.readdirSync(output).length) throw new Error('Git source output must be empty');
  const cache = path.join(root, 'target/git-source-cache');
  fs.mkdirSync(cache, { recursive: true });
  const staging = fs.mkdtempSync(path.join(cache, 'companion-'));
  for (const entry of manifest.files) {
    const file = path.join(cache, entry.file);
    let valid = fs.existsSync(file) && fs.statSync(file).size === entry.size && await digest(file) === entry.sha256;
    if (!valid) {
      const partial = `${file}.${process.pid}.partial`;
      const response = await fetch(entry.url, { signal: AbortSignal.timeout(300000) });
      if (!response.ok || !response.body) throw new Error(`Source download failed: ${entry.file} (${response.status})`);
      await pipeline(Readable.fromWeb(response.body), fs.createWriteStream(partial, { flags: 'wx' }));
      if (fs.statSync(partial).size !== entry.size || await digest(partial) !== entry.sha256) throw new Error(`Source checksum mismatch: ${entry.file}`);
      fs.renameSync(partial, file);
    }
    fs.copyFileSync(file, path.join(staging, entry.file));
  }
  fs.writeFileSync(path.join(staging, 'SOURCE-MATERIALS.json'), JSON.stringify(manifest, null, 2)+'\n');
  fs.writeFileSync(path.join(staging, 'README.txt'), 'Corresponding source and build materials for the Git distribution bundled by Nexus. Original archives are preserved. See SOURCE-MATERIALS.json for pinned origin, component mapping and SHA-256. Keep this companion available with redistributed binaries.\n');
  const basename = `dsh-nexus_${version}_git-sources`;
  const archive = `${basename}.tar`;
  execFileSync('tar', ['-cf', path.join(output, archive), '-C', staging, '.'], { stdio: 'inherit' });
  const sha256 = await digest(path.join(output, archive));
  fs.writeFileSync(path.join(output, `${basename}_build.json`), JSON.stringify({ version, commit, file: archive, sha256, sources: manifest.files }, null, 2)+'\n');
  fs.writeFileSync(path.join(output, `${basename}_SHA256SUMS.txt`), `${sha256}  ${archive}\n`);
  console.log(`Prepared ${manifest.files.length} verified Git source archives`);
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
