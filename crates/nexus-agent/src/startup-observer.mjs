import { StringDecoder } from 'node:string_decoder';
import { parseSkippedBundles } from './startup-diagnosis.mjs';

// Preloaded before the managed CLI imports profile bundles. Keep only explicit
// native skip records in memory; the official appReady commit seals the result.
export function observeSkippedBundles(streams) {
  const rows = [], hooks = [];
  let active = true, truncated = false, bytes = 0, sealed;
  const prefix = /^dsh(?: [\w-]+){0,3}: skipping profile bundle /;
  const record = line => {
    const size = Buffer.byteLength(line);
    if (rows.includes(line)) return;
    if (rows.length >= 48 || bytes + size > 16000) { truncated = true; return; }
    rows.push(line); bytes += size;
  };
  for (const stream of streams) {
    const original = stream.write;
    let decoder = new StringDecoder('utf8'), pending = '', overflow = false, afterSkip = false;
    const collect = line => {
      if (!prefix.test(line)) {
        if (line && afterSkip) truncated = true;
        if (line) afterSkip = false;
        return;
      }
      afterSkip = true;
      record(line);
    };
    const capture = text => {
      for (const part of text.split(/(\n)/)) {
        if (part === '\n') {
          collect(pending.replace(/\r$/, ''));
          pending = ''; overflow = false;
        } else if (!overflow) {
          pending += part;
          if (pending.length > 16000) {
            if (prefix.test(pending)) truncated = true;
            pending = pending.slice(0, 16000); overflow = true;
          }
        }
      }
    };
    function write(chunk, ...args) {
      if (active) {
        if (typeof chunk === 'string') {
          capture(decoder.end() + chunk); decoder = new StringDecoder('utf8');
        } else if (chunk instanceof Uint8Array) capture(decoder.write(chunk));
      }
      return Reflect.apply(original, this, [chunk, ...args]);
    }
    stream.write = write;
    hooks.push(() => {
      capture(decoder.end()); collect(pending.replace(/\r$/, ''));
      if (stream.write === write) stream.write = original;
    });
  }
  return () => {
    if (!sealed) {
      active = false;
      for (const finish of hooks) finish();
      sealed = parseSkippedBundles(rows.join('\n'));
      sealed.truncated ||= truncated;
    }
    return sealed;
  };
}

if (process.env.NEXUS_HOST_STARTUP_FILE && process.env.NEXUS_BROWSER_HEALTH_RUN) {
  globalThis[Symbol.for('nexus.startup.skipped-bundles')] = observeSkippedBundles([process.stdout, process.stderr]);
}
