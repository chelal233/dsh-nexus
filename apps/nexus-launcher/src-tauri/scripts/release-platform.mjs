// Explicitly supported products; do not silently package a host runtime for
// a different target. Cross-building is limited to x86 on Windows x64.
export const targets = {
  'x86_64-pc-windows-msvc': { platform: 'win32', arch: 'x64', nodeVersion: '24.20.0', archive: 'win-x64.zip', sha256: '6cac9ffbca8f6a47091e4b5c772e0606049c3871cb67d900c0cedde630e545ba', bundles: ['nsis', 'msi'] },
  'i686-pc-windows-msvc': { platform: 'win32', arch: 'ia32', nodeVersion: '22.23.2', archive: 'win-x86.zip', sha256: '725c9e2bdd1c2016b41c995a81f4fa36ce4e2ee565b7455d8f889182727df647', bundles: ['nsis', 'msi'] },
  'aarch64-pc-windows-msvc': { platform: 'win32', arch: 'arm64', nodeVersion: '24.20.0', archive: 'win-arm64.zip', sha256: '31c6799744de8a54601643098040c68c3697e56c94e407d61d0e5fa5f34191d7', bundles: ['nsis'] },
  'x86_64-apple-darwin': { platform: 'darwin', arch: 'x64', nodeVersion: '24.20.0', archive: 'darwin-x64.tar.gz', sha256: '9e5b2644cf107befb6aefca676b96d3296bc10138096f022ed378d6233ed81f4', bundles: ['dmg'] },
  'aarch64-apple-darwin': { platform: 'darwin', arch: 'arm64', nodeVersion: '24.20.0', archive: 'darwin-arm64.tar.gz', sha256: '40e5607e5ecb3db9192723776da2d75d966260fc74a7a9e731c1bd67dda96bc8', bundles: ['dmg'] },
};

export function selectPlatform(target, platform = process.platform, arch = process.arch) {
  target ||= Object.keys(targets).find(key => targets[key].platform === platform && targets[key].arch === arch);
  const spec = targets[target];
  if (!spec) throw new Error(`Unsupported release target: ${target || `${platform}/${arch}`}`);
  if (spec.platform !== platform || (spec.arch !== arch && !(platform === 'win32' && arch === 'x64' && spec.arch === 'ia32'))) {
    throw new Error(`Build ${target} on a matching native runner (Windows x64 may build x86)`);
  }
  return { target, ...spec };
}
