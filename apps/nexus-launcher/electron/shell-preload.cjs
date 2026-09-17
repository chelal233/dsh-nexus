const { contextBridge, ipcRenderer, webUtils } = require('electron');
contextBridge.exposeInMainWorld('nexusShell', Object.freeze({
  platform: process.platform,
  health: healthy => ipcRenderer.invoke('nexus:shell', 'health', healthy),
  retry: () => ipcRenderer.invoke('nexus:shell', 'retry'),
  launcher: () => ipcRenderer.invoke('nexus:shell', 'launcher'),
  switchStatus: () => ipcRenderer.invoke('nexus:shell', 'switch-status'),
  session: () => ipcRenderer.invoke('nexus:shell', 'session'),
  sessionOpened: id => ipcRenderer.invoke('nexus:shell', 'session-opened', id),
  onSession: callback => {
    const listener = (_event, id) => callback(id);
    ipcRenderer.on('nexus:session', listener);
    return () => ipcRenderer.removeListener('nexus:session', listener);
  },
}));
contextBridge.exposeInMainWorld('__DSH_DESKTOP_FILE_PATH__', Object.freeze({
  getPathForFile: file => webUtils.getPathForFile(file),
}));
contextBridge.exposeInMainWorld('__DSH_DESKTOP_PICK_DIRECTORY__', () => ipcRenderer.invoke('nexus:shell', 'pick-directory'));
contextBridge.exposeInMainWorld('__DSH_DESKTOP_VALIDATE_DIRECTORY__', value => ipcRenderer.invoke('nexus:shell', 'validate-directory', value));
