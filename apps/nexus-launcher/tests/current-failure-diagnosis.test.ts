import assert from "node:assert/strict";
import test from "node:test";
import { createUiTestLoader } from "./ui-test-loader.ts";
test("actual startup diagnosis rejects stale and running logs", async () => {
 const loader = await createUiTestLoader();
 try {
  const { currentFailureDiagnosis } = await loader.loadModule("/src/App.tsx");
  const runtime = {state: "failed", started_at_unix: 100, updated_at_unix: 110, exit_code: 1};
  const snapshot = {harnessRuntime:{harness:runtime}, profiles:{compatibility:{status:"passed",last_used_at_unix:99}}, recovery:{harness:runtime,log_tail:[{content:"dsh: startup failed: 2 required plugins did not activate\nError: listen EADDRINUSE: address already in use 127.0.0.1:3852\nPlugins waiting for services (15):"}]}};
  assert.equal(currentFailureDiagnosis(snapshot).code,"port_conflict");
  assert.equal(currentFailureDiagnosis({...snapshot,profiles:{compatibility:{status:"passed",last_used_at_unix:120}}}).code,"port_conflict");
  assert.equal(currentFailureDiagnosis({...snapshot,recovery:{...snapshot.recovery,harness:{...runtime,started_at_unix:90}}}),null);
  assert.equal(currentFailureDiagnosis({...snapshot,harnessRuntime:{harness:{...runtime,state:"running"}}}),null);
  assert.equal(currentFailureDiagnosis({...snapshot,lifecycleBusy:true}),null);
  assert.equal(currentFailureDiagnosis({...snapshot,recovery:{...snapshot.recovery,log_tail:[{content:"Error: task-board ledger is already owned by process 29864"}]}}).code,"process_lock");
 } finally {await loader.close();}
});
