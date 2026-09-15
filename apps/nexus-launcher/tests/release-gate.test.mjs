import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdir, mkdtemp, writeFile, rm, readFile, readdir, symlink } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:net";
import { createHash } from "node:crypto";
import { selectBuildId, sourceIdentity, packageOutputs, verificationAttempt } from "../src-tauri/scripts/release-gate.mjs";

test("failed gates can retry the same ID while successful and concurrent attempts stay protected", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "nexus-gate-attempt-"));
  try {
    const first = await verificationAttempt(directory, "review-build", async () => { throw new Error("source identity failed"); });
    const reportFile = path.join(first.directory, "verification.json");
    const before = await readFile(reportFile, "utf8");
    assert.equal(JSON.parse(before).status, "failed"); assert.match(before, /source identity failed/);
    let called = false;
    const repeated = await verificationAttempt(directory, "review-build", async report => { called = true; report.status = "passed"; });
    assert.equal(called, true); assert.notEqual(repeated.directory, first.directory);
    assert.equal(repeated.report.status, "passed");
    assert.equal(await readFile(reportFile, "utf8"), before);
    const rejected = await verificationAttempt(directory, "review-build", async () => assert.fail("successful ID reused"));
    assert.match(rejected.report.error, /use a new build ID/);
    assert.equal(JSON.parse(await readFile(path.join(repeated.directory, "verification.json"))).status, "passed");
    const existing = path.join(directory,"verify-existing-build");await mkdir(existing);
    const bytes = '{ "schemaVersion": 1, "buildId": "existing-build", "status": "failed", "checks": [] }\n';
    await writeFile(path.join(existing,"verification.json"),bytes);
    const resumed=await verificationAttempt(directory,"existing-build",async report=>{report.status="passed";});
    assert.equal(resumed.report.status,"passed");
    assert.equal(await readFile(path.join(existing,resumed.report.previousReport),"utf8"),bytes);
    await verificationAttempt(directory, "concurrent-build", async report => {
      const other = await verificationAttempt(directory, "concurrent-build", async () => assert.fail("concurrent attempt admitted"));
      assert.match(other.report.error, /active verification/); report.status = "passed";
    });
    const invalid = await verificationAttempt(directory, "../outside", async () => assert.fail("invalid ID admitted"));
    assert.match(invalid.report.error, /Invalid release build ID/);
    assert.equal(JSON.parse(await readFile(path.join(invalid.directory, "verification.json"))).status, "failed");
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("damaged latest reports recover without losing bytes or overriding immutable successful evidence", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "nexus-gate-damaged-"));
  try {
    const candidate = path.join(directory, "verify-damaged-build"); await mkdir(candidate);
    const original = Buffer.from([0xff, 0xfe, 0, 0x7b, 0x22]);
    await writeFile(path.join(candidate, "verification.json"), original);
    const first = await verificationAttempt(directory, "damaged-build", async report => { report.status = "failed"; });
    assert.deepEqual(await readFile(path.join(candidate, first.report.previousReport)), original);
    assert.match(first.report.recoveredLatestReport, /full verification required/);
    await writeFile(path.join(candidate, "verification.json"), original);
    const second = await verificationAttempt(directory, "damaged-build", async report => { report.status = "passed"; });
    assert.equal(second.report.status, "passed");
    const immutable = await readFile(path.join(second.directory, "verification.json"));
    await writeFile(path.join(candidate, "verification.json"), original);
    const denied = await verificationAttempt(directory, "damaged-build", async () => assert.fail("successful ID reused"));
    assert.match(denied.report.error, /successful or incomplete/);
    assert.deepEqual(await readFile(path.join(candidate, "verification.json")), original);
    assert.deepEqual(await readFile(path.join(second.directory, "verification.json")), immutable);
    await writeFile(path.join(second.directory, "verification.json"), original);
    const unknown = await verificationAttempt(directory, "damaged-build", async () => assert.fail("damaged immutable evidence ignored"));
    assert.match(unknown.report.error, /immutable attempt report is damaged/);
    assert.deepEqual(await readFile(path.join(second.directory, "verification.json")), original);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("repeated admission refusals do not add retained copies to a completed build", async () => {
  const directory=await mkdtemp(path.join(os.tmpdir(),"nexus-gate-refused-"));
  try {
    await verificationAttempt(directory,"finished-build",async report=>{report.status="passed";});
    const candidate=path.join(directory,"verify-finished-build");
    const before=(await readdir(candidate)).sort();
    const bytes=await readFile(path.join(candidate,"verification.json"));
    for(let i=0;i<3;i++) {
      const result=await verificationAttempt(directory,"finished-build",async()=>assert.fail("finished ID reused"));
      assert.match(result.report.error,/successful or incomplete/);
      assert.deepEqual((await readdir(candidate)).sort(),before);
      assert.deepEqual(await readFile(path.join(candidate,"verification.json")),bytes);
    }
  } finally {await rm(directory,{recursive:true,force:true});}
});

test("kernel lock covers all build IDs and path aliases while stale disk markers recover", async () => {
  const base = await mkdtemp(path.join(os.tmpdir(), "nexus-gate-lock-"));
  const directory = path.join(base, "real"), alias = path.join(base, "alias");
  try {
    await mkdir(directory); await symlink(directory, alias, process.platform === "win32" ? "junction" : "dir");
    const candidate = path.join(directory, "verify-stale-build"); await mkdir(candidate);
    const marker = Buffer.from("stale pre-report lock\0"); await writeFile(path.join(candidate, "active.lock"), marker);
    const resumed = await verificationAttempt(directory, "stale-build", async report => {
      for (const dir of [directory, alias, ...(process.platform === "win32" ? [directory.toUpperCase()] : [])]) {
        const other = await verificationAttempt(dir, "different-build", async () => assert.fail("shared output owner bypassed"));
        assert.match(other.report.error, /active verification/);
      }
      report.status = "passed";
    });
    assert.equal(resumed.report.status, "passed");
    assert.deepEqual(await readFile(path.join(candidate, resumed.report.previousLock)), marker);
    assert.equal((await readdir(candidate)).includes("active.lock"), false);
  } finally { await rm(base, { recursive: true, force: true }); }
});

test("preoccupying the old public IPC name does not deny the file-backed gate", async () => {
  const directory=await mkdtemp(path.join(os.tmpdir(),"nexus-gate-preoccupied-"));
  const identity=process.platform==="win32"?directory.toLowerCase():directory;
  const name=`nexus-release-${createHash("sha256").update(identity).digest("hex")}`;
  const server=createServer(socket=>socket.destroy());
  // macOS supports filesystem sockets, not Linux abstract namespace sockets.
  const unixAddress = process.platform === "linux" ? `\0${name}` : path.join(directory, "public.sock");
  try {
    await new Promise((resolve,reject)=>{server.once("error",reject);server.listen(process.platform==="win32"?`\\\\.\\pipe\\${name}`:unixAddress,resolve);});
    const result=await verificationAttempt(directory,"public-preoccupied",async report=>{report.status="passed";});
    assert.equal(result.report.status,"passed");
  } finally {await new Promise(resolve=>server.close(resolve));await rm(directory,{recursive:true,force:true});}
});

test("a malformed guard is preserved and cannot silently create a second lock", async () => {
  const directory=await mkdtemp(path.join(os.tmpdir(),"nexus-gate-bad-guard-"));
  const guard=path.join(directory,".verification-guard.sqlite");
  const bytes=Buffer.from("not a lock database");
  try {
    await writeFile(guard,bytes,{mode:0o600});
    const result=await verificationAttempt(directory,"bad-guard-build",async()=>assert.fail("invalid guard admitted"));
    assert.equal(result.report.status,"failed");
    assert.deepEqual(await readFile(guard),bytes);
  } finally {await rm(directory,{recursive:true,force:true});}
});

test("process death releases the kernel lock but never invents completion of running work", { timeout: 20000 }, async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "nexus-gate-crash-"));
  const moduleUrl = new URL("../src-tauri/scripts/release-gate.mjs", import.meta.url).href;
  let child;
  try {
    for (const status of ["failed", "running"]) {
      const buildId = `crashed-${status}`;
      const script = `import {verificationAttempt} from ${JSON.stringify(moduleUrl)};
        await verificationAttempt(process.argv[1],process.argv[2],async(report,dir,save)=>{
          report.status=process.argv[3];await save();process.send({ready:true});await new Promise(()=>{});
        });`;
      child = spawn(process.execPath, ["--input-type=module", "-e", script, directory, buildId, status], { windowsHide: true, stdio: ["ignore", "pipe", "pipe", "ipc"] });
      await once(child, "message");
      const live = await verificationAttempt(directory, buildId, async () => assert.fail("live process bypassed"));
      assert.match(live.report.error, /active verification/);
      const exited = once(child, "exit"); child.kill("SIGKILL"); await exited; child = null;
      const retried = await verificationAttempt(directory, buildId, async report => { assert.equal(status, "failed"); report.status = "passed"; });
      assert.equal(retried.report.status, status === "failed" ? "passed" : "failed");
      if (status === "running") assert.match(retried.report.error, /incomplete verification/);
      assert.ok(retried.report.previousLock);
      assert.equal((await readdir(path.join(directory, `verify-${buildId}`))).includes("active.lock"), false);
    }
  } finally {
    if (child && child.exitCode === null && child.signalCode === null) { const exited=once(child,"exit");child.kill("SIGKILL");await exited; }
    await rm(directory, { recursive: true, force: true });
  }
});

test("local release produces one multilingual EXE", () => {
  assert.deepEqual(packageOutputs("0.1.2", "acceptance-build"), [{
    kind: "nsis", locale: "multilingual",
    source: "Nexus Launcher_0.1.2_x64-setup.exe",
    file: "NexusLauncher_0.1.2_acceptance-build_x64.exe",
  }]);
});

test("release identity rejects unsafe and development identifiers", () => {
  assert.equal(selectBuildId("20260907T000000Z-12345678"), "20260907T000000Z-12345678");
  for (const value of ["development", "../outside", "bad id", "a".repeat(97)]) assert.throws(() => selectBuildId(value));
});
test("release source binding detects edits, deletions and new files but excludes generated output", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "nexus-release-gate-"));
  const git = args => execFileSync("git", args, { cwd: directory, windowsHide: true, stdio: "pipe" });
  try {
    git(["init", "--quiet"]);
    await writeFile(path.join(directory, ".gitignore"), "generated\n");
    await writeFile(path.join(directory, "source"), "one");
    git(["add", "."]);
    const original = await sourceIdentity(directory);
    await writeFile(path.join(directory, "generated"), "ignored build output");
    assert.equal((await sourceIdentity(directory)).sha256, original.sha256);
    await writeFile(path.join(directory, "source"), "two");
    assert.notEqual((await sourceIdentity(directory)).sha256, original.sha256);
    await writeFile(path.join(directory, "source"), "one");
    await writeFile(path.join(directory, "new-source"), "new");
    assert.notEqual((await sourceIdentity(directory)).sha256, original.sha256);
    await rm(path.join(directory, "new-source"));
    await rm(path.join(directory, "source"));
    const deleted = await sourceIdentity(directory);
    assert.notEqual(deleted.sha256, original.sha256);
    assert.equal(deleted.files.find(file => file.path === "source").deleted, true);
  } finally {
    // Only the exact directory created above is owned by this test.
    await rm(directory, { recursive: true, force: true });
  }
});


test("MSI cleanup excludes passive and upgrade removal while retaining Basic ARP consent", async () => {
  const source = await readFile(new URL("../src-tauri/installers/shutdown.wxs", import.meta.url), "utf8");
  const condition = source.match(/<Custom Action="NexusAskDataCleanup"[^>]*>([^<]+)<\/Custom>/)?.[1];
  assert.equal(condition, 'Installed AND REMOVE="ALL" AND NOT UPGRADINGPRODUCTCODE AND UILevel &gt;= 3 AND NOT (REBOOTPROMPT="S")');
  // /passive sets REBOOTPROMPT=S; ordinary ARP uninstall can also use UILevel=3.
  // Keep the Basic threshold: raising it to 5 removes normal ARP consent.
});
