import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawnSync } from "node:child_process";

test("terminal uses exact paths and profile only after durable registration", { skip: process.platform !== "win32" }, () => {
  const source = fs.readFileSync(new URL("../../../crates/nexus-agent/src/profile_api.rs", import.meta.url), "utf8");
  const init = source.match(/const DSH_TERMINAL_INIT: &str = r#"([\s\S]*?)"#;/)![1];
  const wait = source.match(/const DSH_TERMINAL_WAIT: &str = r#"([\s\S]*?)"#;/)![1];
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "nexus-terminal-regression-"));
  const directory = path.join(root, "space %UNEXPECTED% ! ' folder"); fs.mkdirSync(directory);
  const entry = path.join(directory, "entry.cjs");
  fs.writeFileSync(entry, "console.log(JSON.stringify(process.argv.slice(2)))");
  const ready = path.join(root, "ready");
  const systemRoot = Object.entries(process.env).find(([key]) => key.toLowerCase() === "systemroot")?.[1];
  assert.ok(systemRoot);
  const shell = path.join(systemRoot, "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
  const env = { ...process.env, NEXUS_TERMINAL_READY: ready, NEXUS_TERMINAL_NODE: process.execPath,
    NEXUS_TERMINAL_ENTRY: entry, NEXUS_TERMINAL_PROFILE: "custom-profile",
    NEXUS_TERMINAL_NPM: entry, NEXUS_TERMINAL_PNPM: entry, NEXUS_TERMINAL_PNPM_SCRIPT: "1" };
  const invoke = (command: string) => spawnSync(shell, ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
    // The script itself waits up to ten seconds for registration. Hosted
    // Windows PowerShell cold startup needs additional headroom under load.
    { env, encoding: "utf8", timeout: 60000 });
  try {
    const aborted = invoke(wait + init + "; dsh --help");
    assert.equal(aborted.error, undefined, aborted.error?.message);
    assert.equal(aborted.status, 1, aborted.stderr); assert.equal(aborted.stdout, "");
    fs.writeFileSync(ready, "ready");
    const accepted = invoke(wait + init + '; dsh --marker "space %UNEXPECTED% !"');
    assert.equal(accepted.status, 0, accepted.stderr);
    assert.deepEqual(JSON.parse(accepted.stdout), ["--profile", "custom-profile", "--marker", "space %UNEXPECTED% !"]);
    assert.equal(fs.existsSync(ready), false);
    const pnpm = invoke(init + "; pnpm --version");
    assert.equal(pnpm.status, 0, pnpm.stderr); assert.deepEqual(JSON.parse(pnpm.stdout), ["--version"]);
    // Bare npm must use the selected runtime, not PowerShell's npm.ps1 shim.
    fs.writeFileSync(path.join(directory, "npm.ps1"), 'throw "Wrong npm entry"');
    const npm = spawnSync(shell, ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Restricted", "-Command",
      '$ErrorActionPreference = "Stop"; ' + init + '; npm --marker "space %UNEXPECTED% !"; exit $LASTEXITCODE'],
      { env: { ...env, PATH: directory + path.delimiter + process.env.PATH }, encoding: "utf8", timeout: 15000 });
    assert.equal(npm.status, 0, npm.stderr);
    assert.deepEqual(JSON.parse(npm.stdout), ["--marker", "space %UNEXPECTED% !"]);
  } finally {
    assert.equal(path.dirname(root), os.tmpdir()); assert.ok(path.basename(root).startsWith("nexus-terminal-regression-"));
    fs.rmSync(root, { recursive: true });
  }
});
