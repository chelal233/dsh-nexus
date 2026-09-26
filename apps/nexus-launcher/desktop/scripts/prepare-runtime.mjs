import { gitDistribution, verifyLinuxGitAbi, stageGitNotices, excludeGitCredentialManager, gitCredentialManagerExcluded, verifyGitNotices } from "./bundled-git.mjs";
import { selectPlatform } from "./release-platform.mjs";
import { createHash } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { access, chmod, cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { spawn, execFileSync } from "node:child_process";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";

// Stages the bundled runtimes into desktop/resources/runtime/ for Electron
// packaging. The runtime/ directory maps to <install dir>/runtime/ in the
// produced installers, which is exactly what nexus-core::bundled_runtime_dir
// observes at run time:
//   runtime/node/            full official Windows x64 Node distribution,
//                            including npm/npx and their package tree
//   runtime/pnpm/            the full pnpm npm package tree; the entry run by
//                            the bundled Node is pnpm/bin/pnpm.cjs, whose
//                            relative imports stay inside pnpm/
// Nexus never invokes Corepack, so pnpm is shipped as a direct Node script.
//
// Downloads are build-time only, checksum-verified, and cached under
// target/bundled-runtime-cache/ so repeated builds stay offline. Set
// NEXUS_NODE_DIST_MIRROR (e.g. https://npmmirror.com/mirrors/node) and
// NEXUS_NPM_REGISTRY (e.g. https://registry.npmmirror.com) for restricted
// networks. Versions default to the current upstream release requirement:
// engines.node ^22.19.0 || >=24.0.0 and packageManager pnpm@11.7.0.
const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const repositoryRoot = path.resolve(appRoot, "..", "..");
const resourceRuntime = path.join(appRoot, "desktop", "resources", "runtime");
const cacheRoot = path.join(repositoryRoot, "target", "bundled-runtime-cache");

const spec = selectPlatform(process.env.CARGO_BUILD_TARGET);
const windows = spec.platform === "win32";
const nodeName = windows ? "node.exe" : "node";
const nodeVersion = process.env.NEXUS_BUNDLED_NODE_VERSION || spec.nodeVersion;
const pnpmVersion = process.env.NEXUS_BUNDLED_PNPM_VERSION || "11.7.0";
const nodeMirror = (process.env.NEXUS_NODE_DIST_MIRROR || "https://nodejs.org/dist").replace(/\/$/, "");
const npmRegistry = (process.env.NEXUS_NPM_REGISTRY || "https://registry.npmjs.org").replace(/\/$/, "");

// Out-of-band trust anchors for the default pinned versions. When the
// requested version matches a pinned entry, the downloaded artifact must
// match this hash (fetched independently of the mirror) - a compromised
// mirror cannot serve a modified binary that still passes. Overrides via the
// NEXUS_BUNDLED_*_VERSION env vars fall back to same-source checksums with
// an explicit downgrade warning; bump the pins together with the version
// defaults when upstream requirements change.
const PINNED_NODE_SHA256 = {
  "24.20.0": "5c976096e04e5c2c1f091938926234cc9fbebfe9787ddd149351b3b0ecc707b5",
};
const PINNED_PNPM_INTEGRITY = {
  "11.7.0": "sha512-GcyFLBIMcSV2DyRD7mvgyltA+fUFmN4aCaHxd1A+AQ5Xwjx3ZG4B52HeWb+HT7IqM5jDOrlpH8E+uUa28PTWIA==",
};
const officialNodeMirror = nodeMirror === "https://nodejs.org/dist";
const officialNpmRegistry = npmRegistry === "https://registry.npmjs.org";
const pinnedNodeSha = windows && spec.arch === "x64" ? PINNED_NODE_SHA256[nodeVersion] : undefined;
const pinnedNodeZipSha = nodeVersion === spec.nodeVersion ? spec.sha256 : undefined;
const pinnedPnpmIntegrity = PINNED_PNPM_INTEGRITY[pnpmVersion];
if (!officialNodeMirror && !pinnedNodeZipSha) {
  console.warn(`[prepare-runtime] WARNING: non-official node mirror with no pinned checksum for ${nodeVersion}; verification is same-source only.`);
}
if (!officialNpmRegistry && !pinnedPnpmIntegrity) {
  console.warn(`[prepare-runtime] WARNING: non-official npm registry with no pinned integrity for pnpm ${pnpmVersion}; verification is same-source only.`);
}

function sha256(file) {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha256");
    createReadStream(file)
      .on("data", (chunk) => hash.update(chunk))
      .on("error", reject)
      .on("end", () => resolve(hash.digest("hex")));
  });
}

function sha512Base64(file) {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha512");
    createReadStream(file)
      .on("data", (chunk) => hash.update(chunk))
      .on("error", reject)
      .on("end", () => resolve(hash.digest("base64")));
  });
}

function run(command, args, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ["ignore", "ignore", "inherit"], ...options });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`${command} exited with ${signal || `code ${code}`}`));
      }
    });
  });
}

// Git Bash's GNU tar reads `E:\...` as a remote host:path, and the packaged
// Windows bsdtar has no such interpretation. Prefer the system tar on Windows.
const tarCommand = process.platform === "win32"
  ? path.join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe")
  : "tar";

async function downloadTo(url, destination) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok || !response.body) {
    throw new Error(`download failed (${response.status}) for ${url}`);
  }
  await pipeline(Readable.fromWeb(response.body), createWriteStream(destination));
}

async function fetchText(url) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`request failed (${response.status}) for ${url}`);
  }
  return response.text();
}

async function cachedDownload(url, destination, expectedSha256) {
  try {
    await access(destination);
    if ((await sha256(destination)) === expectedSha256) {
      return;
    }
  } catch {
    // Cache miss or stale content: fall through to a fresh download.
  }
  console.log(`[prepare-runtime] downloading ${url}`);
  await downloadTo(url, destination);
  if ((await sha256(destination)) !== expectedSha256) {
    throw new Error(`checksum mismatch for ${url}`);
  }
}

async function stageNode() {
  const nodeDir = path.join(resourceRuntime, "node");
  await mkdir(nodeDir, { recursive: true });
  await mkdir(cacheRoot, { recursive: true });
  const archiveName = `node-v${nodeVersion}-${spec.archive}`;
  const destination = path.join(cacheRoot, archiveName);
  const shasums = await fetchText(`${nodeMirror}/v${nodeVersion}/SHASUMS256.txt`);
  const line = shasums.split("\n").find((entry) => entry.trim().split(/\s+/)[1] === archiveName);
  if (!line) throw new Error(`SHASUMS256.txt has no ${archiveName} entry`);
  const expected = line.trim().split(/\s+/)[0];
  if (pinnedNodeZipSha && expected !== pinnedNodeZipSha) {
    throw new Error("Node archive checksum does not match the pinned value");
  }
  await cachedDownload(`${nodeMirror}/v${nodeVersion}/${archiveName}`, destination, pinnedNodeZipSha || expected);
  const extractDir = path.join(cacheRoot, `node-extract-${nodeVersion}-${spec.arch}`);
  await rm(extractDir, { recursive: true, force: true });
  await mkdir(extractDir, { recursive: true });
  try {
    await run(tarCommand, ["-xf", destination, "-C", extractDir]);
    await rm(nodeDir, { recursive: true, force: true });
    const extracted = path.join(extractDir, archiveName.replace(/\.(zip|tar\.gz)$/, ""));
    await cp(extracted, nodeDir, { recursive: true, dereference: true });
    if (!windows) {
      // Normalize to the existing Nexus layout; retain the distribution's
      // licenses and resolve links before the no-symlink resource inventory.
      await cp(path.join(extracted, "bin/node"), path.join(nodeDir, "node"));
      await cp(path.join(extracted, "lib/node_modules"), path.join(nodeDir, "node_modules"), { recursive: true, dereference: true });
      for (const name of ["npm", "npx"]) {
        await writeFile(path.join(nodeDir, name), '#!/bin/sh\nHERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\nexec "$HERE/node" "$HERE/node_modules/npm/bin/' + name + '-cli.js" "$@"\n');
        await chmod(path.join(nodeDir, name), 0o755);
      }
      // Dereferencing distribution links moves JS entry files away from their
      // relative imports. Replace every public bin entry with a relocatable
      // Node wrapper; do not rely on the caller's PATH or system Node.
      for (const [name, entry] of Object.entries({ npm: 'npm/bin/npm-cli.js', npx: 'npm/bin/npx-cli.js', corepack: 'corepack/dist/corepack.js', pnpm: 'corepack/dist/pnpm.js', pnpx: 'corepack/dist/pnpx.js', yarn: 'corepack/dist/yarn.js', yarnpkg: 'corepack/dist/yarnpkg.js' })) {
        const target = path.join(nodeDir, 'lib/node_modules', entry);
        if (!['npm', 'npx'].includes(name) && !(await access(target).then(() => true, () => false))) continue;
        await writeFile(path.join(nodeDir, 'bin', name), '#!/bin/sh\nHERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\nexec "$HERE/node" "$HERE/../lib/node_modules/' + entry + '" "$@"\n');
        await chmod(path.join(nodeDir, 'bin', name), 0o755);
      }
      await chmod(path.join(nodeDir, "node"), 0o755);
    }
  } finally {
    await rm(extractDir, { recursive: true, force: true });
  }
  const target = path.join(nodeDir, nodeName);
  const binarySha = await sha256(target);
  if (execFileSync(target, ["-p", "process.arch"], { encoding: "utf8" }).trim() !== spec.arch) throw new Error("Bundled Node architecture disagrees with target");
  if (pinnedNodeSha && binarySha !== pinnedNodeSha) throw new Error("Bundled node.exe checksum mismatch");
  const npm = JSON.parse(await readFile(path.join(nodeDir, "node_modules/npm/package.json"), "utf8"));
  await access(path.join(nodeDir, windows ? "npm.cmd" : "npm"));
  await access(path.join(nodeDir, windows ? "npx.cmd" : "npx"));
  await run(target, [path.join(nodeDir, "node_modules/npm/bin/npm-cli.js"), "--version"]);

  const probe = spawn(target, ["--version"], { stdio: ["ignore", "pipe", "ignore"] });
  const version = await new Promise((resolve, reject) => {
    let out = "";
    probe.stdout.on("data", (chunk) => {
      out += chunk;
    });
    probe.once("error", reject);
    probe.once("exit", (code) => {
      if (code === 0 && out.trim().startsWith("v")) {
        resolve(out.trim());
      } else {
        reject(new Error(`bundled node.exe failed its smoke probe (${code})`));
      }
    });
  });
  return { version, sha256: binarySha, archiveSha256: expected, npmVersion: npm.version };
}

async function stagePnpm() {
  const pnpmDir = path.join(resourceRuntime, "pnpm");
  await mkdir(path.dirname(pnpmDir), { recursive: true });
  await mkdir(cacheRoot, { recursive: true });
  const packument = JSON.parse(await fetchText(`${npmRegistry}/pnpm`));
  const metadata = packument.versions?.[pnpmVersion];
  if (!metadata) {
    throw new Error(`registry has no pnpm version ${pnpmVersion}`);
  }
  const integrity = metadata.dist?.integrity || "";
  const [algorithm, expectedDigest] = integrity.split("-", 2);
  if (algorithm !== "sha512" || !expectedDigest) {
    throw new Error(`pnpm ${pnpmVersion} has no sha512 dist.integrity`);
  }
  if (pinnedPnpmIntegrity && integrity !== pinnedPnpmIntegrity) {
    throw new Error(`registry integrity for pnpm ${pnpmVersion} does not match the pinned value; refusing the registry payload`);
  }
  const tarball = path.join(cacheRoot, `pnpm-${pnpmVersion}.tgz`);
  const tarballExists = await access(tarball).then(
    () => true,
    () => false,
  );
  if (!tarballExists) {
    console.log(`[prepare-runtime] downloading ${metadata.dist.tarball}`);
    await downloadTo(metadata.dist.tarball, tarball);
  }
  const digest = await sha512Base64(tarball);
  if (digest !== expectedDigest) {
    // One retry: a truncated cached tarball is re-downloaded once.
    console.log(`[prepare-runtime] re-downloading ${metadata.dist.tarball}`);
    await downloadTo(metadata.dist.tarball, tarball);
    if ((await sha512Base64(tarball)) !== expectedDigest) {
      throw new Error(`integrity mismatch for pnpm ${pnpmVersion} tarball`);
    }
  }

  // Extract the whole package/ tree (bin shims + dist bundle) into the cache
  // first, then copy it into the resources (a plain rename can hit EPERM on
  // Windows when a scanner still holds handles on the fresh extraction).
  const extractDir = path.join(cacheRoot, `pnpm-extract-${pnpmVersion}`);
  await rm(extractDir, { recursive: true, force: true });
  await mkdir(extractDir, { recursive: true });
  try {
    await run(tarCommand, ["-xf", tarball, "-C", extractDir]);
    const target = path.join(resourceRuntime, "pnpm");
    await rm(target, { recursive: true, force: true });
    await cp(path.join(extractDir, "package"), target, { recursive: true });
    // Nested lifecycle scripts invoke pnpm by name. Resolve the bundled Node
    // relative to this shim, without a global install or system PATH changes.
    if (windows) await writeFile(path.join(target, "bin/pnpm.cmd"),
      '@ECHO OFF\r\nIF DEFINED NEXUS_RUNTIME_NODE (\r\n  "%NEXUS_RUNTIME_NODE%" "%~dp0pnpm.cjs" %*\r\n) ELSE (\r\n  "%~dp0..\\..\\node\\node.exe" "%~dp0pnpm.cjs" %*\r\n)\r\n');
    if (!windows) {
      await writeFile(path.join(target, "bin/pnpm"), '#!/bin/sh\nHERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\nexec "${NEXUS_RUNTIME_NODE:-$HERE/../../node/node}" "$HERE/pnpm.cjs" "$@"\n');
      await chmod(path.join(target, "bin/pnpm"), 0o755);
    }
  } finally {
    await rm(extractDir, { recursive: true, force: true });
  }
  return { version: pnpmVersion, integrity, entry: "pnpm/bin/pnpm.cjs" };
}

async function stageGit() {
  const dist = gitDistribution(spec);
  const archive = path.join(cacheRoot, dist.archive);
  await mkdir(cacheRoot, { recursive: true });
  await cachedDownload(dist.url, archive, dist.sha256);
  const extractDir = path.join(cacheRoot, `git-extract-${spec.target}`);
  await rm(extractDir, { recursive: true, force: true });
  await mkdir(extractDir, { recursive: true });
  try {
    await run(tarCommand, ["-xf", archive, "-C", extractDir]);
    const target = path.join(resourceRuntime, "git");
    // Keep helpers, certificates and licenses; packaging forbids symlinks.
    await access(path.join(extractDir, windows ? "cmd/git.exe" : "bin/git"));
    await rm(target, { recursive: true, force: true });
    await cp(extractDir, target, { recursive: true, dereference: true });
  } finally {
    await rm(extractDir, { recursive: true, force: true });
  }
  const license = path.join(cacheRoot, "git-2.53.0-COPYING");
  await cachedDownload("https://raw.githubusercontent.com/git/git/v2.53.0/COPYING", license, "5b2198d1645f767585e8a88ac0499b04472164c0d2da22e75ecf97ef443ab32e");
  await cp(license, path.join(resourceRuntime, "git/NEXUS-Git-COPYING.txt"));
  excludeGitCredentialManager(path.join(resourceRuntime, "git"), JSON.parse(await readFile(path.join(repositoryRoot, "docs/audits/git-redistribution-2026-09-23/materials/gcm-official-file-boundary.json"), "utf8")), spec);
  stageGitNotices(path.join(repositoryRoot, "docs/audits/git-redistribution-2026-09-23/materials"), path.join(resourceRuntime, "git/NEXUS-NOTICES"));
  if (spec.platform === "linux") verifyLinuxGitAbi(path.join(resourceRuntime,"git"));
  const binary = path.join(resourceRuntime, dist.entry);
  const version = execFileSync(binary, ["--version"], { encoding: "utf8" }).trim();
  if (!version.startsWith("git version 2.53.0")) throw new Error(`Unexpected bundled Git: ${version}`);
  return { version, release: dist.release, entry: dist.entry, archiveSha256: dist.sha256, sha256: await sha256(binary), gcmExcluded: true };
}

async function isUpToDate(manifestFile) {
  try {
    const manifest = JSON.parse(await readFile(manifestFile, "utf8"));
    if (manifest.target !== spec.target || manifest.node?.version !== `v${nodeVersion}` || manifest.pnpm?.version !== pnpmVersion) {
      return false;
    }
    const git = gitDistribution(spec);
    if (manifest.git?.release !== git.release || manifest.git?.archiveSha256 !== git.sha256
      || manifest.git?.entry !== git.entry || await sha256(path.join(resourceRuntime, git.entry)) !== manifest.git?.sha256) return false;
    await access(path.join(resourceRuntime, "git/NEXUS-Git-COPYING.txt"));
    const boundary = JSON.parse(await readFile(path.join(repositoryRoot, "docs/audits/git-redistribution-2026-09-23/materials/gcm-official-file-boundary.json"), "utf8"));
    if (manifest.git?.gcmExcluded !== true || !gitCredentialManagerExcluded(path.join(resourceRuntime, "git"), boundary, spec)) return false;
    const notices = verifyGitNotices(path.join(resourceRuntime, "git/NEXUS-NOTICES"));
    const expectedNotices = JSON.parse(await readFile(path.join(repositoryRoot, "docs/audits/git-redistribution-2026-09-23/materials/manifest.json"), "utf8"));
    if (JSON.stringify(notices) !== JSON.stringify(expectedNotices)) return false;
    if (spec.platform === "linux") verifyLinuxGitAbi(path.join(resourceRuntime,"git"));
    // The staged binary must still match the out-of-band anchor (pinned) or
    // at least the checksum recorded at staging time; a corrupted or
    // tampered resources/runtime falls through to a fresh verified staging.
    const stagedSha = await sha256(path.join(resourceRuntime, "node", nodeName));
    if (pinnedNodeSha) {
      if (stagedSha !== pinnedNodeSha) return false;
    } else if (manifest.node?.sha256 && stagedSha !== manifest.node.sha256) {
      return false;
    }
    if (!windows) {
      for (const name of ['npm', 'npx']) {
        const shim = await readFile(path.join(resourceRuntime, 'node/bin', name), 'utf8');
        if (!shim.includes('$HERE/../lib/node_modules/npm/bin/' + name + '-cli.js')) return false;
      }
    }
    if (!manifest.node?.npmVersion || !manifest.node?.archiveSha256) return false;
    const npm = JSON.parse(await readFile(path.join(resourceRuntime, "node/node_modules/npm/package.json"), "utf8"));
    if (npm.version !== manifest.node.npmVersion) return false;
    await access(path.join(resourceRuntime, windows ? "node/npm.cmd" : "node/npm"));
    await access(path.join(resourceRuntime, windows ? "node/npx.cmd" : "node/npx"));
    await access(path.join(resourceRuntime, "node/node_modules/npm/bin/npm-cli.js"));
    await access(path.join(resourceRuntime, "pnpm", "bin", "pnpm.cjs"));
    const shim = await readFile(path.join(resourceRuntime, "pnpm", "bin", windows ? "pnpm.cmd" : "pnpm"), "utf8");
    if (!shim.includes("NEXUS_RUNTIME_NODE")) return false;
    return true;
  } catch {
    return false;
  }
}

const manifestFile = path.join(resourceRuntime, "manifest.json");
if (await isUpToDate(manifestFile)) {
  console.log(`[prepare-runtime] bundled runtime already staged (node ${nodeVersion} with npm, pnpm ${pnpmVersion})`);
} else {
  const node = await stageNode();
  const pnpm = await stagePnpm();
  const git = await stageGit();
  await writeFile(
    manifestFile,
    `${JSON.stringify({ target: spec.target, node, pnpm, git }, null, 2)}\n`,
  );
  console.log(`[prepare-runtime] staged node ${node.version} with npm ${node.npmVersion} and pnpm ${pnpm.version} at ${resourceRuntime}`);
}
