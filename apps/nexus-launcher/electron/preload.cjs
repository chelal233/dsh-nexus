const { contextBridge, ipcRenderer } = require('electron');
// No Node, IPC object, filesystem, URL opener, or channel selection is exposed.
const commands = new Set(['harness_desktop_capability', 'harness_desktop_status', 'harness_desktop_start', 'harness_desktop_stop', 'startup_status', 'retry_startup', 'proxy_request', 'build_identity',
  'choose_local_path', 'set_native_locale', 'set_native_notifications', 'update_tray',
  'export_startup_diagnostics', 'autostart_status', 'autostart_set', 'agent_log_set',
  'notify', 'notification_test', 'update_status', 'update_check', 'update_settings', 'update_install']);
const events = new Set(['nexus-native-error', 'nexus-tray-action', 'nexus-update']);
contextBridge.exposeInMainWorld('nexusDesktop', Object.freeze({
  systemLanguages: JSON.parse(process.argv.find(arg => arg.startsWith('--nexus-system-languages='))?.slice('--nexus-system-languages='.length) || '[]'),
  async invoke(command, args) {
    if (!commands.has(command)) throw new Error('Unknown desktop command');
    const result = await ipcRenderer.invoke('nexus:command', command, args);
    if (result.error) throw result.error;
    return result.value;
  },
  listen(name, callback) {
    if (!events.has(name) || typeof callback !== 'function') throw new Error('Unknown desktop event');
    const listener = (_event, payload) => callback({ payload });
    ipcRenderer.on(name, listener);
    return () => ipcRenderer.removeListener(name, listener);
  },
}));
