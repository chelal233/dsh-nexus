import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { EventEmitter } from 'node:events';
import { randomUUID } from 'node:crypto';
import { showDesktopWindow, installDesktopWindowControl } from '../electron/desktop-window.mjs';
import { trayEntries } from '../electron/tray-menu.mjs';
import { HarnessDesktop } from '../electron/harness-desktop.mjs';

const fakeWindow = (parent = null) => {
  const calls = [];
  return { calls, isDestroyed: () => false, webContents: { getType: () => 'window' }, isVisible: () => false,
    getParentWindow: () => parent, isMinimized: () => true, restore: () => calls.push('restore'), show: () => calls.push('show'), focus: () => calls.push('focus') };
};
test('restore a hidden workspace without restarting its process', () => {
  const window = fakeWindow();
  showDesktopWindow([window]);
  assert.deepEqual(window.calls, ['restore', 'show', 'focus']);
});
test('visible modal takes precedence over its hidden parent', () => {
  const parent = fakeWindow(); const modal = fakeWindow(parent); modal.isVisible = () => true;
  showDesktopWindow([parent, modal]);
  assert.deepEqual(parent.calls, []);
  assert.deepEqual(modal.calls, ['restore', 'show', 'focus']);
});
test('missing window fails rather than acknowledging success', () => assert.throws(() => showDesktopWindow([]), /desktop_window_unavailable/));
test('per-launch mailbox restores and acknowledges the current operation', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nexus-window-'));
  const app = new EventEmitter(); const window = fakeWindow();
  const desktop = new HarnessDesktop({ userData: root });
  const operationId = randomUUID();
  fs.mkdirSync(desktop.directory);
  fs.writeFileSync(desktop.file, JSON.stringify({ phase: 'launched', pid: process.pid, childPid: process.pid, operationId, canShowWindow: true }));
  installDesktopWindowControl({ app, BrowserWindow: { getAllWindows: () => [window] }, recipe: { stateFile: desktop.file, operationId } });
  try {
    const states = await Promise.all([desktop.start(), desktop.start()]);
    assert.equal(states[0].operationId, operationId);
    assert.deepEqual(window.calls, ['restore', 'show', 'focus']);
  } finally { app.emit('will-quit'); fs.rmSync(root, { recursive: true, force: true }); }
});
test('legacy independent host does not claim window-control support', async () => {
  const desktop = new HarnessDesktop({ userData: os.tmpdir() });
  desktop.status = () => ({ phase: 'launched', operationId: randomUUID() });
  await assert.rejects(desktop.start(), /desktop_window_unsupported/);
});

test('tray opens a live Desktop only when its host declares window support', () => {
  for (const canShowWindow of [true, false]) {
    const menu = trayEntries({ text: english => english, desktop: { phase: 'launched', canShowWindow }, supported: true, act() {}, show() {} });
    assert.equal(menu.find(item => item.id === 'desktop-group').submenu.find(item => item.id === 'desktop').enabled, canShowWindow);
  }
});
