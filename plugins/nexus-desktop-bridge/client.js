// Harness closure-factory client bundle. No imports and no Electron privileges.
window.__ModuleLoader__.load({ id: '@nexus/desktop-bridge', factory() {
  const labels = ['pending', 'loading', 'active', 'failed', 'disposed', 'unloading'];
  function inspectClient(ctx, settled = true, elapsed = 30000) {
        const entries = [];
        let truncated = false, unknown = false;
        for (const entry of ctx.loader.entries()) {
          // A plugin the user disabled is a deliberate choice, not a failure.
          // Such an entry has no fiber and must never be read as a broken import.
          if (entry.disabled || entry.options?.disabled) continue;
          const state = entry.fiber === undefined ? 'import_failed' : labels[entry.fiber.state];
          if (state === 'active') continue;
          if (!state) unknown = true;
          if (entries.length >= 128) { truncated = true; break; }
          const missing = Object.keys(entry.fiber?.inject || {}).filter(service => ctx.get(service) === undefined);
          if (missing.length > 32) truncated = true;
          entries.push({ name: String(entry.options?.name || entry.id || 'unknown').slice(0, 240),
            state: state || 'unknown', missing: missing.slice(0, 32).map(s => s.slice(0, 240)) });
        }
        const missing_core = ['sessions', 'uiRenderer', 'uiSession', 'uiWorkspace']
          .filter(service => ctx.get(service) === undefined);
        const state = !settled && elapsed < 30000 ? 'checking'
          : unknown ? 'unverified' : entries.length ? 'blocked' : !settled ? 'unverified' : missing_core.length ? 'limited' : 'active';
    const result = { state, entries, missing_core, truncated };
    while (JSON.stringify(result).length > 12000 && result.entries.length) {
      result.entries.pop(); result.truncated = true;
    }
    return result;
  }
  return { inspectClient, apply(ctx) {
    const shell = window.nexusShell;
    let disposed = false;
    // The same activation audit as Harness web boot, also in system browsers.
    // HTTP availability alone must never be presented as client activation.
    let settled = false, healthBusy = false, healthToken;
    const healthStarted = Date.now();
    const reportHealth = async () => {
      if (disposed || healthBusy) return;
      healthBusy = true;
      let observedState;
      try {
        const evidence = inspectClient(ctx, settled, Date.now() - healthStarted);
        const { state } = evidence;
        observedState = state;
        if (healthToken === undefined) {
          const response = await fetch('/nexus-browser-health', {
            method: 'POST', credentials: 'same-origin', headers: { 'content-type': 'application/json', 'x-nexus-health': '1' },
            body: JSON.stringify({ action: 'begin' }), signal: AbortSignal.timeout(2000),
          });
          if (!response.ok) return;
          const value = await response.json();
          if (typeof value.token !== 'string') return;
          healthToken = value.token;
        }
        if (disposed) return;
        const response = await fetch('/nexus-browser-health', {
          method: 'POST', credentials: 'same-origin', headers: { 'content-type': 'application/json', 'x-nexus-health': '1' },
          body: JSON.stringify({ ...evidence, token: healthToken }), signal: AbortSignal.timeout(2000),
        });
        // Do not renew a stale token: this page belongs to an older Host run.
        if (response.status === 409) { disposed = true; clearInterval(healthTimer); return; }
      } catch { /* Unsupported observer or disconnected page: no false success. */ }
      finally {
        healthBusy = false;
        if (!disposed && shell && ['active', 'limited', 'blocked'].includes(observedState)) {
          try { await shell.health(observedState !== 'blocked'); } catch {}
        }
      }
    };
    let auditing = false;
    const audit = async () => {
      if (disposed || auditing) return;
      auditing = true;
      try {
        // Wait for the same quiescence boundary as upstream, including hot reloads.
        await ctx.loader.await();
        if (!disposed) { settled = true; await reportHealth(); }
      } catch { /* No positive evidence if the Loader API is unavailable. */ }
      finally { auditing = false; }
    };
    // A hung import/apply must not suppress the bounded startup observation.
    // Keep one quiescence waiter, but report independently while it is pending.
    const heartbeat = async () => { await reportHealth(); void audit(); };
    const healthStart = setTimeout(heartbeat, 0);
    const healthTimer = setInterval(heartbeat, 5000);
    ctx.effect(() => () => { disposed = true; clearTimeout(healthStart); clearInterval(healthTimer); });
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
