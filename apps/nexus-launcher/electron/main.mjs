import { app, BrowserWindow, Menu, Tray, nativeImage, ipcMain, dialog, shell, Notification, globalShortcut, session } from 'electron';
import electronUpdater from 'electron-updater';
import { readFileSync, writeFileSync, renameSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { RustBridge } from './bridge.mjs';
import { harnessUrl, githubUrl, trustedFrame, validateRequest, nativeEditAction } from './policy.mjs';
import { DesktopUpdater, saveUpdateSettings } from './updater.mjs';
import { EventCursor, settings as notificationSettings, shouldNotify, notificationContent, taskFocused } from './notifications.mjs';
import { HarnessDesktop, desktopActive, readDesktopState } from './harness-desktop.mjs';
import { ClientAudit, browserReady, openVerifiedBrowser } from './client-audit.mjs';
import { trayEntries, cancelTrayStartup, TrayStartupFeedback } from './tray-menu.mjs';
import { NoticeCoordinator } from './notice-coordinator.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const { autoUpdater } = electronUpdater;
const resources = app.isPackaged ? process.resourcesPath : path.join(root, 'desktop/resources');
const openNativeDesktop = process.argv.includes('--nexus-shell') || process.argv.includes('--harness-desktop');
const closeShell = process.argv.includes('--prepare-update');
const desktopData = app.getPath('userData');
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
  let window, tray, bridge, updater, nativeDesktop, noticeCoordinator;
  let quitting = false, updating = false, notifications = true, minimizedNotice = false, locale = 'en';
  let controls = {}, controlsAt = 0;
  let trayStartup = {};
  let nativeMutationCount = 0, trayBusy = false, trayRefreshPending, traySupported = false, trayWeb = {}, trayMenu;
  const desktopLocked = () => nativeDesktop?.busy || desktopActive(nativeDesktop?.status());
  const startDesktop = () => {
    if (nativeMutationCount || updating) return Promise.reject(new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。')));
    return nativeDesktop.start();
  };
  const restartDesktop = () => {
    if (nativeMutationCount || updating) return Promise.reject(new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。')));
    return nativeDesktop.restart();
  };
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
      else if (['harness-failed', 'update-ready'].includes(kind)) show();
      else void openVerifiedBrowser(bridge, clientAudit, url => shell.openExternal(url), validSession(task?.session) ? task.session : undefined).catch(error);
    });
    notice.on('failed', (_event, failure) => emit('nexus-native-error', `Notification delivery failed: ${failure}`));
    notice.show();
    if (process.platform === 'win32') window?.flashFrame(true);
  }
  const text = (en, zh) => locale.startsWith('zh') ? zh : en;
  const show = () => { if (window?.isMinimized()) window.restore(); window?.show(); window?.focus(); };
  const emit = (name, value) => { if (window && !window.isDestroyed()) window.webContents.send(name, value); };
  const trayFeedback = new TrayStartupFeedback((state, mode) => {
    const label = mode === 'desktop' ? 'Desktop' : 'Web';
    const messages = {
      starting: text(`${label}: starting and checking. Please wait.`, `${label}：正在启动并检查，请稍候。`),
      ready: text(`${label}: startup checks passed. Ready to use.`, `${label}：启动检查通过，已就绪。`),
      failed: text(`${label}: startup failed. Click to open the workbench and repair.`, `${label}：启动失败，点击打开工作台处理。`),
      unverified: text(`${label}: startup could not be verified. Click to view check results.`, `${label}：尚未确认启动成功，点击工作台查看检查结果。`),
      cancelled: text(`${label}: startup cancelled.`, `${label}：启动已取消。`),
    };
    const body = messages[state];
    if (tray) tray.setToolTip(`Nexus Launcher — ${body}`);
    // Explicit tray command feedback uses native Windows balloons, not task notices.
    if (process.platform === 'win32' && tray) {
      tray.displayBalloon({title:'Nexus Launcher',content:body,iconType:state === 'failed' ? 'error' : state === 'unverified' ? 'warning' : 'info'});
    } else if (Notification.isSupported()) {
      const notice = new Notification({title:'Nexus Launcher',body});
      notice.on('click', () => { show(); emit('nexus-tray-action','workbench'); });
      notice.show();
    }
  });
  const error = e => { if (e.message === 'desktop_start_cancelled') return; show(); emit('nexus-native-error', e.message ?? String(e)); };
  const validSession = value => typeof value === 'string' && value.length > 0 && value.length <= 200 && !/[\u0000-\u001f]/.test(value);
  app.on('second-instance', (_event, argv, _cwd, data) => {
    if (data.closeShell) return;
    show();
    if (argv.some(arg => ['--nexus-shell', '--harness-desktop'].includes(arg))) void startDesktop().catch(error);
  });
  app.on('activate', show);
  app.on('before-quit', () => { quitting = true; });
  app.on('will-quit', () => { noticeCoordinator?.close(); updater?.stop(); bridge?.close(); globalShortcut.unregisterAll(); });
  await app.whenReady();
  const systemLanguages = app.getPreferredSystemLanguages();
  locale = process.env.NEXUS_LOCALE || systemLanguages.find(language => /^(zh|en)(-|$)/i.test(language)) || 'en';
  // Default deny. Clipboard/IME/file inputs retain Chromium's ordinary user gestures.
  session.defaultSession.setPermissionRequestHandler((_contents, _permission, callback) => callback(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
  const nativeActive = desktopActive(readDesktopState(path.join(desktopData, 'harness-desktop/state.json')));
  bridge = new RustBridge(resources, undefined, openNativeDesktop || nativeActive ? { NEXUS_HARNESS_AUTOSTART: '0' } : {});
  nativeDesktop = new HarnessDesktop({ bridge, userData: desktopData, resources,
    electronApp: app.isPackaged ? undefined : root,
    executable: path.join(resources, 'runtime/node', process.platform === 'win32' ? 'node.exe' : 'node') });
  window = new BrowserWindow({
    title: 'Nexus Launcher', width: 1180, height: 760,
    minWidth: 680, minHeight: 520, show: false,
    icon: path.join(root, 'desktop/icons/icon.ico'),
    webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false,
      webviewTag: false, preload: path.join(root, 'electron/preload.cjs'),
      additionalArguments: [`--nexus-system-languages=${JSON.stringify(systemLanguages)}`] },
  });
  window.on('ready-to-show', show);
  window.on('focus', () => window?.flashFrame(false));
  window.on('closed', () => { window = undefined; });
  window.on('close', event => {
    if (!quitting) {
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
  noticeCoordinator = new NoticeCoordinator(desktopData, 'launcher');
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
  const documentUrl = pathToFileURL(path.join(root, 'dist/index.html')).href;
  const openedRunFile = path.join(app.getPath('userData'), 'browser-opened-run.json');
  let openedRun;
  try { openedRun = JSON.parse(readFileSync(openedRunFile, 'utf8')).run; } catch {}
  const clientAudit = new ClientAudit({ createWindow: options => new BrowserWindow(options), openedRun,
    openReady: async (url, run) => {
      // Persist only the run ID, never the authentication URL. Reopening Nexus
      // must not reopen a tab for an already handled running instance.
      writeFileSync(`${openedRunFile}.tmp`, JSON.stringify({ run }), { mode: 0o600 });
      renameSync(`${openedRunFile}.tmp`, openedRunFile);
      await shell.openExternal(harnessUrl(url).href);
    },
  });
  let auditBusy = false;
  let reportedDesktopFailure;
  const pollAudit = async () => {
    if (auditBusy || quitting || updating) return;
    auditBusy = true;
    try {
      const desktop = nativeDesktop.status();
      if (desktop.operationId && (desktop.audit?.state === 'failed' || desktop.phase === 'failed') && reportedDesktopFailure !== desktop.operationId) {
        reportedDesktopFailure = desktop.operationId;
        show();
        error(new Error(text('Official Desktop startup failed (profile: desktop). Open the Workbench for error details; restart or close Desktop from Nexus.', '官方 Desktop 启动失败（配置：desktop）。请到工作台查看错误详情，通过 Nexus 重启或关闭 Desktop。')));
      }
    } catch { /* Web auditing must continue if Desktop state cannot be read. */ }
    try {
      clientAudit.observe(await bridge.request('proxy_request', { method: 'GET', path: '/v1/harness/ui' }));
    } catch { clientAudit.clear(); }
    finally { auditBusy = false; }
  };
  const auditTimer = setInterval(() => void pollAudit(), 2000); auditTimer.unref();
  app.on('will-quit', () => { clearInterval(auditTimer); clientAudit.stop(); });
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
      if (updating || trayBusy || nativeMutationCount) throw new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。'));
      updating = true;
      try {
        // Agent atomically rejects shutdown while Harness or a managed operation owns a gate.
        if (desktopLocked()) throw new Error(text('Close Harness Desktop before changing versions, configuration, or data.', '请先关闭 Harness Desktop，再修改版本、配置或数据。'));
        await bridge.request('prepare_update');
        quitting = true;
      } catch (e) { await bridge.request('cancel_update').catch(() => {}); updating = false; throw e; }
    },
  });
  function rebuildTray() {
    if (!tray) return;
    const fresh = Date.now() - controlsAt < 180000 ? controls : {};
    const operationId = trayWeb.startup_id;
    trayMenu = Menu.buildFromTemplate(trayEntries({text, web: {...fresh, ...trayWeb}, desktop: nativeDesktop.status(), supported: traySupported,
      busy: trayBusy || updating || nativeMutationCount > 0,
      canCancelStartup: !updating && !trayBusy && !!operationId,
      canStopDesktop: !updating && nativeMutationCount === 0,
      show, act: id => void trayAction(id, operationId).catch(error)}));
    tray.setToolTip(`Nexus Launcher — ${trayMenu.items[0]?.label || ''}`);
    if (process.platform !== 'win32') tray.setContextMenu(trayMenu);
  }
  function refreshTray() {
    if (trayRefreshPending) return trayRefreshPending;
    if (quitting) return Promise.resolve();
    trayRefreshPending = (async () => {
    try {
      const [runtime, capability, startup, ui] = await Promise.allSettled([
        bridge.request('proxy_request', {method:'GET',path:'/v1/harness'}), nativeDesktop.capability(),
        bridge.request('proxy_request', {method:'GET',path:'/v1/harness/startup'}),
        bridge.request('proxy_request', {method:'GET',path:'/v1/harness/ui'}),
      ]);
      if (runtime.status === 'fulfilled') {
        const harness = runtime.value.harness ?? {};
        // Native polling refreshes facts, not the UI's operation/identity gate.
        const info = ui.status === 'fulfilled' ? clientAudit.result(ui.value) : {};
        const current = info.generation === runtime.value.generation && info.run_id === runtime.value.log_session_run_id;
        trayWeb = {state:harness.state, pid:harness.pid, ready:current && browserReady(info), run_id:runtime.value.log_session_run_id, reason:current ? info.browser_health?.reason : undefined, health:current ? info.browser_health?.state : undefined};
      } else trayWeb = {state:undefined,stop:false,start:false,web:false,terminal:false};
      if (startup.status === 'fulfilled' && startup.value.cancellable && !startup.value.cancel_requested)
        trayWeb.startup_id = startup.value.operation_id;
      trayStartup = startup.status === 'fulfilled' ? startup.value : {};
      trayFeedback.observe({web:trayWeb,startup:trayStartup,desktop:nativeDesktop.status()});
      traySupported = capability.status === 'fulfilled' && capability.value.supported === true;
    } finally { trayRefreshPending = undefined; rebuildTray(); }
    })();
    return trayRefreshPending;
  }
  async function trayAction(id, operationId) {
    if (id === 'maintenance' || id === 'profiles') { show(); emit('nexus-tray-action',id); return; }
    if (id === 'cancel-startup' && !updating && !trayBusy && !desktopLocked()) {
      if (await cancelTrayStartup(bridge, operationId)) trayFeedback.finish('cancelled'); show(); await refreshTray(); return;
    }
    if (id === 'desktop-stop' && !updating && nativeMutationCount === 0) {
      await nativeDesktop.stop(); trayFeedback.finish('cancelled'); await refreshTray(); return;
    }
    if (trayBusy || updating || nativeMutationCount) throw new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。'));
    if (['start', 'stop', 'restart', 'web', 'terminal'].includes(id)) {
      if (desktopLocked()) throw new Error(text('Close Harness Desktop first.', '请先关闭 Harness 桌面端。'));
      // Reuse Workbench's action flow, including readiness, receipts, repairs,
      // credential invalidation and refresh, instead of bypassing it with HTTP.
      if (['start','restart'].includes(id)) {
        await refreshTray();
        trayFeedback.begin('web', {run_id:trayWeb.run_id,operation_id:trayStartup.operation_id});
      }
      if (id === 'stop') trayFeedback.finish('cancelled');
      show(); emit('nexus-tray-action', id); return;
    }
    if (['desktop','desktop-restart'].includes(id)) trayFeedback.begin('desktop', {operationId:nativeDesktop.status().operationId});
    trayBusy = true; rebuildTray();
    try {
      if (id === 'desktop') await startDesktop();
      else if (id === 'desktop-restart') await restartDesktop();
      else if (id === 'desktop-stop') await nativeDesktop.stop();
      else if (id === 'exit') { if(nativeDesktop.busy) throw new Error('Desktop preparation is still starting'); quitting=true; app.quit(); }
      else if (id === 'stop-exit') {
        await nativeDesktop.stop();
        await bridge.request('proxy_request',{path:'/v1/agent',method:'POST',body:{action:'stop'}});
        quitting=true; app.quit();
      }
    } catch (failure) { trayFeedback.finish('failed'); throw failure; } finally { trayBusy=false; if(!quitting) await refreshTray(); }
  }
  tray = new Tray(nativeImage.createFromPath(path.join(root, 'desktop/icons/32x32.png')));
  tray.on('balloon-click', () => { show(); emit('nexus-tray-action','workbench'); });
  tray.setToolTip('Nexus Launcher'); tray.on('click',show);
  tray.on('right-click', () => { if (process.platform === 'win32') tray.popUpContextMenu(trayMenu); void refreshTray().catch(error); });
  rebuildTray(); void refreshTray();
  const trayTimer=setInterval(()=>void refreshTray(),5000); trayTimer.unref();
  app.on('will-quit',()=>clearInterval(trayTimer));
  globalShortcut.register('CommandOrControl+Shift+N', show);
  ipcMain.handle('nexus:command', async (event, command, args = {}) => {
    let trackedMutation = false;
    try {
      if (!trustedFrame(event, window, documentUrl)) throw new Error('Untrusted desktop frame');
      validateRequest(command, args);
      if (updating) throw new Error('Update coordination in progress');
      if (trayBusy && ((command === 'proxy_request' && args.method === 'POST') || command === 'retry_startup')) throw new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。'));
      if (desktopLocked() && ((command === 'proxy_request' && args.method === 'POST') || command === 'retry_startup')) throw new Error(text('Close Harness Desktop before changing versions, configuration, or data.', '请先关闭 Harness Desktop，再修改版本、配置或数据。'));
      if ((command === 'proxy_request' && args.method === 'POST') || command === 'retry_startup') { nativeMutationCount++; trackedMutation=true; }
      let value;
      switch (command) {
        case 'harness_desktop_capability': value = await nativeDesktop.capability(); break;
        case 'harness_desktop_status': value = nativeDesktop.status(); break;
        case 'harness_desktop_start': if(trayBusy) throw new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。')); value = await startDesktop(); break;
        case 'harness_desktop_stop': value = await nativeDesktop.stop(); break;
        case 'harness_desktop_restart': if(trayBusy) throw new Error(text('Wait for the current operation to finish.', '请等待当前操作完成。')); value = await restartDesktop(); break;
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
        case 'open_github': await shell.openExternal(githubUrl(args.url).href); break;
        case 'update_release_notes': {
          const version = updater.state.version;
          if (!version || !/^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) throw new Error('No update version available');
          await shell.openExternal(`https://github.com/chelal233/dsh-nexus/releases/tag/v${encodeURIComponent(version)}`);
          break;
        }
        case 'update_check': value = await updater.check({ manual: true }); break;
        case 'update_download': value = await updater.download(args.version); break;
        case 'update_settings': value = updater.setEnabled(args.enabled); break;
        case 'update_install': await updater.install(); break;
        case 'proxy_request':
          if (args.path === '/v1/harness/ui' && args.method === 'POST' && args.body?.action === 'open') {
            value = await openVerifiedBrowser(bridge, clientAudit, url => shell.openExternal(url)); break;
          }
          value = await bridge.request(command, args);
          if (args.path === '/v1/harness/ui' && args.method === 'GET') value = clientAudit.result(value);
          break;
        default:
          value = await bridge.request(command, args);
          if (command === 'export_startup_diagnostics' && value.export_path) shell.showItemInFolder(value.export_path);
      }
      return { value };
    } catch (e) { return { error: { code: e.code ?? 'desktop_error', message: e.message ?? String(e), ...e } }; }
    finally { if(trackedMutation) nativeMutationCount--; rebuildTray(); }
  });
  await window.loadURL(documentUrl);
  if (openNativeDesktop) void startDesktop().catch(error);
  if (app.isPackaged) updater.start();
}
