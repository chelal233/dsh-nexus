import { spawn } from 'node:child_process';
import path from 'node:path';

// Keep the outer transport alive beyond the Agent client's bounded operation.
// Compatibility checks already have a 660 s budget in nexus-launcher-core.
export function requestTimeout(command, args) {
  if (command === 'proxy_request' && args.method === 'POST' && args.path === '/v1/plugin-manager') return 960000;
  const compatibility = command === 'proxy_request' && args.method === 'POST' &&
    (['/v1/releases', '/v1/harness', '/v1/market'].includes(args.path) ||
      (args.path === '/v1/profiles' && ['select', 'compatibility_check'].includes(args.body?.action)));
  return compatibility ? 690000 : 120000;
}

export class RustBridge {
  #child;
  #pending = new Map();
  #nextId = 0;
  #closed = false;
  constructor(resources, spawnProcess = spawn, environment = {}) {
    this.resources = resources; this.spawnProcess = spawnProcess; this.environment = environment;
    this.#start();
  }
  #start() {
    const resources = this.resources;
    const child = this.#child = this.spawnProcess(path.join(resources, process.platform === 'win32' ? 'nexus-desktop-bridge.exe' : 'nexus-desktop-bridge'), [], {
      windowsHide: true, stdio: ['pipe', 'pipe', 'inherit'],
      env: { ...process.env, ...this.environment, NEXUS_DESKTOP_RESOURCES: resources },
    });
    let buffer = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      if (this.#child !== child) return;
      buffer += chunk;
      if (Buffer.byteLength(buffer) > 16 * 1024 * 1024) return this.#fail(new Error('Desktop response exceeds limit'), child);
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        try {
          const message = JSON.parse(line);
          const pending = this.#pending.get(message.id);
          if (!pending) continue;
          this.#pending.delete(message.id); clearTimeout(pending.timer);
          if (message.error) pending.reject(message.error); else pending.resolve(message.value);
        } catch { this.#fail(new Error('Invalid desktop response'), child); return; }
      }
    });
    child.on('error', error => this.#fail(error, child));
    child.on('exit', () => this.#fail(new Error('Rust desktop adapter exited; check operation status before retrying'), child));
    child.stdin.on('error', error => this.#fail(error, child));
  }
  #fail(error, child) {
    if (this.#child !== child) return;
    this.#child = undefined; child.kill();
    for (const entry of this.#pending.values()) { clearTimeout(entry.timer); entry.reject(error); }
    this.#pending.clear();
  }
  request(command, args = {}) {
    if (this.#closed) return Promise.reject(new Error('Rust desktop adapter closed'));
    // Recover transport for the NEXT request, never replay an uncertain mutation.
    if (!this.#child) this.#start();
    if (this.#pending.size >= 64) return Promise.reject(new Error('Too many desktop requests'));
    return new Promise((resolve, reject) => {
      const id = ++this.#nextId;
      const timer = setTimeout(() => { this.#pending.delete(id); reject(new Error('Desktop request timed out; check operation status before retrying')); }, requestTimeout(command, args));
      this.#pending.set(id, { resolve, reject, timer });
      this.#child.stdin.write(`${JSON.stringify({ id, command, args })}\n`);
    });
  }
  close() { this.#closed = true; this.#child?.stdin.end(); }
}
