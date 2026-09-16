const { version } = require('./package.json');
const platform = process.platform === 'win32' ? 'windows' : 'macos';
if (!['win32', 'darwin'].includes(process.platform) || !['x64', 'arm64'].includes(process.arch)) {
  throw new Error('Electron release supports Windows/macOS x64 and ARM64');
}
// electron-builder 26.15's 7z auto-filters can silently lose PE files in the
// bundled NSIS decoder. BCJ is supported on both Windows targets (#9983).
if (process.platform === 'win32') process.env.ELECTRON_BUILDER_7Z_FILTER = 'BCJ';
module.exports = {
  appId: 'com.nexus.launcher', productName: 'Nexus Launcher',
  directories: { output: 'electron-dist' },
  artifactName: `dsh-nexus_${version}_${platform}_\${arch}.\${ext}`,
  asar: true,
  files: ['dist/**', 'electron/**', 'desktop/icons/**', 'package.json'],
  // Preserve the staged runtime tree. Its manifest omits only the metadata
  // files that electron-builder's copy walker always excludes.
  extraResources: [{ from: 'desktop/resources', to: '.', filter: ['**/*'] }],
  // Signing is mandatory for release builds. Local unsigned smoke builds are explicit.
  forceCodeSigning: process.env.NEXUS_UNSIGNED_SMOKE !== '1',
  // Versions are identified by published GitHub Release tags (v<package version>).
  publish: [{ provider: 'github', owner: 'chelal233', repo: 'dsh-nexus', vPrefixedTagName: true,
    channel: `latest-${process.arch}`, releaseType: 'prerelease' }],
  generateUpdatesFilesForAllChannels: false,
  win: { target: ['nsis'], icon: 'desktop/icons/icon.ico' },
  nsis: { oneClick: false, perMachine: false, allowToChangeInstallationDirectory: true,
    differentialPackage: false, installerLanguages: ['en_US', 'zh_CN', 'zh_TW', 'ja_JP', 'ko_KR', 'de_DE', 'fr_FR', 'es_ES'],
    deleteAppDataOnUninstall: false },
  mac: { target: ['dmg', 'zip'], icon: 'desktop/icons/icon.icns', hardenedRuntime: true,
    notarize: process.env.NEXUS_UNSIGNED_SMOKE !== '1', category: 'public.app-category.utilities' },
};
