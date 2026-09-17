import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
export const name = 'nexus-desktop-compat';
const validName = value => typeof value === 'string' && /^[A-Za-z0-9._-]{1,64}$/.test(value) && value !== '.' && value !== '..';

export function createPnpm(subprocess, config, environment = process.env) {
  let active, closed = false;
  function run(argv, cwd, signal) {
    if (environment.NEXUS_DESKTOP_PROBE) throw Error('Package operations are unavailable during a compatibility probe');
    if (closed) throw Error('Nexus desktop package service has been disposed');
    if (active) throw Error('Another desktop pnpm operation is already running');
    signal?.throwIfAborted();
    if (!path.isAbsolute(cwd) || cwd.includes('\0')) throw Error('An absolute working directory is required');
    const env = { ...environment, DSH_HOME: config.home, CI: 'true', npm_config_minimumReleaseAge: '0' };
    // Harness runs in Node, not Electron. Native packages must target this Node ABI.
    for (const key of Object.keys(env)) if (/^(electron_run_as_node|npm_config_(runtime|target|disturl))$/i.test(key)) delete env[key];
    const pathKey = Object.keys(env).find(key => key.toLowerCase() === 'path') || 'PATH';
    env[pathKey] = path.dirname(config.node) + path.delimiter + (env[pathKey] || '');
    env.NODE = config.node;
    const child = subprocess.spawn({ argv, cwd, env, signal, graceMs: 3000, stdio: { stdin: 'ignore', stdout: 'pipe', stderr: 'pipe' } });
    if (!child.stdout || !child.stderr || typeof child.waitForExit !== 'function') { child.terminate(); throw Error('Harness subprocess service lacks the required stream/tree contract'); }
    active = child;
    const done = (async () => {
      try { const outcome = await child.done; return { exitCode: outcome.exitCode, signal: outcome.signal }; }
      finally { try { await child.waitForExit(); } finally { if (active === child) active = undefined; } }
    })();
    return { stdout: child.stdout, stderr: child.stderr, done, cancel: () => child.terminate() };
  }
  function args(argv) {
    if (!Array.isArray(argv) || !argv.length || argv.length > 256 || argv.some(x => typeof x !== 'string' || x.includes('\0') || x.length > 32768)) throw Error('Invalid package command arguments');
    if (argv.some(x => /^(--(profile|home|dir|prefix)(=|$)|-C$)/.test(x))) throw Error('Package commands cannot override the selected profile');
    return [...argv];
  }
  const api = {
    run(argv, signal) {
      if (!config.pnpm) throw Error('Configure pnpm in Nexus runtime settings');
      const command = /\.[cm]?js$/i.test(config.pnpm) ? [config.node, config.pnpm] : [config.pnpm];
      return run([...command, ...args(argv)], config.dir, signal);
    },
    runPlugin(argv, invokingDir, signal) { return run([config.node, config.entry, 'plugin', '--profile', config.profile, ...args(argv)], invokingDir, signal); },
    runExternalMarketPluginInstall(argv, invokingDir, signal) {
      const values = args(argv), targets = values.slice(1).filter(x => !x.startsWith('-'));
      if (values[0] !== 'add' || targets.length !== 1 || !/^(?:@[a-z0-9][a-z0-9._-]*\/)?[a-z0-9][a-z0-9._-]*@\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(targets[0])) throw Error('Market installation requires one exact npm package version');
      return api.runPlugin(values, invokingDir, signal);
    },
  };
  return { api: Object.freeze(api), dispose: async () => { closed = true; if (active) { const child = active; child.terminate(); await child.waitForExit(); } } };
}

function requestSwitch(config, profile) {
  return new Promise((resolve, reject) => {
    const child = spawn(config.bridge, [], { env: { ...process.env, NEXUS_DATA_DIR: config.root }, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'] });
    let text = '', settled = false;
    const finish = error => { if (settled) return; settled = true; clearTimeout(timer); child.stdin.end(); if (error) { child.kill(); reject(error); } else resolve(); };
    const timer = setTimeout(() => finish(Error('Profile switch request timed out; check Nexus before retrying')), 15000);
    child.on('error', finish); child.stdin.on('error', finish); child.stderr.resume();
    child.on('exit', () => { if (!settled) finish(Error('Profile switch adapter exited before acceptance')); });
    child.stdout.on('data', bytes => {
      text += bytes.toString();
      if (text.length > 262144) return finish(Error('Invalid profile switch response'));
      const end = text.indexOf('\n'); if (end < 0) return;
      try { const response = JSON.parse(text.slice(0, end)); finish(response.error ? Error(response.error.message || 'Profile switch rejected') : undefined); }
      catch { finish(Error('Invalid profile switch response')); }
    });
    child.stdin.write(JSON.stringify({ id: 1, command: 'proxy_request', args: { method: 'POST', path: '/v1/desktop/profile', body: { profile, run: process.env.NEXUS_DESKTOP_RUN } } }) + '\n');
  });
}

export function apply(ctx) {
  if (!process.env.NEXUS_DESKTOP_CONTEXT) return;
  const config = JSON.parse(process.env.NEXUS_DESKTOP_CONTEXT);
  // Use the executable actually running this Harness, including PATH launches.
  config.node = process.execPath;
  const index = process.argv.indexOf('--profile');
  config.profile = index >= 0 ? process.argv[index + 1] : config.profile;
  config.home = process.env.DSH_HOME || config.home;
  if (!validName(config.profile) || ![config.home, config.node, config.entry, config.root, config.bridge].every(p => typeof p === 'string' && path.isAbsolute(p) && !p.includes('\0'))) throw Error('Invalid Nexus desktop runtime context');
  config.dir = path.join(config.home, 'profiles', config.profile);
  let disposed = false, selection;
  const profiles = {
    current: Object.freeze({ name: config.profile, dir: config.dir }),
    list() {
      if (disposed) throw Error('Profile service has been disposed');
      return fs.readdirSync(path.join(config.home, 'profiles')).filter(validName).flatMap(name => {
        const dir = path.join(config.home, 'profiles', name), file = path.join(dir, 'package.json');
        try {
          if (fs.existsSync(path.join(dir, '.nexus-compatibility.json'))) return [];
          if (fs.lstatSync(dir).isSymbolicLink() || fs.lstatSync(file).isSymbolicLink() || fs.statSync(file).size > 1048576) return [];
          const manifest = JSON.parse(fs.readFileSync(file, 'utf8'));
          return Array.isArray(manifest.dsh?.profile?.bundles) ? [{ name, dir, selectable: true, current: name === config.profile }] : [];
        } catch { return []; }
      });
    },
    select(profile) {
      if (disposed) return Promise.reject(Error('Profile service has been disposed'));
      if (!validName(profile) || !profiles.list().some(item => item.name === profile)) return Promise.reject(Error('Profile is unavailable'));
      if (selection) return selection.profile === profile ? selection.promise : Promise.reject(Error('Another profile switch is pending'));
      if (!process.env.NEXUS_DESKTOP_RUN || process.env.NEXUS_DESKTOP_PROBE) return Promise.reject(Error('Profile switching is unavailable during a compatibility probe'));
      const promise = requestSwitch(config, profile);
      selection = { profile, promise };
      void promise.catch(() => { selection = undefined; });
      return promise;
    },
  };
  ctx.effect(() => ctx.reflect.provide('desktopProfiles', Object.freeze(profiles)));
  ctx.effect(() => () => { disposed = true; });
  ctx.inject(['subprocess'], c => c.effect(() => {
    const provider = createPnpm(c.subprocess, config);
    const remove = c.reflect.provide('desktopPnpm', provider.api);
    return async () => { await provider.dispose(); await remove(); };
  }));
}
