import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { inventoryResources, verifyInventory, releaseIdentity, verifyIdentity, verifyRuntimeVersions, verifyAgentIdentity } from "../desktop/scripts/prepare-release.mjs";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

test("compiled Agent identity must match the package, including old or development builds", () => {
  const manifest = { buildId: "package", version: "0.1.0" };
  verifyAgentIdentity(manifest, manifest);
  for (const buildId of ["old", "development", undefined]) assert.throws(() => verifyAgentIdentity(manifest, { buildId, version: "0.1.0" }), /Compiled Agent identity/);
  assert.throws(() => verifyAgentIdentity(manifest, { ...manifest, version: "old" }), /Compiled Agent identity/);
});

test("actual bundled versions must agree with the runtime declaration", { skip: !["win32", "darwin"].includes(process.platform) }, () => {
  const resources = fileURLToPath(new URL("../desktop/resources/", import.meta.url));
  const runtime = JSON.parse(readFileSync(path.join(resources, "runtime/manifest.json"), "utf8"));
  verifyRuntimeVersions(resources, runtime);
  assert.throws(() => verifyRuntimeVersions(resources, { ...runtime, target: "wrong-architecture" }), /architecture mismatch/);
  assert.throws(() => verifyRuntimeVersions(resources, { ...runtime, pnpm: { ...runtime.pnpm, version: "old-version" } }), /versions disagree/);
});

test("an old GUI identity cannot pass verification with a newer manifest", () => {
  const manifest = { version: "0.1.0", buildId: "new-build", createdAt: "now", commit: "revision", dirty: true,
    runtime: { node: { version: "v24", npmVersion: "11" }, pnpm: { version: "11" } } };
  const identity = releaseIdentity(manifest, "digest");
  verifyIdentity(manifest, identity, "digest");
  assert.throws(() => verifyIdentity(manifest, { ...identity, buildId: "old-build" }, "digest"), /does not match/);
  assert.throws(() => verifyIdentity(manifest, identity, "changed-digest"), /does not match/);
});

test("release inventory detects swapped old binaries and transitive runtime changes", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "nexus-release-"));
  try {
    await mkdir(path.join(root, "runtime/node"), { recursive: true });
    await writeFile(path.join(root, "agent.exe"), "new executable");
    await writeFile(path.join(root, "runtime/node/npm.js"), "complete npm");
    await writeFile(path.join(root, "runtime/node/.gitkeep"), "");
    await writeFile(path.join(root, "runtime/node/.DS_Store"), "metadata");
    const files = await inventoryResources(root, ["agent.exe", "runtime"]);
    assert.equal(files.length, 2);
    await rm(path.join(root, "runtime/node/.gitkeep"));
    await rm(path.join(root, "runtime/node/.DS_Store"));
    await verifyInventory(root, files);
    await writeFile(path.join(root, "agent.exe"), "old executable");
    await assert.rejects(verifyInventory(root, files), /changed: agent.exe/);
    await writeFile(path.join(root, "agent.exe"), "new executable");
    await writeFile(path.join(root, "runtime/node/npm.js"), "broken npm");
    await assert.rejects(verifyInventory(root, files), /changed: runtime\/node\/npm.js/);
    await assert.rejects(verifyInventory(root, [{ path: "../outside" }]), /Invalid/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
