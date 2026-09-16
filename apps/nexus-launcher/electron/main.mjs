import { app, BrowserWindow, Menu, Tray, nativeImage, ipcMain, dialog, shell, Notification, globalShortcut, session } from 'electron';
import electronUpdater from 'electron-updater';
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { RustBridge } from './bridge.mjs';
import { harnessUrl, trustedFrame, validateRequest } from './policy.mjs';
import { DesktopUpdater, saveUpdateSettings } from './updater.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const { autoUpdater } = electronUpdater;
const resources = app.isPackaged ? process.resourcesPath : path.join(root, 'desktop/resources');
const shellMode = process.argv.includes('--nexus-shell');
const closeShell = process.argv.includes('--prepare-update');
const desktopData = app.getPath('userData');
// Distinct userData gives Launcher and independent Shell distinct single-instance locks.
if (shellMode) app.setPath('userData', `${desktopData}-shell`);
const lock = app.requestSingleInstanceLock({ closeShell });
if (!lock) { app.exit(closeShell ? 20 : 0); }
else if (closeShell) { app.exit(0); }
else {
  // Do not top-level-await app.whenReady: Electron waits for ESM evaluation
  // before emitting ready, so awaiting run() here deadlocks native startup.
  void run().catch(async error => {
    await app.whenReady(); dialog.showErrorBox('Nexus Launcher', error.message); app.exit(1);
  });
}

async function run() {
  let window, tray, bridge, updater;
  let quitting = false, updating = false, notifications = true, minimizedNotice = false, locale = 'en';
  let controls = {}, controlsAt = 0;
  const text = (en, zh) => locale.startsWith('zh') ? zh : en;
  const show = () => { if (window?.isMinimized()) window.restore(); window?.show(); window?.focus(); };
  const emit = (name, value) => { if (window && !window.isDestroyed()) window.webContents.send(name, value); };
  const error = e => { show(); emit('nexus-native-error', e.message ?? String(e)); };
  app.on('second-instance', (_event, _argv, _cwd, data) => {
    if (shellMode && data.closeShell) { quitting = true; app.quit(); } else show();
  });
  app.on('activate', show);
  app.on('before-quit', () => { quitting = true; });
  app.on('will-quit', () => { updater?.stop(); bridge?.close(); globalShortcut.unregisterAll(); });
  await app.whenReady();
  locale = process.env.NEXUS_LOCALE || app.getLocale();
  // Default deny. Clipboard/IME/file inputs retain Chromium's ordinary user gestures.
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
  bridge = new RustBridge(resources);
  window = new BrowserWindow({
    title: shellMode ? 'DSH — Nexus' : 'Nexus Launcher', width: 1180, height: 760,
    minWidth: 680, minHeight: 520, show: false,
    icon: path.join(root, 'desktop/icons/icon.ico'),
    webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false,
      webviewTag: false, ...(shellMode ? {} : { preload: path.join(root, 'electron/preload.cjs') }) },
  });
  window.on('ready-to-show', show);
  window.on('closed', () => { window = undefined; if (shellMode) app.quit(); });
  window.on('close', event => {
    if (!shellMode && !quitting) {
      event.preventDefault(); window.hide();
      if (notifications && !minimizedNotice && Notification.isSupported()) {
        minimizedNotice = true;
        new Notification({ title: 'Nexus Launcher', body: text('Launcher is still running in the system tray', '启动器仍在系统托盘中运行') }).show();
      }
    }
  });
  window.webContents.on('will-attach-webview', event => event.preventDefault());
  Menu.setApplicationMenu(null);
  window.webContents.on('context-menu', (_event, params) => {
    const items = params.isEditable ? [{ role: 'cut' }, { role: 'copy' }, { role: 'paste' }, { role: 'selectAll' }]
      : [{ role: 'copy', enabled: !!params.selectionText }];
    if (params.mediaType === 'image') items.push({ label: text('Copy image', '复制图片'), click: () => window.webContents.copyImageAt(params.x, params.y) });
    Menu.buildFromTemplate(items).popup({ window });
  });
  if (shellMode) {
    // Resolve the URL through Agent, never accept credentials or arbitrary URLs in argv.
    const status = await bridge.request('proxy_request', { method: 'POST', path: '/v1/agent', body: { action: 'status' } });
    if (!status.available) throw new Error(status.message || 'Agent unavailable');
    const info = await bridge.request('proxy_request', { method: 'GET', path: '/v1/harness/ui' });
    const url = harnessUrl(info.url);
    window.webContents.on('will-navigate', (event, target) => { if (new URL(target).origin !== url.origin) event.preventDefault(); });
    window.webContents.setWindowOpenHandler(({ url: target }) => {
      if (['https:', 'http:'].includes(new URL(target).protocol)) void shell.openExternal(target);
      return { action: 'deny' };
    });
    await window.loadURL(url.href);
    return;
  }
  const documentUrl = pathToFileURL(path.join(root, 'dist/index.html')).href;
  session.defaultSession.webRequest.onHeadersReceived((details, callback) => {
    if (details.url !== documentUrl) return callback({ responseHeaders: details.responseHeaders });
    callback({ responseHeaders: { ...details.responseHeaders,
      'Content-Security-Policy': ["default-src 'self'; connect-src 'self'; frame-src http://127.0.0.1:* http://localhost:* http://[::1]:*; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; script-src 'self'; object-src 'none'; base-uri 'none'"] } });
  });
  window.webContents.on('will-navigate', (event, target) => { if (target !== documentUrl) event.preventDefault(); });
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  const settingsPath = path.join(desktopData, 'desktop-update.json');
  let settings = {};
  try { settings = JSON.parse(readFileSync(settingsPath, 'utf8')); } catch (e) { if (e.code !== 'ENOENT') settings = { enabled: false }; }
  updater = new DesktopUpdater(autoUpdater, {
    settings, packaged: app.isPackaged, changed: state => emit('nexus-update', state),
    save: value => saveUpdateSettings(settingsPath, value),
    cancel: async () => {
      try { await bridge.request('cancel_update'); }
      finally { updating = false; quitting = false; }
    },
    coordinate: async () => {
      if (updating) throw new Error('Update coordination already in progress');
      updating = true;
      try {
        // Agent atomically rejects shutdown while Harness or a managed operation owns a gate.
        await bridge.request('prepare_update');
        await closeIndependentShell();
        quitting = true;
      } catch (e) { await bridge.request('cancel_update').catch(() => {}); updating = false; throw e; }
    },
  });
  function launchShell() {
    if (updating) throw new Error('Update in progress');
    const child = spawn(process.execPath, [...(app.isPackaged ? [] : [root]), '--nexus-shell'], {
      detached: true, stdio: 'ignore', env: process.env, windowsHide: false,
    });
    child.on('error', error); child.unref();
  }
  async function closeIndependentShell() {
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      const code = await new Promise((resolve, reject) => {
        const child = spawn(process.execPath, [...(app.isPackaged ? [] : [root]), '--nexus-shell', '--prepare-update'], { stdio: 'ignore', windowsHide: true });
        child.on('error', reject); child.on('exit', resolve);
      });
      if (code === 0) return;
      if (code !== 20) throw new Error('Shell exit coordination failed');
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    throw new Error('Shell is still open; update deferred');
  }
  function rebuildTray() {
    const fresh = Date.now() - controlsAt < 180000 ? controls : {};
    const status = {
      running: ['Harness: running', 'Harness：运行中'],
      stopped: ['Harness: stopped', 'Harness：已停止'],
      detached: ['Harness: stopped', 'Harness：已停止'],
      starting: ['Harness: starting', 'Harness：启动中'],
      stopping: ['Harness: stopping', 'Harness：停止中'],
      failed: ['Harness: failed', 'Harness：运行失败'],
    }[fresh.state] ?? ['Harness: status unavailable', 'Harness：状态不可用'];
    tray.setContextMenu(Menu.buildFromTemplate([
      { label: text(...status), enabled: false },
      { label: text('Show launcher', '显示启动器'), click: show },
      ...[fresh.stop ? ['stop', 'Stop Harness', '停止 Harness'] : ['start', 'Start Harness', '启动 Harness'],
        ['web', 'Open Harness Web', '打开 Harness Web'], ['terminal', 'Open DSH terminal', '打开 DSH 终端']].map(([id, en, zh]) => ({
        label: text(en, zh), enabled: fresh[id] === true,
        click: () => { if (Date.now() - controlsAt < 180000) emit('nexus-tray-action', id); else show(); },
      })),
      { type: 'separator' },
      { label: text('Exit launcher (keep services running)', '退出启动器（服务继续运行）'), click: () => { quitting = true; app.quit(); } },
      { label: text('Stop services and exit', '停止服务并退出'), click: () => {
        void bridge.request('proxy_request', { path: '/v1/agent', method: 'POST', body: { action: 'stop' } })
          .then(() => { quitting = true; app.quit(); }).catch(error);
      } },
    ]));
  }
  tray = new Tray(nativeImage.createFromPath(path.join(root, 'desktop/icons/32x32.png')));
  tray.setToolTip('Nexus Launcher'); tray.on('click', show); rebuildTray();
  const trayTimer = setInterval(rebuildTray, 30000); trayTimer.unref();
  globalShortcut.register('CommandOrControl+Shift+N', show);
  ipcMain.handle('nexus:command', async (event, command, args = {}) => {
    try {
      if (!trustedFrame(event, window, documentUrl)) throw new Error('Untrusted desktop frame');
      validateRequest(command, args);
      if (updating) throw new Error('Update coordination in progress');
      let value;
      switch (command) {
        case 'build_identity': value = JSON.parse(readFileSync(path.join(resources, 'release-identity.json'), 'utf8')); break;
        case 'choose_local_path': {
          const filters = args.archive ? [{ name: 'Nexus offline package', extensions: ['tar.gz'] }] : [];
          const result = args.save ? await dialog.showSaveDialog(window, { filters })
            : await dialog.showOpenDialog(window, { filters, properties: [args.directory ? 'openDirectory' : 'openFile'] });
          value = result.canceled ? null : result.filePath ?? result.filePaths[0]; break;
        }
        case 'set_native_locale': locale = String(args.locale); rebuildTray(); break;
        case 'set_native_notifications': notifications = args.enabled === true; break;
        case 'notify': if (notifications && Notification.isSupported()) new Notification({ title: String(args.title).slice(0, 256), body: String(args.body ?? '').slice(0, 4096) }).show(); break;
        case 'update_tray': controls = args.controls ?? {}; controlsAt = Date.now(); rebuildTray(); break;
        case 'autostart_status': value = app.getLoginItemSettings().openAtLogin; break;
        case 'autostart_set': app.setLoginItemSettings({ openAtLogin: args.enabled === true }); break;
        case 'update_status': value = updater.state; break;
        case 'update_check': value = await updater.check({ manual: true }); break;
        case 'update_settings': value = updater.setEnabled(args.enabled); break;
        case 'update_install': await updater.install(); break;
        case 'proxy_request':
          if (args.path === '/v1/harness/ui' && args.method === 'POST' && args.body?.action === 'open') {
            value = await bridge.request(command, { method: 'GET', path: '/v1/harness/ui' });
            harnessUrl(value.url); launchShell(); break;
          }
          value = await bridge.request(command, args); break;
        default:
          value = await bridge.request(command, args);
          if (command === 'export_startup_diagnostics' && value.export_path) shell.showItemInFolder(value.export_path);
      }
      return { value };
    } catch (e) { return { error: { code: e.code ?? 'desktop_error', message: e.message ?? String(e), ...e } }; }
  });
  await window.loadURL(documentUrl);
  if (app.isPackaged) updater.start();
}
