import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// Compare reports from identical probes in the isolated Rust A and B workspaces.
const directory = path.resolve(process.argv[2] || "");
assert.ok(process.argv[2], "Usage: node scripts/ab-checkpoint.mjs <baseline-directory>");
const hash = text => createHash("sha256").update(text).digest("hex");
const reports = ["A", "B"].map(side => {
  const report = JSON.parse(readFileSync(path.join(directory, "rust", `${side}.json`), "utf8"));
  const crate = path.join(directory, "rust", side, "crates/nexus-agent");
  assert.equal(path.resolve(report.manifest), crate, "Cargo reused the wrong workspace artifact");
  assert.equal(report.implementation, readFileSync(path.join(crate, "src/checkpoint_api.rs"), "utf8"),
    "The executed binary does not contain this implementation");
  const tests = readFileSync(path.join(crate, "src/tests/checkpoint_tests.rs"), "utf8");
  const probe = readFileSync(new URL("../tests/ab-checkpoint-probe.rs", import.meta.url), "utf8");
  assert.ok(tests.trimEnd().endsWith(probe.trimEnd()), "Both workspaces must use the same observation probe");
  return report;
});
const candidate = readFileSync(fileURLToPath(new URL("../../../crates/nexus-agent/src/checkpoint_api.rs", import.meta.url)), "utf8");
assert.equal(reports[1].implementation, candidate, "B is stale");
assert.deepEqual(reports[1].observations, reports[0].observations);
// The reused response mapper must preserve every original arm, including error text.
const matchBody = source => source.match(/match (?:result_rx\.await|result) \{([\s\S]*?)\n\s*\};?(?:\n|$)/)[1];
const tokens = source => {
  const parts = source.match(/"(?:\\.|[^"\\])*"|[A-Za-z_][A-Za-z_0-9]*|\S/g);
  return parts.filter((part, index) => !(part === "," && parts[index + 1] === ")"));
};
const mapper = matchBody(candidate.slice(candidate.indexOf("fn content_restore_http_result(")));
for (const [entry, code, message] of [
  ["pub(super) async fn checkpoint_restore(", "checkpoint_restore_failed", "checkpoint restore owner exited without a result"],
  ["fn content_restore_http_result(", "checkpoint_restore_retry_failed", "checkpoint retry owner exited without a result"],
]) {
  const before = matchBody(reports[0].implementation.slice(reports[0].implementation.indexOf(entry)));
  const after = mapper.replaceAll("failure_code", JSON.stringify(code)).replaceAll("owner_error", JSON.stringify(message));
  assert.deepEqual(tokens(after), tokens(before), "Response mapping changed");
}
const result = { status: "passed", baseline: hash(reports[0].implementation), candidate: hash(candidate),
  cases: reports[0].observations.map(item => item.scenario),
  responseMappingTokensIdentical: true,
  scope: "Local restore HTTP results, catalogs, journal, runtime persistence and cancellation; no installer or real Harness acceptance" };
writeFileSync(path.join(directory, "rust/comparison.json"), JSON.stringify(result, null, 2));
console.log(JSON.stringify(result, null, 2));
