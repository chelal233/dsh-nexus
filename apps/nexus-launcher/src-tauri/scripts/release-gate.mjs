import { createHash, randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { mkdir, mkdtemp, readFile, readdir, writeFile, rename, lstat, copyFile, open, unlink, realpath } from "node:fs/promises";
import { DatabaseSync } from "node:sqlite";
import { spawn, execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const app = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const root = path.resolve(app, "../..");
const node = process.execPath;
export function selectBuildId(value) {
  const id = value || `${new Date().toISOString().replace(/[-:.]/g, "")}-${randomUUID().slice(0, 8)}`;
  if (!/^[A-Za-z0-9][A-Za-z0-9_-]{7,95}$/.test(id) || id === "development") throw new Error("Invalid release build ID");
  return id;
}
export function packageOutputs(version, buildId, languages) {
  if (!Array.isArray(languages) || languages.length === 0 || new Set(languages).size !== languages.length
      || languages.some(locale => !["en-US", "zh-CN"].includes(locale))) throw new Error("Unsupported or duplicate MSI language");
  return [
    { kind: "nsis", locale: "multilingual", source: `Nexus Launcher_${version}_x64-setup.exe`, file: `NexusLauncher_${version}_${buildId}_x64.exe` },
    ...languages.map(locale => ({ kind: "msi", locale, source: `Nexus Launcher_${version}_x64_${locale}.msi`, file: `NexusLauncher_${version}_${buildId}_x64_${locale}.msi` })),
  ];
}
async function sha(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}
export async function sourceIdentity(directory) {
  const files = [...new Set(execFileSync("git", ["ls-files", "--cached", "--others", "--exclude-standard", "-z"],
    { cwd: directory, encoding: "utf8", windowsHide: true, maxBuffer: 16 * 1024 * 1024 }).split("\0").filter(Boolean))].sort();
  const entries = [];
  for (const name of files) {
    const file = path.join(directory, name);
    try {
      const info = await lstat(file);
      if (!info.isFile() || info.isSymbolicLink()) throw new Error(`Unsupported source entry: ${name}`);
      entries.push({ path: name, sha256: await sha(file) });
    } catch (error) { if (error.code === "ENOENT") entries.push({ path: name, deleted: true }); else throw error; }
  }
  return { sha256: createHash("sha256").update(JSON.stringify(entries)).digest("hex"), files: entries };
}
async function verificationLock(directory) {
  // A file-backed native lock has no public IPC name to preoccupy. Node's
  // built-in SQLite keeps the OS lock in this process, with no helper lifetime
  // or stale-PID recovery. The file is permanent; only its held lock expires.
  const file=path.join(directory,".verification-guard.sqlite");
  try { const created=await open(file,"wx",0o600); await created.close(); }
  catch(error) { if(error.code!=="EEXIST") throw error; }
  const info=await lstat(file);
   if(!info.isFile()||info.isSymbolicLink()||info.nlink!==1) throw new Error("Verification guard must be an ordinary file with one link");
  if(process.platform!=="win32" && (info.uid!==process.getuid() || (info.mode&0o077)!==0)) throw new Error("Verification guard must be private to its owner");
  const guard=new DatabaseSync(file,{timeout:0,allowExtension:false});
  try {
    if(guard.prepare("PRAGMA journal_mode=DELETE").get().journal_mode!=="delete") throw new Error("Verification guard requires exclusive file locking");
    guard.exec("BEGIN EXCLUSIVE");
  } catch(error) {
    guard.close();
    if(error.errcode===5||error.errcode===6) throw new Error("This release directory has an active verification attempt");
    throw error;
  }
  // Preserve the lifetime of a pending action even if it has no other handles.
  const lifetime=setInterval(()=>{},60*60*1000);
  return () => {try {guard.exec("ROLLBACK");} finally {guard.close();clearInterval(lifetime);} };
}
async function reportBytes(file) {
  const info = await lstat(file).catch(error => { if (error.code === "ENOENT") return null; throw error; });
  if (!info) return null;
  if (!info.isFile() || info.isSymbolicLink()) throw new Error("Verification evidence must be an ordinary file");
  return readFile(file);
}
function decodedReport(bytes, buildId) {
  const value = JSON.parse(bytes.toString("utf8"));
  if (value.schemaVersion !== 1 || value.buildId !== buildId || typeof value.status !== "string") throw new Error("Invalid verification report identity");
  return value;
}
async function admitVerification(candidate, report) {
  const priorBytes = await reportBytes(path.join(candidate, "verification.json"));
  let prior;
  if (priorBytes !== null) {
    try { prior = decodedReport(priorBytes, report.buildId); }
    catch { report.recoveredLatestReport = "Invalid latest copy retained; full verification required"; }
  }
  // An invalid latest copy never overrides successful/unfinished immutable
  // evidence. Unknown attempt evidence is preserved and remains fail-closed.
  for (const entry of await readdir(candidate, { withFileTypes: true })) {
    if (!entry.name.startsWith("attempt-")) continue;
    if (!entry.isDirectory() || entry.isSymbolicLink()) throw new Error("Invalid verification attempt directory");
    const directory = path.join(candidate, entry.name);
    const bytes = await reportBytes(path.join(directory, "verification.json"));
    if (bytes === null && (await readdir(directory)).length === 0) continue;
    let record;
    try { record = decodedReport(bytes ?? Buffer.alloc(0), report.buildId); }
    catch { throw new Error("An immutable attempt report is damaged; original evidence retained. Use a new build ID"); }
    if (record.status !== "failed") throw new Error("Build ID already has successful or incomplete verification evidence; use a new build ID");
  }
  if (prior && prior.status !== "failed") throw new Error("Build ID already has successful or incomplete verification evidence; use a new build ID");
  if (priorBytes !== null) {
    // Only admitted work advances latest. Refusals leave its bytes in place
    // without accumulating copies; identical admitted evidence is deduplicated.
    report.previousReport = `retained-report-${createHash("sha256").update(priorBytes).digest("hex")}.json`;
    const retained = path.join(candidate, report.previousReport);
    try { await writeFile(retained, priorBytes, { flag: "wx" }); }
    catch (error) {
      if (error.code !== "EEXIST" || !(await reportBytes(retained))?.equals(priorBytes)) throw error;
    }
  }
}
export async function verificationAttempt(releaseDirectory, requestedId, action) {
  const report = { schemaVersion: 1, buildId: requestedId ?? null, startedAt: new Date().toISOString(), status: "running", checks: [] };
  let directory, candidate, lock, lockPath, unlock;
  const save = async () => {
    const temp = path.join(directory, "verification.next.json");
    await writeFile(temp, JSON.stringify(report, null, 2) + "\n");
    await rename(temp, path.join(directory, "verification.json"));
  };
  try {
    report.buildId = selectBuildId(requestedId);
    await mkdir(releaseDirectory, { recursive: true });
    releaseDirectory = await realpath(releaseDirectory);
    unlock = await verificationLock(releaseDirectory);
    candidate = path.join(releaseDirectory, `verify-${report.buildId}`);
    await mkdir(candidate, { recursive: true });
    const candidateInfo = await lstat(candidate);
    if (!candidateInfo.isDirectory() || candidateInfo.isSymbolicLink()) throw new Error("Verification candidate must be an ordinary directory");
    lockPath = path.join(candidate, "active.lock");
    if (await reportBytes(lockPath) !== null) {
      report.previousLock = `retained-lock-${randomUUID()}.json`;
      await rename(lockPath, path.join(candidate, report.previousLock));
    }
    lock = await open(lockPath, "wx");
    await lock.writeFile(JSON.stringify({ pid: process.pid, token: randomUUID(), startedAt: report.startedAt }));
    await admitVerification(candidate, report);
    directory = await mkdtemp(path.join(candidate, "attempt-"));
    report.attempt = path.basename(directory);
    await save();
    await action(report, directory, save);
  } catch (error) {
    report.status = "failed"; report.error = error.message;
  } finally {
    report.finishedAt = new Date().toISOString();
    try {
      // A rejected attempt must not alter evidence from an earlier build.
      if (!directory) {
        await mkdir(releaseDirectory, { recursive: true });
        directory = await mkdtemp(path.join(releaseDirectory, "verify-failed-"));
      }
      await save();
      // Keep every attempt immutable after completion, with a latest-report
      // copy for admission. Only the exclusive owner may advance that copy.
      if (lock && path.dirname(directory) === candidate) {
        const next = path.join(candidate, "verification.next.json");
        await writeFile(next, JSON.stringify(report, null, 2) + "\n");
        await rename(next, path.join(candidate, "verification.json"));
      }
    } catch (error) {
      report.status = "failed"; report.reportWriteError = error.message;
      process.stderr.write(`[release-gate] Unable to save report: ${JSON.stringify(report)}\n`);
    } finally {
      try { if (lock) { await lock.close(); await unlink(lockPath); } }
      finally { if (unlock) await unlock(); }
    }
  }
  return { report, directory };
}
async function main() {
  if (process.platform !== "win32" || process.arch !== "x64" || (process.env.CARGO_BUILD_TARGET && process.env.CARGO_BUILD_TARGET !== "x86_64-pc-windows-msvc")) throw new Error("release:gate is the Windows x64 local gate; use Desktop build for other targets");
  const target = path.join(root, "target-rtest");
  const { report, directory } = await verificationAttempt(path.join(target, "release"), process.env.NEXUS_BUILD_ID, async (report, directory, save) => {
  const buildId = report.buildId;
  Object.assign(report, {
    commit: execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8", windowsHide: true }).trim(),
    version: JSON.parse(await readFile(path.join(app, "package.json"), "utf8")).version,
    source: await sourceIdentity(root), status: "running", checks: [],
    realEnvironment: { status: "pending", executor: "Recorded separately through authorized machine acceptance", cases: ["first_install", "overlay_upgrade", "reinstall", "clean_data_reinstall", "interruption_recovery", "installed_offline_start", "tray_controls", "interactive_dsh_terminal"] },
    limitations: ["Ignored Rust integration tests are not passed tests.", "Static and automated checks do not replace user machine acceptance.", "Package resource extraction and independent review are recorded separately before handoff."]
  });
  const env = { ...process.env, NEXUS_BUILD_ID: buildId, CARGO_BUILD_JOBS: "2", CARGO_TARGET_DIR: target };
  async function run(name, command, args, cwd) {
    const check = { name, status: "running", startedAt: new Date().toISOString(), command: [command, ...args], cwd };
    report.checks.push(check); await save();
    let tail = "";
    const began = Date.now();
    const result = await new Promise(resolve => {
      const child = spawn(command, args, { cwd, env, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
      const output = chunk => { const text = chunk.toString(); process.stdout.write(text); tail = (tail + text).slice(-16000); };
      child.stdout.on("data", output); child.stderr.on("data", output);
      child.once("error", error => resolve({ code: null, error: error.message }));
      child.once("close", (code, signal) => resolve({ code, signal }));
    });
    Object.assign(check, result, { status: result.code === 0 ? "passed" : "failed", durationMs: Date.now() - began, outputTail: tail });
    await save();
    if (result.code !== 0) throw new Error(`${name} failed; see ${directory}`);
    const after = await sourceIdentity(root);
    if (after.sha256 !== report.source.sha256) throw new Error("Source changed during release verification; start a new build gate");
  }
  await save();
    await run("rust-workspace", "cargo", ["test", "--manifest-path", "Cargo.toml", "--workspace", "--locked", "-j", "2", "--", "--test-threads=4"], root);
    await run("frontend-typecheck", node, ["node_modules/typescript/bin/tsc", "--noEmit"], app);
    const tests = (await readdir(path.join(app, "tests"))).filter(name => name.endsWith(".test.ts")).sort().map(name => `tests/${name}`);
    await run("frontend-tests", node, ["--experimental-strip-types", "--test", "--test-concurrency=4", ...tests], app);
    const scripts = (await readdir(path.join(app, "tests"))).filter(name => name.endsWith(".test.mjs")).sort().map(name => `tests/${name}`);
    await run("release-script-tests", node, ["--test", "--test-concurrency=4", ...scripts], app);
    await run("compatibility-checker-tests", node, ["--test", "--test-concurrency=4", "crates/nexus-agent/tests/compatibility.test.mjs"], root);
    await run("offline-private-writer-build", "cargo", ["build", "-p", "nexus-agent", "--locked", "-j", "2"], root);
    await run("offline-space-tests", node, ["--test", "crates/nexus-agent/scripts/offline-package-space.test.mjs", "crates/nexus-agent/scripts/offline-package-migration.test.mjs"], root);
    await run("package", node, ["node_modules/@tauri-apps/cli/tauri.js", "build", "--ci"], app);
    // A clean checkout has no generated resources. Tauri's build script copies
    // them even for debug tests, so run the native suite after package staging.
    // No verified candidate artifacts are published if this suite fails.
    await run("native-tests", "cargo", ["test", "--manifest-path", "apps/nexus-launcher/src-tauri/Cargo.toml", "--locked", "-j", "2", "--", "--test-threads=4"], root);
    const identity = JSON.parse(await readFile(path.join(app, "src-tauri/resources/release-identity.json"), "utf8"));
    if (identity.buildId !== buildId || identity.version !== report.version) throw new Error("Packaged release identity differs from verification build");
    report.releaseIdentity = identity;
    report.artifacts = [];
    const tauri = JSON.parse(await readFile(path.join(app, "src-tauri/tauri.windows.conf.json"), "utf8"));
    for (const { kind, locale, source, file: filename } of packageOutputs(report.version, buildId, tauri.bundle.windows.wix.language)) {
      const folder = path.join(target, "release/bundle", kind);
      const output = path.join(folder, source);
      const file = path.join(folder, filename);
      await copyFile(output, file);
      report.artifacts.push({ path: file, locale, sha256: await sha(file), bytes: (await lstat(file)).size });
    }
    report.status = "candidate_pending_package_extraction_and_machine_acceptance";
  });
  if (report.status === "failed") process.exitCode = 1;
  console.log(`[release-gate] ${report.status}: ${directory ?? "stderr report"}`);
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
