import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import { completedDevToolsPort } from './linux-rpm-preflight-lifecycle.mjs';

export const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
export async function until(child, probe, timeout = 60000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (child && (child.exitCode !== null || child.signalCode !== null))
      throw new Error(`Owned GUI exited: ${child.exitCode ?? child.signalCode}`);
    const value = await probe(); if (value) return value;
    await delay(250);
  }
  throw new Error('Owned GUI readiness timeout');
}

export async function socket(url) {
  const address = new URL(url);
  assert.equal(address.protocol, 'ws:'); assert.equal(address.hostname, '127.0.0.1');
  const ws = new WebSocket(url), pending = new Map(); let next = 0;
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { ws.close(); reject(new Error('Local CDP timeout')); }, 5000);
    ws.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
    ws.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Local CDP connection failed')); }, { once: true });
  });
  ws.addEventListener('message', event => {
    const message = JSON.parse(event.data), receiver = pending.get(message.id);
    if (receiver) { pending.delete(message.id); message.error ? receiver.reject(new Error(JSON.stringify(message.error))) : receiver.resolve(message.result); }
  });
  ws.addEventListener('close', () => {
    for (const receiver of pending.values()) receiver.reject(new Error('Local CDP closed'));
    pending.clear();
  });
  const cdp = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++next, timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, method === 'Runtime.evaluate' ? 120000 : 10000);
    pending.set(id, { resolve: value => { clearTimeout(timer); resolve(value); }, reject: error => { clearTimeout(timer); reject(error); } });
    try { ws.send(JSON.stringify({ id, method, params })); }
    catch (error) { pending.delete(id); clearTimeout(timer); reject(error); }
  });
  const evaluate = async expression => {
    const response = await cdp('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
    if (response.exceptionDetails) {
      const error = response.exceptionDetails;
      const message = String(error.exception?.description ?? error.text)
        .replace(/([?&]token=)[^\s&"']+/gi, '$1[redacted]').slice(0, 4096);
      throw new Error('Renderer evaluation failed: ' + message);
    }
    return response.result.value;
  };
  return { ws, cdp, evaluate };
}

export async function connect(child, userData, selectPage) {
  const port = await until(child, async () => {
    try {
      const value = completedDevToolsPort(await fs.readFile(path.join(userData, 'DevToolsActivePort'), 'utf8'));
      if (!value) return null;
      const response = await fetch(`http://127.0.0.1:${value}/json/version`, { signal: AbortSignal.timeout(5000) });
      assert.equal(response.status, 200); return value;
    } catch (error) {
      if (error.code === 'ENOENT' || error.cause?.code === 'ECONNREFUSED') return null;
      throw error;
    }
  });
  const get = async endpoint => {
    const response = await fetch(`http://127.0.0.1:${port}/${endpoint}`, { signal: AbortSignal.timeout(5000) });
    assert.equal(response.status, 200); return response.json();
  };
  const page = await until(child, async () => (await get('json/list')).find(selectPage));
  const channel = await socket(page.webSocketDebuggerUrl);
  try {
    const host = await socket((await get('json/version')).webSocketDebuggerUrl);
    return { ...channel, host, port, getPages: () => get('json/list') };
  } catch (error) { channel.ws.close(); throw error; }
}
