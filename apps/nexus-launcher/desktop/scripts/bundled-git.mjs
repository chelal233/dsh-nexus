import { readdirSync, readFileSync, lstatSync, mkdirSync, copyFileSync, unlinkSync, writeFileSync, existsSync } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";
// Portable distributions maintained by GitHub Desktop. Build-time downloads only.
export const gitRelease = "v2.53.0-4";
const distributions = {
  "win32/x64": ["windows-x64", "7b76bc5c32c0d7c5984efdc2a8a32697cf1e8a43bc55176fbf9869c0ee995130"],
  "win32/arm64": ["windows-arm64", "1abbeb3a2ce06e9b80e75bb888dce959b6c73bdb11ccc670a01a71d64f4422a5"],
  "darwin/x64": ["macOS-x64", "ae6686718aa34f4140424db16b92a47dcffd6d1f312eb8b5f3b267f7404e2680"],
  "darwin/arm64": ["macOS-arm64", "f9dc64635a5b62fbd7ad95db73268bbb8912255ac516d65d37bf7af22fcb8ffe"],
  "linux/arm64": ["ubuntu-arm64", "a161f45af4626bb7e0c688854bd4a9aee47cc514bca404cff0a5e3536ef1c0af"],
};
export function gitDistribution({ platform, arch }) {
  const item = distributions[`${platform}/${arch}`];
  if (!item) throw new Error(`Unsupported bundled Git target: ${platform}/${arch}`);
  const archive = `dugite-native-v2.53.0-4098283-${item[0]}.tar.gz`;
  return { release: gitRelease, archive, sha256: item[1],
    entry: platform === "win32" ? "git/cmd/git.exe" : "git/bin/git",
    url: `https://github.com/desktop/dugite-native/releases/download/${gitRelease}/${archive}` };
}

// Portable Git has a higher ABI floor than the Rust executables. Check every
// ELF helper/library, not just git --version on the build machine.
export function verifyLinuxGitAbi(root) {
  let count=0;
  const visit=directory=>{
    for(const entry of readdirSync(directory,{withFileTypes:true})) {
      const file=path.join(directory,entry.name);
      if(entry.isSymbolicLink()) throw new Error(`Linked Git runtime: ${file}`);
      if(entry.isDirectory()){visit(file);continue;}
      if(!entry.isFile())throw new Error(`Unsupported Git runtime entry: ${file}`);
      const bytes=readFileSync(file);
      if(bytes.length<20 || bytes.subarray(0,4).toString('hex')!=='7f454c46')continue;
      count++;
      if(bytes[5]!==1 || bytes.readUInt16LE(18)!==183)throw new Error(`Git runtime is not ARM64 ELF: ${file}`);
      for(const match of bytes.toString('latin1').matchAll(/GLIBC_(\d+)\.(\d+)/g)) {
        if(Number(match[1])>2 || Number(match[1])===2&&Number(match[2])>34)throw new Error(`Git helper exceeds glibc 2.34: ${file} ${match[0]}`);
      }
    }
  };
  visit(root);
  if(!count)throw new Error('Git runtime contains no ELF executables');
  return count;
}

// Notice materials are repository-owned and pinned before entering an offline runtime.
export function verifyGitNotices(materials) {
  const manifest = JSON.parse(readFileSync(path.join(materials, 'manifest.json'), 'utf8'));
  if (!Array.isArray(manifest.files) || !manifest.files.length) throw new Error('Missing Git notice inventory');
  const seen = new Set();
  for (const item of manifest.files) {
    if (typeof item.path !== 'string' || !/^[a-zA-Z0-9_.+~-]+(?:\/[a-zA-Z0-9_.+~-]+)*$/.test(item.path)
        || item.path.split('/').some(part => part === '.' || part === '..') || seen.has(item.path)) throw new Error('Invalid Git notice path');
    seen.add(item.path);
    let file = materials;
    for (const part of item.path.split('/')) {
      file = path.join(file, part);
      if (lstatSync(file).isSymbolicLink()) throw new Error('Linked Git notice');
    }
    if (!lstatSync(file).isFile() || createHash('sha256').update(readFileSync(file)).digest('hex') !== item.sha256) throw new Error(`Git notice checksum mismatch: ${item.path}`);
  }
  return manifest;
}
export function stageGitNotices(materials, destination) {
  const manifest = verifyGitNotices(materials);
  for (const item of manifest.files) {
    const output = path.join(destination, item.path);
    mkdirSync(path.dirname(output), { recursive: true });
    copyFileSync(path.join(materials, item.path), output);
  }
  copyFileSync(path.join(materials, 'manifest.json'), path.join(destination, 'manifest.json'));
  return manifest.files.length;
}

// Remove only independently matched optional GCM files from a freshly staged tree.
export function excludeGitCredentialManager(root, boundary, { platform, arch }) {
  if (platform === 'linux') return 0;
  const target = `${platform === 'win32' ? 'windows' : 'macOS'}-${arch}`;
  const record = boundary.find(item => item.target === target);
  if (!record || !Array.isArray(record.files)) throw new Error(`Missing GCM exclusion evidence: ${target}`);
  const base = platform === 'win32' ? `${arch === 'arm64' ? 'clangarm64' : 'mingw64'}/bin` : 'libexec/git-core';
  const entries = record.files.filter(item => item.present && item.matches && item.name !== 'NOTICE');
  if (!entries.length || !entries.some(item => item.name.startsWith('git-credential-manager'))) throw new Error('Incomplete GCM exclusion evidence');
  const files = entries.map(item => {
    if (!/^[a-zA-Z0-9_.+-]+(?:\/[a-zA-Z0-9_.+-]+)*$/.test(item.name) || item.name.split('/').some(part => part === '.' || part === '..')) throw new Error('Unsafe GCM exclusion path');
    const file = path.resolve(root, base, item.name);
    for (let parent = path.dirname(file); parent !== path.resolve(root); parent = path.dirname(parent)) {
      if (parent === path.dirname(parent) || lstatSync(parent).isSymbolicLink()) throw new Error('Linked GCM exclusion parent');
    }
    if (!file.startsWith(path.resolve(root)+path.sep) || !lstatSync(file).isFile() || lstatSync(file).isSymbolicLink()
      || createHash('sha256').update(readFileSync(file)).digest('hex') !== item.sha256) throw new Error(`GCM exclusion identity mismatch: ${item.name}`);
    return file;
  });
  // Validate the complete removal set before changing any staged file.
  for (const file of files) unlinkSync(file);
  if (platform === 'win32') {
    const config = path.join(root, 'etc/gitconfig');
    const text = readFileSync(config, 'utf8');
    writeFileSync(config, text.replace(/^[ \t]*helper[ \t]*=[ \t]*manager[ \t]*\r?\n/gm, ''));
  }
  return files.length;
}

export function gitCredentialManagerExcluded(root, boundary, {platform, arch}) {
  if (platform === 'linux') return true;
  const record = boundary.find(item => item.target === `${platform === 'win32' ? 'windows' : 'macOS'}-${arch}`);
  if (!record?.files?.length) return false;
  const base = platform === 'win32' ? `${arch === 'arm64' ? 'clangarm64' : 'mingw64'}/bin` : 'libexec/git-core';
  if (record.files.some(item => item.present && item.matches && item.name !== 'NOTICE' && existsSync(path.join(root, base, item.name)))) return false;
  return platform !== 'win32' || !/^[ \t]*helper[ \t]*=[ \t]*manager[ \t]*\r?$/m.test(readFileSync(path.join(root, 'etc/gitconfig'), 'utf8'));
}
