import { execFile } from 'node:child_process';
import path from 'node:path';

// Only accepts the worker's live ChildProcess handle, never a PID from persisted state.
export async function stopDesktopChild(child) {
  if (!child?.pid) return;
  if (process.platform === 'win32') {
    if (child.exitCode !== null || child.signalCode !== null) return;
    await new Promise((resolve, reject) => execFile(path.join(process.env.SystemRoot || 'C:\\Windows', 'System32/taskkill.exe'),
      ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, timeout: 10000 }, error => {
        if (error && child.exitCode === null && child.signalCode === null) reject(error); else resolve();
      }));
  } else {
    // The leader may exit before its descendants. Wait for the owned group,
    // not just the leader, before publishing stopped or removing its files.
    const group = -child.pid;
    const signal = value => { try { process.kill(group, value); return true; } catch (error) { if (error.code === 'ESRCH') return false; throw error; } };
    if (!signal('SIGTERM')) return;
    const started = Date.now(); let forced = false;
    while (true) {
      try { if (!signal(0)) return; }
      catch (error) {
        // Darwin may report EPERM while only unreaped zombies remain in an
        // owned group. After an accepted TERM, wait only; never escalate a
        // denied probe into another signal or treat denial as successful stop.
        if (process.platform !== 'darwin' || error.code !== 'EPERM' || Date.now() - started >= 10000) throw error;
        await new Promise(resolve => setTimeout(resolve, 50));
        continue;
      }
      if (!forced && Date.now() - started >= 750) { signal('SIGKILL'); forced = true; }
      if (Date.now() - started >= 10000) throw new Error('desktop_stop_timeout');
      await new Promise(resolve => setTimeout(resolve, 50));
    }
  }
}
