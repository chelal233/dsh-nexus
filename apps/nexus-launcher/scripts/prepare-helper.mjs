import { access, copyFile, mkdir, rm } from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = path.resolve(appRoot, "..", "..");
const helperName = process.platform === "win32" ? "nexus-launcher.exe" : "nexus-launcher";
const cargoCommand = process.env.NEXUS_CARGO_BIN || (process.platform === "win32" ? "cargo.exe" : "cargo");
const helperSource = path.join(repositoryRoot, "target", "release", helperName);
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
  ["build", "--release", "-p", "nexus-launcher", "--manifest-path", path.join(repositoryRoot, "Cargo.toml"), "--locked"],
  { cwd: repositoryRoot },
);

try {
  await access(helperSource);
} catch {
  throw new Error(`Release helper was not produced at ${helperSource}`);
}

await mkdir(resourceDirectory, { recursive: true });
for (const staleName of ["nexus-launcher", "nexus-launcher.exe"]) {
  await rm(path.join(resourceDirectory, staleName), { force: true });
}

const bundledNames = process.platform === "win32"
  ? ["nexus-launcher.exe", "nexus-launcher"]
  : ["nexus-launcher"];
for (const bundledName of bundledNames) {
  await copyFile(helperSource, path.join(resourceDirectory, bundledName));
}

console.log(`[prepare-helper] staged ${helperName} for Tauri at ${resourceDirectory}`);
