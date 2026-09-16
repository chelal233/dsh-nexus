import { spawn } from 'node:child_process';
import path from 'node:path';

export class RustBridge {
  #child;
  #pending = new Map();
  #nextId = 0;
  constructor(resources) {
    this.#child = spawn(path.join(resources, process.platform === 'win32' ? 'nexus-desktop-bridge.exe' : 'nexus-desktop-bridge'), [], {
      windowsHide: true, stdio: ['pipe', 'pipe', 'inherit'],
      env: { ...process.env, NEXUS_DESKTOP_RESOURCES: resources },
    });
    let buffer = '';
    this.#child.stdout.setEncoding('utf8');
    this.#child.stdout.on('data', chunk => {
      buffer += chunk;
      if (Buffer.byteLength(buffer) > 16 * 1024 * 1024) return this.#fail(new Error('Desktop response exceeds limit'));
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        try {
          const message = JSON.parse(line);
          const pending = this.#pending.get(message.id);
          if (!pending) continue;
          this.#pending.delete(message.id); clearTimeout(pending.timer);
          if (message.error) pending.reject(message.error); else pending.resolve(message.value);
        } catch { this.#fail(new Error('Invalid desktop response')); }
      }
    });
    this.#child.on('error', error => this.#fail(error));
    this.#child.on('exit', () => this.#fail(new Error('Rust desktop adapter exited')));
    this.#child.stdin.on('error', error => this.#fail(error));
  }
  #fail(error) {
    for (const entry of this.#pending.values()) { clearTimeout(entry.timer); entry.reject(error); }
    this.#pending.clear();
  }
  request(command, args = {}) {
    if (this.#child.exitCode !== null || this.#child.killed) return Promise.reject(new Error('Rust desktop adapter unavailable'));
    if (this.#pending.size >= 64) return Promise.reject(new Error('Too many desktop requests'));
    return new Promise((resolve, reject) => {
      const id = ++this.#nextId;
      const timer = setTimeout(() => { this.#pending.delete(id); reject(new Error('Desktop request timed out; check operation status before retrying')); }, 120000);
      this.#pending.set(id, { resolve, reject, timer });
      this.#child.stdin.write(`${JSON.stringify({ id, command, args })}\n`);
    });
  }
  close() { this.#child.stdin.end(); }
}
