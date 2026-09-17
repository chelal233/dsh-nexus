import { app, BrowserWindow, Menu, Tray, nativeImage, ipcMain, dialog, shell, Notification, globalShortcut, session } from 'electron';
import electronUpdater from 'electron-updater';
import { spawn } from 'node:child_process';
import { readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { RustBridge } from './bridge.mjs';
import { harnessUrl, trustedFrame, validateRequest, nativeEditAction } from './policy.mjs';
import { DesktopUpdater, saveUpdateSettings } from './updater.mjs';
import { EventCursor, settings as notificationSettings, shouldNotify, notificationContent, taskFocused } from './notifications.mjs';
import { ShellController } from './shell-controller.mjs';
import { NoticeCoordinator } from './notice-coordinator.mjs';

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
    await app.whenReady();
    // The React app may not be available yet. Render a self-contained Nexus
    // error window with escaped text instead of an OS message box.
    const escape = value => String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
    const zh = app.getLocale().toLowerCase().startsWith('zh');
    const failure = new BrowserWindow({ width: 620, height: 380, autoHideMenuBar: true, webPreferences: { nodeIntegration: false, contextIsolation: true, sandbox: true } });
    failure.setMenu(null);
    failure.on('closed', () => app.exit(1));
    await failure.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent(`<!doctype html><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-nexus-error'"><title>Nexus Launcher</title><style>body{font:16px system-ui;padding:28px;background:#f4f7f7;color:#183737}pre{white-space:pre-wrap;overflow-wrap:anywhere}button{padding:8px 24px;border:0;border-radius:8px;background:#086c65;color:white}</style><h2>${zh ? 'Nexus 启动失败' : 'Nexus could not start'}</h2><pre>${escape(error?.message ?? error)}</pre><button id="close">${zh ? '关闭' : 'Close'}</button><script nonce="nexus-error">document.getElementById('close').onclick=()=>window.close()</script>`));
  });
}

async function run() {
  let window, tray, bridge, updater, shellController, noticeCoordinator;
  let pendingSession = process.argv.find(arg => arg.startsWith('--nexus-session='))?.slice(16);
  let quitting = false, updating = false, notifications = true, minimizedNotice = false, locale = 'en';
  let controls = {}, controlsAt = 0;
  let noticeSettings = notificationSettings(), noticeReady = false, noticeBusy = false;
  const noticeCursor = new EventCursor();
  let noticeViews = [];
  const noticeFocused = (kind, task) => ['harness-failed', 'update-ready'].includes(kind) ? noticeCoordinator?.peer('launcher')?.focused === true : taskFocused(task, noticeViews);
  function deliverNotice(kind, test = false, task = undefined) {
    if (!test && (!noticeReady || !shouldNotify(noticeSettings, kind, noticeFocused(kind, task)))) return;
    if (!Notification.isSupported()) throw new Error('System notifications are unavailable');
    const notice = new Notification(test ? { title: 'Nexus Launcher', body: text('Test notification', '测试通知') } : notificationContent(kind, locale, task));
    notice.on('click', () => {
      if (test) show();
      else if (['harness-failed', 'update-ready'].includes(kind)) { if (shellMode) launchHost(false); else show(); }
      else if (shellMode) {
        pendingSession = task?.session; show();
        window?.webContents.send('nexus:session', pendingSession);
        void shellController?.tick();
      } else if (noticeCoordinator?.peer('shell')) {
        launchHost(true, task?.session);
      } else void bridge.request('proxy_request', { method: 'GET', path: '/v1/harness/ui' })
        .then(info => {
          const url = harnessUrl(info.url);
          if (validSession(task?.session)) url.searchParams.set('nexus-session', task.session);
          return shell.openExternal(url.href);
        }).catch(error);
    });
    notice.on('failed', (_event, failure) => emit('nexus-native-error', `Notification delivery failed: ${failure}`));
    notice.show();
    if (process.platform === 'win32') window?.flashFrame(true);
  }
  const text = (en, zh) => locale.startsWith('zh') ? zh : en;
  const show = () => { if (window?.isMinimized()) window.restore(); window?.show(); window?.focus(); };
  const emit = (name, value) => { if (window && !window.isDestroyed()) window.webContents.send(name, value); };
  const error = e => { show(); emit('nexus-native-error', e.message ?? String(e)); };
  const validSession = value => typeof value === 'string' && value.length > 0 && value.length <= 200 && !/[\u0000-\u001f]/.test(value);
  function launchHost(asShell, sessionId) {
    const args = [...(app.isPackaged ? [] : [root]), `--user-data-dir=${desktopData}`,
      ...(asShell ? ['--nexus-shell'] : []), ...(validSession(sessionId) ? [`--nexus-session=${sessionId}`] : [])];
    const child = spawn(process.execPath, args, { detached: true, stdio: 'ignore', windowsHide: true });
    child.on('error', error); child.unref();
  }
  app.on('second-instance', (_event, _argv, _cwd, data) => {
    if (shellMode && data.closeShell) { quitting = true; app.quit(); }
    else {
      const id = _argv.find(arg => arg.startsWith('--nexus-session='))?.slice(16);
      if (shellMode && validSession(id)) { pendingSession = id; window?.webContents.send('nexus:session', id); }
      show();
    }
  });
  app.on('activate', show);
  app.on('before-quit', () => { quitting = true; });
  app.on('will-quit', () => { shellController?.stop(); noticeCoordinator?.close(); updater?.stop(); bridge?.close(); globalShortcut.unregisterAll(); });
  await app.whenReady();
  const systemLanguages = app.getPreferredSystemLanguages();
  locale = process.env.NEXUS_LOCALE || systemLanguages.find(language => /^(zh|en)(-|$)/i.test(language)) || 'en';
  // Default deny. Clipboard/IME/file inputs retain Chromium's ordinary user gestures.
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
  bridge = new RustBridge(resources);
  window = new BrowserWindow({
    title: shellMode ? 'DSH — Nexus' : 'Nexus Launcher', width: 1180, height: 760,
    minWidth: 680, minHeight: 520, show: false,
    icon: path.join(root, 'desktop/icons/icon.ico'),
    webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false,
      webviewTag: false, ...(shellMode ? { preload: path.join(root, 'electron/shell-preload.cjs') } : { preload: path.join(root, 'electron/preload.cjs'),
        additionalArguments: [`--nexus-system-languages=${JSON.stringify(systemLanguages)}`] }) },
  });
  window.on('ready-to-show', show);
  window.on('focus', () => window?.flashFrame(false));
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
  // A menu-less macOS window has no Edit menu accelerators. Keep ordinary
  // editing available in both Launcher and the independent Harness window.
  window.webContents.on('before-input-event', (event, input) => {
    const action = nativeEditAction(process.platform, input);
    if (action) { event.preventDefault(); window.webContents[action](); }
  });
  window.webContents.on('context-menu', (_event, params) => {
    const items = params.isEditable ? [{ role: 'cut' }, { role: 'copy' }, { role: 'paste' }, { role: 'selectAll' }]
      : [{ role: 'copy', enabled: !!params.selectionText }];
    if (params.mediaType === 'image') items.push({ label: text('Copy image', '复制图片'), click: () => window.webContents.copyImageAt(params.x, params.y) });
    Menu.buildFromTemplate(items).popup({ window });
  });
  noticeCoordinator = new NoticeCoordinator(desktopData, shellMode ? 'shell' : 'launcher');
  noticeCoordinator.prune();
  const heartbeat = () => { try { noticeCoordinator.heartbeat(window?.isFocused() === true); } catch {} };
  window.on('focus', heartbeat); window.on('blur', heartbeat); heartbeat();
  const pollNotices = async () => {
    if (noticeBusy || quitting || updating) return;
    noticeBusy = true;
    try {
      heartbeat();
      const snapshot = await bridge.request('proxy_request', { method: 'GET', path: '/v1/notifications' });
      noticeSettings = notificationSettings(snapshot.settings); noticeReady = true;
      noticeViews = snapshot.views ?? [];
      for (const task of noticeCursor.consume(snapshot)) {
        if (shouldNotify(noticeSettings, task.kind, noticeFocused(task.kind, task)) && noticeCoordinator.claim(snapshot.epoch, task)) deliverNotice(task.kind, false, task);
      }
    } catch { /* Agent unavailable: retain cursor; discovery retries independently. */ }
    finally { noticeBusy = false; }
  };
  const noticeTimer = setInterval(() => void pollNotices(), 1000); noticeTimer.unref();
  app.on('will-quit', () => clearInterval(noticeTimer));
  void pollNotices();
  if (shellMode) {
    const recoveryUrl = pathToFileURL(path.join(root, 'electron/shell-recovery.html')).href;
    let healthTimer;
    const recover = () => { clearTimeout(healthTimer); return window && !window.isDestroyed() ? window.loadURL(recoveryUrl) : Promise.resolve(); };
    shellController = new ShellController({
      discover: async () => {
        await bridge.request('startup_status');
        return bridge.request('proxy_request', { method: 'GET', path: '/v1/harness/ui' });
      },
      load: url => {
        clearTimeout(healthTimer);
        healthTimer = setTimeout(() => void shellController.broken(), 45000); healthTimer.unref();
        return window.loadURL(url);
      }, recover,
      changed: () => { if (validSession(pendingSession)) window.webContents.send('nexus:session', pendingSession); },
    });
    const trustedShell = event => !window?.isDestroyed() && event.sender === window.webContents
      && event.senderFrame === window.webContents.mainFrame
      && (event.senderFrame.url === recoveryUrl || new URL(event.senderFrame.url).origin === shellController.current?.origin);
    ipcMain.handle('nexus:shell', async (event, action, value) => {
      if (!trustedShell(event)) throw new Error('Untrusted Harness frame');
      if (action === 'retry') { await bridge.request('retry_startup'); return shellController.retry(); }
      if (action === 'launcher') return launchHost(false);
      if (action === 'switch-status') {
        const status = await bridge.request('proxy_request', { method: 'GET', path: '/v1/desktop/profile' });
        return { phase: status.phase };
      }
      if (event.senderFrame.url === recoveryUrl) throw new Error('Harness is not ready');
      if (action === 'health') { clearTimeout(healthTimer); if (value === false) await shellController.broken(); return; }
      if (action === 'session') return validSession(pendingSession) ? pendingSession : null;
      if (action === 'session-opened' && value === pendingSession) { pendingSession = undefined; return; }
      if (action === 'pick-directory') {
        const picked = await dialog.showOpenDialog(window, { properties: ['openDirectory'] });
        return picked.canceled ? null : picked.filePaths[0];
      }
      if (action === 'validate-directory') {
        if (typeof value !== 'string' || value.length > 32768 || value.includes('\0') || !path.isAbsolute(value)) return false;
        try { return statSync(value).isDirectory(); } catch { return false; }
      }
      throw new Error('Unsupported Harness action');
    });
    window.webContents.on('will-navigate', (event, target) => {
      const url = new URL(target);
      if (url.origin !== shellController.current?.origin) {
        event.preventDefault();
        if (['http:', 'https:'].includes(url.protocol)) void shell.openExternal(url.href).catch(error);
      }
    });
    window.webContents.setWindowOpenHandler(({ url: target }) => {
      if (['https:', 'http:'].includes(new URL(target).protocol)) void shell.openExternal(target);
      return { action: 'deny' };
    });
    window.webContents.on('did-fail-load', (_event, code, _description, url, main) => {
      if (main && code !== -3 && url !== recoveryUrl) void shellController.broken();
    });
    window.webContents.on('render-process-gone', () => void shellController.broken());
    window.on('unresponsive', () => void shellController.broken());
    await recover();
    void shellController.tick();
    const timer = setInterval(() => void shellController.tick(), 2000); timer.unref();
    app.on('will-quit', () => { clearInterval(timer); clearTimeout(healthTimer); });
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
    settings, packaged: app.isPackaged, changed: state => {
      emit('nexus-update', state);
      if (state.phase === 'ready') { try { deliverNotice('update-ready', false, { title: `Nexus ${state.version ?? ''}`, body: text('Restart to apply the update. Running Harness tasks will be stopped.', '重启后应用更新，正在运行的 Harness 任务会被中止。') }); } catch (e) { error(e); } }
    },
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
  async function closeIndependentShell() {
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      const code = await new Promise((resolve, reject) => {
        const child = spawn(process.execPath, [...(app.isPackaged ? [] : [root]), `--user-data-dir=${desktopData}`, '--nexus-shell', '--prepare-update'], { stdio: 'ignore', windowsHide: true });
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
        case 'notify': deliverNotice('harness-failed', false, { title: args?.title, body: args?.body }); break;
        case 'notification_test': deliverNotice('completed', true); break;
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
            await shell.openExternal(harnessUrl(value.url).href); break;
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
