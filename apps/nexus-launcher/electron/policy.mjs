export const commands = new Set([
  'harness_desktop_capability', 'harness_desktop_status', 'harness_desktop_start', 'harness_desktop_stop', 'startup_status', 'retry_startup', 'proxy_request', 'build_identity',
  'choose_local_path', 'set_native_locale', 'set_native_notifications',
  'update_tray', 'export_startup_diagnostics', 'autostart_status', 'autostart_set',
  'agent_log_set', 'notify', 'update_status', 'update_check', 'update_download', 'update_settings', 'update_install',
  'notification_test', 'update_release_notes', 'harness_desktop_restart',
]);
export const events = new Set(['nexus-native-error', 'nexus-tray-action', 'nexus-update']);

export function harnessUrl(raw) {
  const url = new URL(raw);
  if (url.protocol !== 'http:' || !['127.0.0.1', '[::1]', 'localhost'].includes(url.hostname)
      || url.username || url.password || !url.port) throw new Error('Invalid local Harness URL');
  return url;
}

export function trustedFrame(event, window, documentUrl) {
  return !window.isDestroyed() && event.sender === window.webContents
    && event.senderFrame === window.webContents.mainFrame
    && event.senderFrame.url === documentUrl;
}

export function validateRequest(command, args) {
  if (!commands.has(command)) throw new Error('Unknown desktop command');
  if (args !== undefined && (args === null || Array.isArray(args) || typeof args !== 'object')) {
    throw new Error('Desktop arguments must be an object');
  }
  if (Buffer.byteLength(JSON.stringify(args ?? {})) > 32768) throw new Error('Desktop request is too large');
}
export function nativeEditAction(platform, input) {
  if (platform !== 'darwin' || input.type !== 'keyDown' || !input.meta || input.control || input.alt) return undefined;
  return { a: 'selectAll', c: 'copy', x: 'cut', v: 'paste', z: input.shift ? 'redo' : 'undo' }[input.key.toLowerCase()];
}
