import { access, copyFile, mkdir, rm } from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

// This is a local, reproducible staging step for the three Nexus binaries. It
// never downloads, edits, or starts Harness source or data.
const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const repositoryRoot = path.resolve(appRoot, "..", "..");
const binaryNames = process.platform === "win32"
  ? ["nexus-agent.exe", "nexus-launcher.exe", "nexusctl.exe"]
  : ["nexus-agent", "nexus-launcher", "nexusctl"];
const packageNames = ["nexus-agent", "nexus-launcher", "nexus-cli"];
const cargoCommand = process.env.NEXUS_CARGO_BIN || (process.platform === "win32" ? "cargo.exe" : "cargo");
const cargoManifest = path.join(repositoryRoot, "Cargo.toml");
const releaseDirectory = path.join(repositoryRoot, "target", "release");
const resourceDirectory = path.join(appRoot, "src-tauri", "resources");

function run(command, args, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { ...options, stdio: "inherit" });
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

await run(
  cargoCommand,
  [
    "build",
    "--release",
    ...packageNames.flatMap((packageName) => ["-p", packageName]),
    "--manifest-path",
    cargoManifest,
    "--locked",
  ],
  { cwd: repositoryRoot },
);

for (const binaryName of binaryNames) {
  const binarySource = path.join(releaseDirectory, binaryName);
  try {
    await access(binarySource);
  } catch {
    throw new Error(`Release binary was not produced at ${binarySource}`);
  }
}

await mkdir(resourceDirectory, { recursive: true });
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
]) {
  await rm(path.join(resourceDirectory, staleName), { force: true });
}
for (const binaryName of binaryNames) {
  await copyFile(
    path.join(releaseDirectory, binaryName),
    path.join(resourceDirectory, binaryName),
  );
}

console.log(`[prepare-binaries] staged ${binaryNames.join(", ")} for Tauri at ${resourceDirectory}`);
