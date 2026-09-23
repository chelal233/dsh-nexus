import { readdirSync, readFileSync } from "node:fs";
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
