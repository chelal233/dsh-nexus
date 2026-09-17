import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';

// The two independently-lived Electron hosts share only focus and event receipts.
// No URL, credential or conversation content is persisted here.
export class NoticeCoordinator {
  constructor(root, role, now = Date.now) {
    this.root = path.join(root, 'notification-hosts'); this.role = role; this.now = now;
    this.owner = `${process.pid}-${randomUUID()}`;
    fs.mkdirSync(this.root, { recursive: true, mode: 0o700 });
    this.file = path.join(this.root, `${role}.json`);
  }
  heartbeat(focused) {
    const temp = `${this.file}.${this.owner}.tmp`;
    fs.writeFileSync(temp, JSON.stringify({ owner: this.owner, focused: focused === true, time: this.now() }), { mode: 0o600 });
    fs.renameSync(temp, this.file);
  }
  peer(role) {
    try {
      const file = path.join(this.root, `${role}.json`);
      if (fs.statSync(file).size > 1024) return;
      const value = JSON.parse(fs.readFileSync(file, 'utf8'));
      if (this.now() - value.time < 5000 && this.now() >= value.time && typeof value.owner === 'string') return value;
    } catch { /* A closed or interrupted host is not a focus owner. */ }
  }
  focused() { return ['launcher', 'shell'].some(role => this.peer(role)?.focused === true); }
  claim(epoch, event) {
    const id = createHash('sha256').update(`${epoch}:${event.sequence}:${event.id ?? event.kind}`).digest('hex');
    try { fs.writeFileSync(path.join(this.root, `event-${id}`), '', { flag: 'wx', mode: 0o600 }); return true; }
    catch (e) { if (e.code === 'EEXIST') return false; throw e; }
  }
  prune() {
    for (const name of fs.readdirSync(this.root)) {
      if (!/^event-[a-f0-9]{64}$/.test(name)) continue;
      const file = path.join(this.root, name);
      try { if (this.now() - fs.statSync(file).mtimeMs > 86400000) fs.unlinkSync(file); } catch {}
    }
  }
  close() {
    try { if (JSON.parse(fs.readFileSync(this.file, 'utf8')).owner === this.owner) fs.unlinkSync(this.file); } catch {}
  }
}
