import assert from 'node:assert/strict';

export function completedDevToolsPort(text) {
  // Chromium creates/truncates this file before writing the two-line value.
  // Do not interpret an empty file or a partially written first line as ready.
  const newline = text.indexOf('\n');
  if (newline < 0) return null;
  const port = text.slice(0, newline);
  assert.match(port, /^\d+$/);
  assert.ok(Number(port) > 0 && Number(port) <= 65535, 'Invalid DevTools port');
  return port;
}

export function releaseOwnedChildHandles(child) {
  // A grandchild can retain inherited pipes after the owned child has exited.
  // Call only after recording the failed bounded cleanup; this is not proof
  // of descendant termination. The caller's container cleanup remains required.
  child.stdout?.destroy();
  child.stderr?.destroy();
  child.unref();
}
