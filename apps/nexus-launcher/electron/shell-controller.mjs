import { harnessUrl } from './policy.mjs';

// A single owner serializes discovery and navigation across Agent generations.
export class ShellController {
  current; stopped = false; busy = false; failed = true; revision = 0;
  constructor({ discover, load, recover, changed = () => {} }) {
    Object.assign(this, { discover, load, recover, changed });
  }
  async tick() {
    if (this.stopped || this.busy) return;
    this.busy = true;
    try {
      let info;
      try { info = await this.discover(); if (info.available === false) throw new Error('Harness is not ready'); }
      catch (error) { this.held = undefined; throw error; }
      const url = harnessUrl(info.url);
      if (this.stopped) return;
      if (this.held === url.href) return;
      if (this.failed || this.current?.href !== url.href) {
        this.current = url; // Set the trusted origin before load triggers navigation.
        const revision = this.revision;
        await this.load(url.href);
        if (this.stopped || revision !== this.revision) return;
        this.failed = false;
        this.changed();
      }
    } catch {
      // Never expose token-bearing URLs or raw network errors in the recovery page.
      if (!this.stopped && !this.failed) await this.recover().catch(() => {});
      this.failed = true;
    } finally { this.busy = false; }
  }
  async broken() {
    if (this.stopped) return;
    this.failed = true;
    this.revision++;
    this.held = this.current?.href;
    await this.recover().catch(() => {});
  }
  retry() { this.held = undefined; return this.tick(); }
  stop() { this.stopped = true; }
}
