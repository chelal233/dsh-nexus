const { version, devDependencies } = require('./package.json');
const desktopRuntime = require('./desktop/desktop-runtime-lock.json');
if (devDependencies.electron !== desktopRuntime.electronVersion ||
    require('electron/package.json').version !== desktopRuntime.electronVersion) {
  throw new Error('Nexus Electron must match the pinned official Harness Desktop runtime. Update both pins and reinstall before packaging.');
}
const platform = ({ win32: 'windows', darwin: 'macos', linux: 'linux' })[process.platform];
if (!['win32', 'darwin', 'linux'].includes(process.platform) || !['x64', 'arm64'].includes(process.arch) || (process.platform === 'linux' && process.arch !== 'arm64')) {
  throw new Error('Electron release supports Windows/macOS x64 and ARM64, and Linux ARM64');
}
// electron-builder 26.15's 7z auto-filters can silently lose PE files in the
// bundled NSIS decoder. BCJ is supported on both Windows targets (#9983).
if (process.platform === 'win32') process.env.ELECTRON_BUILDER_7Z_FILTER = 'BCJ';
module.exports = {
  appId: 'com.nexus.launcher', productName: 'Nexus Launcher',
  extraMetadata: { homepage: 'https://github.com/chelal233/dsh-nexus' },
  directories: { output: 'electron-dist' },
  artifactName: `dsh-nexus_${version}_${platform}_\${arch}.\${ext}`,
  asar: true,
  asarUnpack: ['electron/harness-desktop*.mjs', 'electron/prepare-harness-desktop.mjs', 'electron/desktop-runtime*.mjs', 'electron/desktop-paths.mjs', 'electron/desktop-process.mjs'],
  files: ['dist/**', 'electron/**', 'desktop/icons/**', 'package.json'],
  // Preserve the staged runtime tree. Its manifest omits only the metadata
  // files that electron-builder's copy walker always excludes.
  extraResources: [{ from: 'desktop/resources', to: '.', filter: ['**/*'] }],
  // Signing is mandatory for release builds. Local unsigned smoke builds are explicit.
  forceCodeSigning: process.platform !== 'linux' && process.env.NEXUS_UNSIGNED_SMOKE !== '1',
  // Versions are identified by published GitHub Release tags (v<package version>).
  publish: [{ provider: 'github', owner: 'chelal233', repo: 'dsh-nexus', vPrefixedTagName: true,
    channel: `latest-${process.arch}`, releaseType: 'prerelease' }],
  generateUpdatesFilesForAllChannels: false,
  // ZIP-only and --dir builds do not ask electron-builder to generate this.
  // Both portable and installed applications need the same runtime feed.
  afterPack: async context => {
    const path = require('node:path');
    const { writeFile, readFile } = require('node:fs/promises');
    const { createHash } = require('node:crypto');
    const arch = require('electron-builder').Arch[context.arch];
    if (!['x64', 'arm64'].includes(arch)) throw new Error('Unsupported update architecture');
    const resources = context.electronPlatformName === 'darwin'
      ? path.join(context.appOutDir, `${context.packager.appInfo.productFilename}.app`, 'Contents', 'Resources')
      : path.join(context.appOutDir, 'resources');
    await writeFile(path.join(resources, 'nexus-electron-host.json'), JSON.stringify({ schema: 1,
      entry: 'nexus-official-desktop', electronVersion: desktopRuntime.electronVersion,
      appAsarSha256: createHash('sha256').update(await readFile(path.join(resources, 'app.asar'))).digest('hex'),
    }));
    await writeFile(path.join(resources, 'app-update.yml'), [
      'provider: github', 'owner: chelal233', 'repo: dsh-nexus',
      `channel: latest-${arch}`, 'updaterCacheDirName: nexus-launcher-updater', '',
    ].join('\n'));
  },
  win: { target: ['nsis', 'zip'], icon: 'desktop/icons/icon.ico' },
  nsis: { oneClick: false, perMachine: false, allowToChangeInstallationDirectory: true,
    differentialPackage: false, installerLanguages: ['en_US', 'zh_CN', 'zh_TW', 'ja_JP', 'ko_KR', 'de_DE', 'fr_FR', 'es_ES'],
    deleteAppDataOnUninstall: false },
  linux: { target: ['AppImage', 'deb', 'rpm'], executableName: 'nexus-launcher', icon: 'desktop/icons',
    category: 'Utility', synopsis: 'Offline Harness launcher', maintainer: 'Nexus maintainers',
    desktop: { entry: { StartupWMClass: 'nexus-launcher' } } },
  deb: { depends: ['libc6 (>= 2.28)', 'libgtk-3-0', 'libnss3', 'libasound2', 'libgbm1', 'libxss1', 'libatk-bridge2.0-0', 'libdrm2', 'libxkbcommon0'] },
  rpm: { depends: ['glibc >= 2.28', 'gtk3', 'nss', 'alsa-lib', 'mesa-libgbm', 'libXScrnSaver', 'at-spi2-atk', 'libdrm', 'libxkbcommon'] },
  mac: { target: ['dmg', 'zip'], icon: 'desktop/icons/icon.icns', hardenedRuntime: true,
    ...(process.env.NEXUS_UNSIGNED_SMOKE === '1' ? { identity: '-' } : {}),
    sign: async options => (await import('./desktop/scripts/sign-macos.mjs')).signMacApplication(options),
    notarize: process.env.NEXUS_UNSIGNED_SMOKE !== '1', category: 'public.app-category.utilities' },
};
