import { access, copyFile, mkdir, rm } from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { verifyStaticRuntime } from "./verify-static-runtime.mjs";
import { selectBuildId } from "./release-gate.mjs";

// This is a local, reproducible staging step for the three Nexus binaries. It
// never downloads, edits, or starts Harness source or data.
const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const repositoryRoot = path.resolve(appRoot, "..", "..");
const binaryNames = process.platform === "win32"
  ? ["nexus-agent.exe", "nexus-launcher.exe", "nexusctl.exe", "nexus-desktop-bridge.exe"]
  : ["nexus-agent", "nexus-launcher", "nexusctl", "nexus-desktop-bridge"];
const packageNames = ["nexus-agent", "nexus-launcher", "nexus-cli"];
const cargoCommand = process.env.NEXUS_CARGO_BIN || (process.platform === "win32" ? "cargo.exe" : "cargo");
const cargoManifest = path.join(repositoryRoot, "Cargo.toml");
const resourceDirectory = path.join(appRoot, "desktop", "resources");

export function recordArtifact(message, artifacts, expectedNames) {
  if (message.reason !== "compiler-artifact" || !message.target?.kind?.includes("bin")
      || message.profile?.test || !message.executable) return;
  const name = path.basename(message.executable);
  if (!expectedNames.includes(name)) return;
  const previous = artifacts.get(name);
  if (previous && previous !== message.executable) throw new Error(`Ambiguous Cargo artifact: ${name}`);
  artifacts.set(name, message.executable);
}

function run(command, args, options) {
  return new Promise((resolve, reject) => {
    const artifacts = new Map();
    let parseError;
    let finished = false;
    const child = spawn(command, args, { ...options, stdio: ["ignore", "pipe", "inherit"] });
    const lines = createInterface({ input: child.stdout });
    lines.on("line", (line) => {
      try {
        const message = JSON.parse(line);
        if (message.reason === "compiler-message" && message.message?.rendered) {
          process.stderr.write(message.message.rendered);
        }
        if (message.reason === "build-finished") finished = message.success === true;
        recordArtifact(message, artifacts, binaryNames);
      } catch (error) { parseError ??= error; }
    });
    child.once("error", reject);
    child.once("close", (code, signal) => {
      if (parseError) reject(parseError);
      else if (code === 0 && finished) {
        resolve(artifacts);
      } else {
        reject(new Error(`${command} exited with ${signal || `code ${code}`}`));
      }
    });
  });
}

async function main() {
const buildId = selectBuildId(process.env.NEXUS_BUILD_ID);
const artifacts = await run(
  cargoCommand,
  [
    "build",
    "--release",
    ...(process.env.CARGO_BUILD_TARGET ? ["--target", process.env.CARGO_BUILD_TARGET] : []),
    "-j", "2",
    ...packageNames.flatMap((packageName) => ["-p", packageName]),
    "--manifest-path",
    cargoManifest,
    "--locked",
    "--message-format=json-render-diagnostics",
  ],
  { cwd: repositoryRoot, env: { ...process.env, NEXUS_BUILD_ID: buildId } },
);

for (const binaryName of binaryNames) {
  const binarySource = artifacts.get(binaryName);
  if (!binarySource) throw new Error(`Cargo did not report a release artifact for ${binaryName}`);
  try {
    await access(binarySource);
  } catch {
    throw new Error(`Release binary was not produced at ${binarySource}`);
  }
}

await mkdir(resourceDirectory, { recursive: true });
verifyStaticRuntime(binaryNames.map(name => artifacts.get(name)));
// Remove only exact generated names for the three staged binaries. The
// resource directory is owned by this staging step; no Harness path is ever
// traversed.
for (const staleName of [
  "nexus-agent",
  "nexus-agent.exe",
  "nexus-launcher",
  "nexus-launcher.exe",
  "nexusctl",
  "nexusctl.exe",
  "nexus-desktop-bridge",
  "nexus-desktop-bridge.exe",
]) {
  await rm(path.join(resourceDirectory, staleName), { force: true });
}
for (const binaryName of binaryNames) {
  await copyFile(
    artifacts.get(binaryName),
    path.join(resourceDirectory, binaryName),
  );
}

console.log(`[prepare-binaries] staged ${binaryNames.join(", ")} for Electron at ${resourceDirectory}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
