import assert from "node:assert/strict";
import test from "node:test";
import { webcrypto } from "node:crypto";
import { createRequestClient, mergeRequestHistory, requiresRequestId } from "../src/request-client.ts";

test("profile archive mutations use durable retry references while listing stays read-only",()=>{
  assert.equal(requiresRequestId("/v1/profiles",{action:"delete"}),true);
  assert.equal(requiresRequestId("/v1/profiles",{action:"restore_deleted"}),true);
  assert.equal(requiresRequestId("/v1/profiles",{action:"deleted_list"}),false);
});

function storage() {
  const values = new Map<string, string>();
  return { getItem: (key = "nexus.pending-requests.v1.root-one") => values.get(key) ?? null, setItem: (key: string, next: string) => { values.set(key, next); } };
}
test("lost transport responses retain the same request ID across client recreation without storing payloads", async () => {
  const saved = storage(), ids: unknown[] = [];
  let fail = true;
  const transport = async (_: string, __: "GET" | "POST", body?: Record<string, unknown>) => {
    ids.push(body?.request_id);
    if (fail) throw { code: "agent_transport_error", message: "connection lost" };
    return { request: { state: "completed", http_status: 202 } };
  };
  const body = { action: "switch", tag: "v1", source: "sensitive-source-placeholder" };
  await assert.rejects(createRequestClient(saved, transport, "root-one", webcrypto as Crypto).post("/v1/updates", body));
  assert.ok(!saved.getItem()!.includes("sensitive-source-placeholder"));
  fail = false;
  await createRequestClient(saved, transport, "root-one", webcrypto as Crypto).post("/v1/updates", body);
  assert.equal(ids[0], ids[1]);
  await createRequestClient(saved, transport, "root-one", webcrypto as Crypto).post("/v1/updates", body);
  assert.notEqual(ids[1], ids[2], "a confirmed completed request allows a new intentional action");
});
test("interrupted requests retain identity until the user explicitly allows a new attempt", async () => {
  const saved = storage(), ids: string[] = [];
  const client = createRequestClient(saved, async (_p, _m, body) => {
    ids.push(body!.request_id as string);
    throw { code: "request_interrupted", status: 409, message: "inspect recovery" };
  }, "root-one", webcrypto as Crypto);
  for (let i = 0; i < 2; i++) await assert.rejects(client.post("/v1/releases", { action: "rollback" }));
  assert.equal(ids[0], ids[1]);
  client.forget(ids[0]);
  await assert.rejects(client.post("/v1/releases", { action: "rollback" }));
  assert.notEqual(ids[1], ids[2]);
});
test("known domain failures allow a corrected attempt and unrelated operations need no receipt", async () => {
  const saved = storage();
  const client = createRequestClient(saved, async (_p, _m, body) => {
    if (body!.action === "restore") throw { code: "checkpoint_missing", status: 404, message: "missing" };
    return body!;
  }, "root-one", webcrypto as Crypto);
  await assert.rejects(client.post("/v1/checkpoints", { action: "restore", id: "missing" }));
  assert.deepEqual(client.pending(), []);
  const ordinary = await client.post("/v1/harness", { action: "start" });
  assert.equal(ordinary.request_id, undefined);
});

test("persistent references survive a new storage handle and are isolated by data root", async () => {
  const durable = storage(), ids: string[] = [];
  const transport = async (_p: string, _m: "GET" | "POST", body?: Record<string, unknown>) => {
    ids.push(body!.request_id as string);
    throw { code: "agent_transport_error", message: "response lost" };
  };
  const newHandle = () => ({ getItem: (key: string) => durable.getItem(key), setItem: (key: string, value: string) => durable.setItem(key, value) });
  await assert.rejects(createRequestClient(newHandle(), transport, "root-one", webcrypto as Crypto).post("/v1/releases", { action: "rollback" }));
  await assert.rejects(createRequestClient(newHandle(), transport, "root-one", webcrypto as Crypto).post("/v1/releases", { action: "rollback" }));
  assert.equal(ids[0], ids[1]);
  await assert.rejects(createRequestClient(newHandle(), transport, "root-two", webcrypto as Crypto).post("/v1/releases", { action: "rollback" }));
  assert.notEqual(ids[1], ids[2]);
  assert.throws(() => createRequestClient(newHandle(), transport, "", webcrypto as Crypto));
});


test("expired orphan references are visible and require explicit release before a new operation", async () => {
  const saved = storage(), ids: string[] = [];
  let offline = true, expired = true;
  const client = createRequestClient(saved, async (_path, _method, body) => {
    ids.push(body!.request_id as string);
    if (offline) throw { code: "agent_transport_error", message: "not received" };
    if (expired) throw { code: "request_id_expired", status: 409, message: "retry window elapsed" };
    return { request: { state: "completed", http_status: 200 } };
  }, "root-one", webcrypto as Crypto);
  await assert.rejects(client.post("/v1/releases", { action: "rollback" }));
  offline = false;
  await assert.rejects(client.post("/v1/releases", { action: "rollback" }));
  assert.equal(ids[0], ids[1]);
  const server: Record<string, unknown>[] = [];
  const history = mergeRequestHistory(server, client.pending());
  assert.equal(history.length, 1);
  assert.equal(history[0].state, "unconfirmed");
  client.forget(history[0].request_id as string); // UI asks for confirmation first.
  assert.deepEqual(server, [], "clearing the reference never mutates server history");
  assert.deepEqual(client.pending(), []);
  assert.equal(ids.length, 2, "release itself sends no request");
  expired = false;
  await client.post("/v1/releases", { action: "rollback" });
  assert.notEqual(ids[2], ids[1]);
  const receipt = { request_id: ids[2], state: "interrupted" };
  assert.deepEqual(mergeRequestHistory([receipt], [ids[2]]), [receipt], "known receipt stays authoritative");
});

test("restart retries retain their original request identity", async()=>{
 const saved=storage(),ids:unknown[]=[];let fail=true;
 const transport=async (_:string,__:"GET"|"POST",body?:Record<string,unknown>)=>{ids.push(body?.request_id);if(fail)throw {code:"agent_transport_error"};return {request:{state:"completed",http_status:200}};};
 await assert.rejects(createRequestClient(saved,transport,"root-one",webcrypto as Crypto).post("/v1/harness",{action:"restart"}));
 fail=false;await createRequestClient(saved,transport,"root-one",webcrypto as Crypto).post("/v1/harness",{action:"restart"});
 assert.equal(typeof ids[0],"string");assert.equal(ids[0],ids[1]);
});
