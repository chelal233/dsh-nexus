// Harness closure-factory client bundle. No imports and no Electron privileges.
window.__ModuleLoader__.load({ id: '@nexus/desktop-bridge', factory() {
  return { apply(ctx) {
    const shell = window.nexusShell;
    let disposed = false;
    if (shell) {
      ctx.effect(() => ctx.reflect.provide('desktopWindow', Object.freeze({
        mode: 'compatibility', platform: shell.platform,
        material: 'off', micaSupported: false, availableMaterials: Object.freeze(['off']),
        safeAreaInsets: Object.freeze({ top: 0, right: 0, bottom: 0, left: 0 }),
        dragRegion: Object.freeze({ height: 0, leftInset: 0, rightInset: 0 }),
      })));
      // Adapt the public workspace chooser only in the trusted native window.
      ctx.inject(['uiWorkspace'], c => c.effect(() => {
        const service = c.uiWorkspace, previous = service.pickDirectory;
        if (typeof previous !== 'function') return;
        const pick = () => window.__DSH_DESKTOP_PICK_DIRECTORY__();
        service.pickDirectory = pick;
        return () => { if (service.pickDirectory === pick) service.pickDirectory = previous; };
      }));
      // Settle only after this plugin's apply has returned (avoid Loader deadlock).
      const timer = setTimeout(async () => {
        try {
          await ctx.loader.await();
          const failed = [...ctx.loader.entries()].some(entry => !entry.disabled && !entry.options?.disabled && entry.fiber?.state !== 2);
          if (!disposed) await shell.health(!failed);
        } catch { if (!disposed) void shell.health(false).catch(() => {}); }
      }, 0);
      ctx.effect(() => () => { disposed = true; clearTimeout(timer); });
    }
    ctx.inject(['sessions'], c => c.effect(() => {
      let stopped = false, busy = false;
      const page = crypto.randomUUID();
      let presenceBusy = false, presenceAgain = false;
      const reportView = async () => {
        if (stopped) return;
        if (presenceBusy) { presenceAgain = true; return; }
        presenceBusy = true;
        try {
          await fetch('/nexus-notifications/view', {
            method: 'POST', credentials: 'same-origin', headers: { 'content-type': 'application/json', 'x-nexus-view': '1' },
            body: JSON.stringify({ page, session: c.sessions.list?.getSnapshot()?.current ?? '', focused: document.visibilityState === 'visible' && document.hasFocus() }),
            signal: AbortSignal.timeout(2000),
          });
        } catch { /* Optional observer absent or Harness restarting. */ }
        finally { presenceBusy = false; if (presenceAgain) { presenceAgain = false; void reportView(); } }
      };
      const clearView = () => {
        void fetch('/nexus-notifications/view', { method: 'POST', credentials: 'same-origin', keepalive: true,
          headers: { 'content-type': 'application/json', 'x-nexus-view': '1' }, body: JSON.stringify({ page, session: '', focused: false }) }).catch(() => {});
      };
      window.addEventListener('focus', reportView); window.addEventListener('blur', reportView);
      document.addEventListener('visibilitychange', reportView); window.addEventListener('pagehide', clearView);
      const stopPresence = c.sessions.list?.subscribe(reportView);
      const presenceTimer = setInterval(reportView, 2000); void reportView();
      async function openSession(id) {
        if (stopped || busy || typeof id !== 'string' || !id || id.length > 200) return;
        busy = true;
        try {
          await c.sessions.refresh();
          if (stopped) return;
          await c.sessions.open(id);
          if (shell) await shell.sessionOpened(id);
          const url = new URL(location.href);
          url.searchParams.delete('nexus-session'); history.replaceState(null, '', url);
        } catch { /* Missing session or reconnect: keep intent for a later attempt. */ }
        finally { busy = false; }
      }
      const poll = async () => {
        try { await openSession(shell ? await shell.session() : new URL(location.href).searchParams.get('nexus-session')); } catch {}
      };
      const stop = shell?.onSession(openSession);
      const timer = setInterval(poll, 1500); void poll();
      return () => {
        stopped = true; clearInterval(timer); stop?.(); clearInterval(presenceTimer); stopPresence?.(); clearView();
        window.removeEventListener('focus', reportView); window.removeEventListener('blur', reportView);
        document.removeEventListener('visibilitychange', reportView); window.removeEventListener('pagehide', clearView);
      };
    }));
  } };
} });
