// Explicitly supported products; do not silently package a host runtime for
// a different target. Build on a matching native runner.
export const bundleFormats = {
  nsis: { extension: '.exe', count: 1 },
  dmg: { extension: '.dmg', count: 1 },
  zip: { extension: '.zip', count: 1 },
  AppImage: { extension: '.AppImage', count: 1 },
  deb: { extension: '.deb', count: 1 },
  rpm: { extension: '.rpm', count: 1 },
};

export const targets = {
  'x86_64-pc-windows-msvc': {
    platform: 'win32',
    arch: 'x64',
    nodeVersion: '24.20.0',
    archive: 'win-x64.zip',
    sha256: '6cac9ffbca8f6a47091e4b5c772e0606049c3871cb67d900c0cedde630e545ba',
    bundles: ['nsis', 'zip']
  },
  'aarch64-pc-windows-msvc': {
    platform: 'win32',
    arch: 'arm64',
    nodeVersion: '24.20.0',
    archive: 'win-arm64.zip',
    sha256: '31c6799744de8a54601643098040c68c3697e56c94e407d61d0e5fa5f34191d7',
    bundles: ['nsis', 'zip']
  },
  'x86_64-apple-darwin': {
    platform: 'darwin',
    arch: 'x64',
    nodeVersion: '24.20.0',
    archive: 'darwin-x64.tar.gz',
    sha256: '9e5b2644cf107befb6aefca676b96d3296bc10138096f022ed378d6233ed81f4',
    bundles: ['dmg', 'zip']
  },
  'aarch64-apple-darwin': {
    platform: 'darwin',
    arch: 'arm64',
    nodeVersion: '24.20.0',
    archive: 'darwin-arm64.tar.gz',
    sha256: '40e5607e5ecb3db9192723776da2d75d966260fc74a7a9e731c1bd67dda96bc8',
    bundles: ['dmg', 'zip']
  },
};

targets['aarch64-unknown-linux-gnu'] = {
  platform: 'linux', arch: 'arm64', nodeVersion: '24.20.0',
  archive: 'linux-arm64.tar.gz',
  sha256: '3515603e2487879a39bc75716f1a2affd027500c64ba50e845cf72cb33219013',
  bundles: ['AppImage', 'deb', 'rpm'],
};

export const updateChannelFile = spec => `latest-${spec.arch}${spec.platform === 'darwin' ? '-mac' : spec.platform === 'linux' ? '-linux' + (spec.arch === 'x64' ? '' : '-' + spec.arch) : ''}.yml`;

export function selectPlatform(target, platform = process.platform, arch = process.arch) {
  target ||= Object.keys(targets).find(key => targets[key].platform === platform && targets[key].arch === arch);
  const spec = targets[target];
  if (!spec) throw new Error(`Unsupported release target: ${target || `${platform}/${arch}`}`);
  if (spec.platform !== platform || spec.arch !== arch) {
    throw new Error(`Build ${target} on a matching native runner`);
  }
  return { target, ...spec };
}

// Keep target triples and CI identifiers in metadata, not user-facing names.
export function releaseBasename(target, version) {
  const spec = targets[target];
  if (!spec?.bundles) throw new Error(`Unsupported release target: ${target}`);
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`Invalid release version: ${version}`);
  }
  const system = ({ win32: 'windows', darwin: 'macos', linux: 'linux' })[spec.platform];
  const arch = spec.arch === 'ia32' ? 'x86' : spec.arch;
  return `dsh-nexus_${version}_${system}_${arch}`;
}
