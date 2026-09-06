import { createHash } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { access, copyFile, cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";

// Stages the bundled runtimes into src-tauri/resources/runtime/ for Tauri
// packaging. The runtime/ directory maps to <install dir>/runtime/ in the
// produced installers, which is exactly what nexus-core::bundled_runtime_dir
// observes at run time:
//   runtime/node/node.exe    official Windows x64 distribution binary
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
const resourceRuntime = path.join(appRoot, "src-tauri", "resources", "runtime");
const cacheRoot = path.join(repositoryRoot, "target", "bundled-runtime-cache");

const nodeVersion = process.env.NEXUS_BUNDLED_NODE_VERSION || "24.20.0";
const pnpmVersion = process.env.NEXUS_BUNDLED_PNPM_VERSION || "11.7.0";
const nodeMirror = (process.env.NEXUS_NODE_DIST_MIRROR || "https://nodejs.org/dist").replace(/\/$/, "");
const npmRegistry = (process.env.NEXUS_NPM_REGISTRY || "https://registry.npmjs.org").replace(/\/$/, "");

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
  const destination = path.join(cacheRoot, `node-v${nodeVersion}-win-x64.exe`);
  const shasums = await fetchText(`${nodeMirror}/v${nodeVersion}/SHASUMS256.txt`);
  const line = shasums
    .split("\n")
    .find((entry) => entry.trimEnd().endsWith(`win-x64/node.exe`));
  if (!line) {
    throw new Error(`SHASUMS256.txt for v${nodeVersion} has no win-x64/node.exe entry`);
  }
  const expected = line.trim().split(/\s+/)[0];
  await cachedDownload(`${nodeMirror}/v${nodeVersion}/win-x64/node.exe`, destination, expected);
  const target = path.join(nodeDir, "node.exe");
  await rm(target, { force: true });
  await copyFile(destination, target);

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
  return { version, sha256: expected };
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
  } finally {
    await rm(extractDir, { recursive: true, force: true });
  }
  return { version: pnpmVersion, integrity, entry: "pnpm/bin/pnpm.cjs" };
}

async function isUpToDate(manifestFile) {
  try {
    const manifest = JSON.parse(await readFile(manifestFile, "utf8"));
    if (manifest.node?.version !== `v${nodeVersion}` || manifest.pnpm?.version !== pnpmVersion) {
      return false;
    }
    await access(path.join(resourceRuntime, "node", "node.exe"));
    await access(path.join(resourceRuntime, "pnpm", "bin", "pnpm.cjs"));
    return true;
  } catch {
    return false;
  }
}

const manifestFile = path.join(resourceRuntime, "manifest.json");
if (await isUpToDate(manifestFile)) {
  console.log(`[prepare-runtime] bundled runtime already staged (node ${nodeVersion}, pnpm ${pnpmVersion})`);
} else {
  const node = await stageNode();
  const pnpm = await stagePnpm();
  await writeFile(
    manifestFile,
    `${JSON.stringify({ node: { version: node.version, sha256: node.sha256 }, pnpm }, null, 2)}\n`,
  );
  console.log(`[prepare-runtime] staged node ${node.version} and pnpm ${pnpm.version} at ${resourceRuntime}`);
}
