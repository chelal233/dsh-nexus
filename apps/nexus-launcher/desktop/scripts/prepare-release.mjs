import { selectPlatform } from "./release-platform.mjs";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { readFile, readdir, writeFile, lstat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const repositoryRoot = path.resolve(appRoot, "../..");

async function digest(file) {
  const hash = createHash("sha256");
  let bytes = 0;
  for await (const chunk of createReadStream(file)) { hash.update(chunk); bytes += chunk.length; }
  return { bytes, sha256: hash.digest("hex") };
}

// Manifest scope is fixed to the staged executables and runtime tree. Never
// traverse data directories, symlinks, junctions, or arbitrary manifest paths.
export async function inventoryResources(root, names) {
  const result = [];
  async function visit(relative) {
    for (let ancestor = path.dirname(relative); ancestor !== "."; ancestor = path.dirname(ancestor)) {
      if ((await lstat(path.join(root, ancestor))).isSymbolicLink()) throw new Error(`Linked release ancestor: ${ancestor}`);
    }
    const parent = path.dirname(relative);
    const entries = await readdir(path.join(root, parent), { withFileTypes: true });
    const entry = entries.find(item => item.name === path.basename(relative));
    if (!entry || entry.isSymbolicLink()) throw new Error(`Missing or linked release resource: ${relative}`);
    if (entry.isDirectory()) {
      for (const child of (await readdir(path.join(root, relative))).sort()) {
        // electron-builder's copy walker always omits these metadata files,
        // including inside bundled packages. They are not runtime resources.
        if (child === '.gitkeep' || child === '.DS_Store') continue;
        await visit(path.join(relative, child));
      }
    } else if (entry.isFile()) {
      result.push({ path: relative.replaceAll("\\", "/"), ...await digest(path.join(root, relative)) });
    } else throw new Error(`Unsupported release resource: ${relative}`);
  }
  for (const name of names) await visit(name);
  return result.sort((a, b) => a.path.localeCompare(b.path, "en"));
}

export async function verifyInventory(root, files) {
  for (const file of files) {
    if (!file.path || path.isAbsolute(file.path) || file.path.includes("\\") || file.path.split("/").some(p => !p || p === ".." || p === ".") || file.path.includes(":")) {
      throw new Error("Invalid release resource path");
    }
    const current = (await inventoryResources(root, [file.path]))[0];
    if (current.path !== file.path || current.bytes !== file.bytes || current.sha256 !== file.sha256) {
      throw new Error(`Release resource changed: ${file.path}`);
    }
  }
}

export function releaseIdentity(manifest, manifestSha256) {
  return { schemaVersion: 1, version: manifest.version, buildId: manifest.buildId,
    createdAt: manifest.createdAt, commit: manifest.commit, dirty: manifest.dirty,
    node: manifest.runtime.node.version, npm: manifest.runtime.node.npmVersion, pnpm: manifest.runtime.pnpm.version,
    manifestSha256 };
}

export function verifyIdentity(manifest, identity, manifestSha256) {
  const expected = releaseIdentity(manifest, manifestSha256);
  if (Object.entries(expected).some(([key, value]) => value === undefined || identity[key] !== value)) {
    throw new Error("Release identity does not match the verified manifest");
  }
}

export function verifyRuntimeVersions(resources, runtime) {
  const node = path.join(resources, "runtime/node", process.platform === "win32" ? "node.exe" : "node");
  const run = args => execFileSync(node, args, { cwd: resources, encoding: "utf8", windowsHide: true, timeout: 15000 }).trim();
  const spec = selectPlatform(process.env.CARGO_BUILD_TARGET);
  if (runtime.target !== spec.target || run(["-p", "process.arch"]) !== spec.arch) throw new Error("Runtime target architecture mismatch");
  const actual = {
    node: run(["--version"]),
    npm: run([path.join(resources, "runtime/node/node_modules/npm/bin/npm-cli.js"), "--version"]),
    pnpm: run([path.join(resources, "runtime/pnpm/bin/pnpm.cjs"), "--version"]),
  };
  const gitEntry = process.platform === "win32" ? "git/cmd/git.exe" : "git/bin/git";
  if (runtime.git?.entry !== gitEntry) throw new Error("Bundled Git manifest is missing or has an invalid entry");
  actual.git = execFileSync(path.join(resources, "runtime", gitEntry), ["--version"], { cwd: resources, encoding: "utf8", windowsHide: true, timeout: 15000 }).trim();
  if (actual.git !== runtime.git.version) throw new Error("Bundled Git version disagrees with the manifest");
  if (actual.node !== runtime.node.version || actual.npm !== runtime.node.npmVersion || actual.pnpm !== runtime.pnpm.version) {
    throw new Error(`Bundled runtime versions disagree with the manifest: ${JSON.stringify(actual)}`);
  }
}

export function verifyAgentIdentity(manifest, agent) {
  if (!agent.buildId || agent.buildId === "development" || agent.buildId !== manifest.buildId || agent.version !== manifest.version) {
    throw new Error("Compiled Agent identity disagrees with the release manifest; run prepare:agent again");
  }
}

function readAgentIdentity(resources) {
  return JSON.parse(execFileSync(path.join(resources, process.platform === "win32" ? "nexus-agent.exe" : "nexus-agent"),
    ["--build-identity"], { cwd: resources, encoding: "utf8", windowsHide: true, timeout: 5000 }));
}

async function main() {
  const resources = path.join(appRoot, "desktop/resources");
  const { verifyDesktopKit } = await import('../../electron/desktop-runtime.mjs');
  verifyDesktopKit(path.join(resources, 'runtime/desktop'));
  const manifestPath = path.join(resources, "release-manifest.json");
  if (process.argv.includes("--verify")) {
    const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
    verifyAgentIdentity(manifest, readAgentIdentity(resources));
    const actual = await inventoryResources(resources, ["nexus-agent", "nexus-launcher", "nexusctl", "nexus-desktop-bridge"].map(n => n + (process.platform === "win32" ? ".exe" : "")).concat("runtime", "notices"));
    if (JSON.stringify(actual) !== JSON.stringify(manifest.files)) throw new Error("Release inventory is incomplete or changed");
    verifyIdentity(manifest, JSON.parse(await readFile(path.join(resources, "release-identity.json"), "utf8")), (await digest(manifestPath)).sha256);
    verifyRuntimeVersions(resources, manifest.runtime);
    await verifyInventory(resources, manifest.files);
    console.log(`[release] verified ${manifest.buildId}: ${manifest.files.length} resources`);
    return;
  }
  const pkg = JSON.parse(await readFile(path.join(appRoot, "package.json"), "utf8"));
  const workspace = await readFile(path.join(repositoryRoot, "Cargo.toml"), "utf8");
  if (workspace.match(/^version\s*=\s*"([^"]+)"/m)?.[1] !== pkg.version) throw new Error("Package and Rust release versions disagree");
  const suffix = process.platform === "win32" ? ".exe" : "";
  const files = await inventoryResources(resources, ["nexus-agent", "nexus-launcher", "nexusctl", "nexus-desktop-bridge"].map(n => n + suffix).concat("runtime", "notices"));
  const runtime = JSON.parse(await readFile(path.join(resources, "runtime/manifest.json"), "utf8"));
  verifyRuntimeVersions(resources, runtime);
  const git = args => execFileSync("git", args, { cwd: repositoryRoot, encoding: "utf8", windowsHide: true }).trim();
  const manifest = {
    schemaVersion: 1, version: pkg.version,
    buildId: readAgentIdentity(resources).buildId,
    createdAt: new Date().toISOString(), commit: git(["rev-parse", "HEAD"]),
    dirty: git(["status", "--porcelain", "--untracked-files=normal"]).length > 0,
    runtime, files,
  };
  verifyAgentIdentity(manifest, readAgentIdentity(resources));
  await writeFile(manifestPath, JSON.stringify(manifest, null, 2) + "\n");
  const identity = releaseIdentity(manifest, (await digest(manifestPath)).sha256);
  await writeFile(path.join(resources, "release-identity.json"), JSON.stringify(identity, null, 2) + "\n");
  await verifyInventory(resources, files);
  console.log(`[release] ${manifest.version} / ${manifest.buildId}; verified ${files.length} resources`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
