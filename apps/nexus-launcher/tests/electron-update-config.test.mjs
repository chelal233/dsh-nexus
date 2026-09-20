import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
const require = createRequire(import.meta.url);
const config = require('../electron-builder.cjs');
for (const platform of ['win32', 'darwin', 'linux']) for (const [arch, name] of [[1, 'x64'], [3, 'arm64']]) {
  test(`portable update config exists for ${platform} ${name}`, async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), 'nexus-update-config-'));
    try {
      const resources = platform === 'darwin' ? path.join(root, 'Nexus Launcher.app/Contents/Resources') : path.join(root, 'resources');
      await mkdir(resources, { recursive: true });
      await writeFile(path.join(resources, 'app.asar'), 'fixture app archive');
      await config.afterPack({ appOutDir: root, arch, electronPlatformName: platform,
        packager: { appInfo: { productFilename: 'Nexus Launcher' } } });
      const value = await readFile(path.join(resources, 'app-update.yml'), 'utf8');
      assert.match(value, /provider: github/);
      assert.ok(value.includes(`channel: latest-${name}\n`));
      assert.match(value, /updaterCacheDirName: nexus-launcher-updater/);
      const host = JSON.parse(await readFile(path.join(resources, 'nexus-electron-host.json'), 'utf8'));
      assert.equal(host.entry, 'nexus-official-desktop');
      assert.equal(host.appAsarSha256, createHash('sha256').update('fixture app archive').digest('hex'));
    } finally { await rm(root, { recursive: true, force: true }); }
  });
}
