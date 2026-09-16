import assert from "node:assert/strict";
import { test } from "node:test";
import { recordArtifact } from "../desktop/scripts/prepare-agent.mjs";

test("staging follows Cargo's actual executable even for fresh custom-target builds", () => {
  const artifacts = new Map();
  const names = ["nexus-agent.exe"];
  const message = { reason: "compiler-artifact", target: { kind: ["bin"] },
    profile: { test: false }, fresh: true, executable: "X:/custom/x86_64-pc-windows-msvc/release/nexus-agent.exe" };
  recordArtifact(message, artifacts, names);
  assert.equal(artifacts.get(names[0]), message.executable);
  recordArtifact({ ...message, executable: "X:/other/nexusctl.exe" }, artifacts, names);
  recordArtifact({ ...message, profile: { test: true }, executable: "X:/tests/nexus-agent.exe" }, artifacts, names);
  assert.equal(artifacts.size, 1);
  assert.throws(() => recordArtifact({ ...message, executable: "X:/old/nexus-agent.exe" }, artifacts, names), /Ambiguous/);
});
