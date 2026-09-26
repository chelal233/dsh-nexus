import fs from 'node:fs';
import path from 'node:path';

export function showDesktopWindow(windows) {
  const candidates = windows.filter(window => !window.isDestroyed() && window.webContents.getType() !== 'devTools');
  const window = candidates.find(window => window.isVisible() && window.getParentWindow())
    ?? candidates.find(window => !window.getParentWindow());
  if (!window) throw new Error('desktop_window_unavailable');
  if (window.isMinimized()) window.restore();
  window.show();
  window.focus();
}

// Private per-launch mailbox. Only a fixed show operation is accepted; no
// renderer IPC, arbitrary URL, script, or OS-wide protocol handler is exposed.
export function installDesktopWindowControl({ app, BrowserWindow, recipe }) {
  const file = path.join(path.dirname(recipe.stateFile), `show-${recipe.operationId}.json`);
  const reply = `${file}.ack`;
  fs.writeFileSync(`${file}.capability`, JSON.stringify({ operationId: recipe.operationId, pid: process.pid }), { mode: 0o600 });
  const timer = setInterval(() => {
    if (!fs.existsSync(file)) return;
    try {
      const meta = fs.lstatSync(file);
      if (!meta.isFile() || meta.isSymbolicLink() || meta.size > 1024) return;
      const request = JSON.parse(fs.readFileSync(file, 'utf8'));
      fs.unlinkSync(file);
      if (!Number.isFinite(request.expiresAt) || Date.now() > request.expiresAt || request.operationId !== recipe.operationId || !/^[a-f0-9-]{36}$/.test(request.requestId ?? '')) return;
      let error;
      try { showDesktopWindow(BrowserWindow.getAllWindows()); } catch (cause) { error = cause.message; }
      const temporary = `${reply}.tmp`;
      fs.writeFileSync(temporary, JSON.stringify({ ...request, error }), { mode: 0o600 });
      fs.renameSync(temporary, reply);
    } catch (error) { console.warn('Desktop window request failed:', error.message); }
  }, 100);
  timer.unref();
  app.once('will-quit', () => clearInterval(timer));
}
