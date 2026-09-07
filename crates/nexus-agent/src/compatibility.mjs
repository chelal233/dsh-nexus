// Runs in a Nexus-owned process tree. This is dependency isolation, not a
// security sandbox: installed plugins execute during the real DSH boot probe.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';

export const checkerVersion = 1;
const marker = '.nexus-compatibility.json';
const readJson = p => JSON.parse(fs.readFileSync(p, 'utf8'));
const within = (root, p) => { const rel = path.relative(root, p); return rel === '' || (!rel.startsWith('..' + path.sep) && rel !== '..' && !path.isAbsolute(rel)); };
const validName = name => typeof name === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(name) && !name.endsWith('.');
const validPackage = name => /^(?:@[\w.-]+\/)?[\w.-]+$/.test(name) && !name.split('/').some(p => p === '.' || p === '..');
const profileFiles = ['package.json', 'pnpm-lock.yaml', 'cordis.patch.yml', 'pnpm-workspace.yaml'];
const homeFiles = ['settings.yaml', 'cordis.patch.yml', '.env'];
function atomicJson(file, value) {
  const tmp = file + '.' + crypto.randomUUID() + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(value, null, 2), { flag: 'wx' });
  fs.renameSync(tmp, file);
}

export function sourceInfo(home, selected) {
  if (!validName(selected)) throw Error('Invalid source profile name');
  let source = selected;
  const seen = new Set();
  while (fs.existsSync(path.join(home, 'profiles', source, marker))) {
    if (seen.has(source) || seen.size > 4) throw Error('Compatibility profile source cycle');
    seen.add(source);
    source = readJson(path.join(home, 'profiles', source, marker)).source_profile;
    if (!validName(source)) throw Error('Invalid compatibility source');
  }
  const dir = fs.realpathSync(path.join(home, 'profiles', source));
  if (!within(fs.realpathSync(path.join(home, 'profiles')), dir)) throw Error('Source profile escapes profiles directory');
  const manifest = readJson(path.join(dir, 'package.json'));
  const bundles = manifest.dsh?.profile?.bundles;
  if (!Array.isArray(bundles) || !bundles.every(x => typeof x === 'string' && validPackage(x))) throw Error('Unsupported profile bundle manifest');
  const policy = path.join(home, 'profiles', '.nexus-plugin-isolation', source + '.json');
  let manualDisabled = [], fingerprint = profileFingerprint(home, dir);
  if (fs.existsSync(policy)) {
    if (fs.lstatSync(policy).isSymbolicLink()) throw Error('Plugin isolation policy cannot be a link');
    const bytes = fs.readFileSync(policy);
    const choices = JSON.parse(bytes.toString('utf8'));
    if (!Array.isArray(choices) || !choices.every(p => typeof p === 'string' && validPackage(p) && !p.startsWith('@deepseek-ai/'))) throw Error('Invalid plugin isolation policy');
    manualDisabled = [...new Set(choices)].filter(p => bundles.includes(p));
    fingerprint = crypto.createHash('sha256').update(fingerprint).update(bytes).digest('hex');
  }
  return { source, dir, manifest, fingerprint, manualDisabled };
}

function profileFingerprint(home, dir) {
  const hash = crypto.createHash('sha256');
  // Configuration and installed package manifests determine cache validity.
  for (const name of profileFiles) {
    const file = path.join(dir, name);
    hash.update(name).update(fs.existsSync(file) ? fs.readFileSync(file) : '<missing>');
  }
  for (const name of homeFiles) {
    const file = path.join(home, name);
    hash.update('home/' + name).update(fs.existsSync(file) ? fs.readFileSync(file) : '<missing>');
  }
  const modules = path.join(dir, 'node_modules');
  function manifests(root, depth = 0) {
    if (depth > 2 || !fs.existsSync(root)) return;
    for (const name of fs.readdirSync(root).sort()) {
      if (name.startsWith('.')) continue;
      const p = path.join(root, name);
      const m = path.join(p, 'package.json');
      if (fs.existsSync(m)) hash.update(path.relative(modules, m)).update(fs.readFileSync(m));
      else if (name.startsWith('@')) manifests(p, depth + 1);
    }
  }
  manifests(modules);
  return hash.digest('hex');
}

function officialPackages(slot) {
  const packages = new Map();
  for (const base of ['vendor', 'packages']) {
    const root = path.join(slot, base);
    if (!fs.existsSync(root)) continue;
    const dirs = [];
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const dir = path.join(root, entry.name); dirs.push(dir);
      if (base === 'packages') for (const child of fs.readdirSync(dir, { withFileTypes: true })) {
        if (child.isDirectory()) dirs.push(path.join(dir, child.name));
      }
    }
    for (const dir of dirs) {
      const file = path.join(dir, 'package.json');
      if (!fs.existsSync(file)) continue;
      const pkg = readJson(file);
      if (!pkg.name?.startsWith('@deepseek-ai/') || !validPackage(pkg.name)) continue;
      const target = fs.realpathSync(dir);
      if (!within(slot, target)) throw Error('Official package escapes target release');
      if (packages.has(pkg.name) && packages.get(pkg.name) !== target) throw Error('Duplicate official package in release');
      packages.set(pkg.name, target);
    }
  }
  if (!packages.size) throw Error('Target release has no supported official package layout');
  return packages;
}

function copyModules(source, target, official) {
  if (!fs.existsSync(source)) return;
  const canonical = fs.realpathSync(source);
  let count = 0;
  fs.cpSync(source, target, { recursive: true, dereference: true, filter: p => {
    if (++count > 150000) throw Error('Profile dependency copy exceeds file limit');
    const rel = path.relative(source, p).split(path.sep);
    if (rel[0] === '.bin' || rel[0] === '.pnpm') return false;
    const offset = rel.lastIndexOf('node_modules') + 1;
    const name = rel[offset]?.startsWith('@') ? rel.slice(offset, offset + 2).join('/') : rel[offset];
    if (official.has(name)) return false;
    // External workspace links need an explicit import, never traverse them
    // while preparing a supposedly self-contained profile copy.
    if (!within(canonical, fs.realpathSync(p))) throw Error('Dependency link escapes source node_modules');
    return true;
  } });
}

export function incompatibleBundles(text, bundles) {
  const found = new Map();
  const signatures = /does not provide an export named|is not a function|Cannot find (?:package|module)|ERR_PACKAGE_PATH_NOT_EXPORTED/;
  for (const line of text.split('\n')) {
    if (!signatures.test(line)) continue;
    const matches = [...line.matchAll(/failed to (?:import|apply) loader entry [^()\r\n]+ \(([^()]+)\):/g)];
    // The innermost error identifies the failing package, not a parent include.
    const pkg = matches.at(-1)?.[1];
    if (pkg && !pkg.startsWith('@deepseek-ai/') && bundles.includes(pkg)) {
      found.set(pkg, line.slice(line.lastIndexOf('):') + 2).trim().slice(0, 240));
    }
  }
  return [...found].map(([packageName, reason]) => ({ package: packageName, reason }));
}

async function stopProbe(child) {
  if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
  // Windows taskkill is scoped to the exact child tree created by this probe.
  if (process.platform === 'win32') {
    await new Promise((resolve, reject) => {
      const killer = spawn(path.join(process.env.SystemRoot || 'C:/Windows', 'System32/taskkill.exe'), ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
      killer.on('error', reject); killer.on('exit', resolve);
    });
  } else { try { process.kill(-child.pid, 'SIGKILL'); } catch (e) { if (e.code !== 'ESRCH') throw e; } }
  if (child.exitCode !== null || child.signalCode !== null) return;
  await Promise.race([
    new Promise(resolve => child.once('exit', resolve)),
    new Promise((_, reject) => setTimeout(() => reject(Error('Probe process cleanup timed out')), 5000).unref()),
  ]);
}

function loaderFailures(text, bundles) {
  const found = new Set();
  for (const line of text.split('\n')) {
    const matches = [...line.matchAll(/failed to (?:import|apply) loader entry [^()\r\n]+ \(([^()]+)\):/g)];
    const pkg = matches.at(-1)?.[1];
    if (pkg && !pkg.startsWith('@deepseek-ai/') && bundles.includes(pkg)) found.add(pkg);
  }
  return found;
}

async function webReady(address) {
  let url = new URL(address);
  const origin = url.origin;
  const cookies = new Map();
  for (let hop = 0; hop < 4; hop++) {
    const response = await fetch(url, {
      redirect: 'manual', signal: AbortSignal.timeout(2000),
      headers: cookies.size ? { cookie: [...cookies].map(([k, v]) => `${k}=${v}`).join('; ') } : {},
    });
    for (const cookie of response.headers.getSetCookie()) {
      const pair = cookie.split(';', 1)[0], eq = pair.indexOf('=');
      if (eq > 0) cookies.set(pair.slice(0, eq), pair.slice(eq + 1));
    }
    if ([301, 302, 303, 307, 308].includes(response.status)) {
      const location = response.headers.get('location');
      await response.body?.cancel();
      if (!location) return false;
      url = new URL(location, url);
      if (url.origin !== origin || url.username || url.password) return false;
      continue;
    }
    return response.ok && /<html[\s>]/i.test(await response.text());
  }
  return false;
}

export async function probe(node, entry, home, profile, timeoutMs, patches = []) {
  const child = spawn(node, [entry, '--profile', profile, ...patches.flatMap(p => ['--patch', p]), '--no-open', '--host', '127.0.0.1', '--port', '0'], {
    cwd: home, windowsHide: true, detached: process.platform !== 'win32',
    env: { ...process.env, DSH_HOME: home }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let text = '', exit = null, spawnError;
  child.on('error', e => { spawnError = e; });
  child.on('exit', code => { exit = code ?? -1; });
  for (const stream of [child.stdout, child.stderr]) stream.on('data', data => {
    text = (text + data.toString()).slice(-256 * 1024);
  });
  const deadline = Date.now() + timeoutMs;
  let readySince = 0, errorSince = 0;
  try {
    while (Date.now() < deadline) {
      if (spawnError) throw spawnError;
      if (exit !== null) return { ok: false, text };
      if (/failed to (?:import|apply) loader entry/.test(text)) {
        errorSince ||= Date.now();
        if (Date.now() - errorSince >= 300) return { ok: false, text };
        await new Promise(resolve => setTimeout(resolve, 100));
        continue;
      }
      const url = text.match(/dsh web: (http:\/\/(?:127\.0\.0\.1|localhost):\d+\/[^\s\x1b]*)/)?.[1];
      if (url) {
        try {
          if (await webReady(url)) {
            readySince ||= Date.now();
            if (Date.now() - readySince >= 1000 && !/failed to (?:import|apply) loader entry/.test(text)) return { ok: true, text: '' };
          }
        } catch { /* readiness may precede the listener */ }
      }
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw Error('Compatibility startup probe timed out; no plugins were guessed or disabled');
  } finally { await stopProbe(child); }
}

export async function check(options) {
  const { home, selected, release_id, node, output, work, force = false, timeout_ms = 45000 } = options;
  const trigger = ['version_switch', 'profile_switch', 'startup'].includes(options.trigger) ? options.trigger : null;
  const slot = fs.realpathSync(options.slot);
  const source = sourceInfo(home, selected);
  const patches = options.patches ?? [];
  const preferencesFingerprint = crypto.createHash('sha256').update(JSON.stringify({ environment: options.preferences_env ?? {}, capabilities: options.preference_capabilities ?? null }));
  for (const patch of patches) preferencesFingerprint.update(patch).update(fs.readFileSync(patch));
  const key = crypto.createHash('sha256').update(JSON.stringify([checkerVersion, slot, release_id, source.source, source.fingerprint, preferencesFingerprint.digest('hex')])).digest('hex');
  const effective = 'nexus-' + key.slice(0, 24);
  const destination = path.join(home, 'profiles', effective);
  const metadataFile = path.join(destination, marker);
  let cached;
  let projectionBefore;
  if (fs.existsSync(destination)) {
    if (fs.lstatSync(destination).isSymbolicLink()) throw Error('Projection destination cannot be a link');
    cached = readJson(metadataFile);
    if (cached.fingerprint !== source.fingerprint || cached.release_id !== release_id || cached.checker_version !== checkerVersion || cached.source_profile !== source.source) {
      throw Error('Projection destination ownership mismatch');
    }
    projectionBefore = profileFingerprint(home, destination);
    if (!force && cached.projection_fingerprint === projectionBefore) {
      const reused = { ...cached, trigger: cached.trigger ?? null, last_trigger: trigger,
        last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: true };
      atomicJson(output, reused); return reused;
    }
  }
  // Never reuse/mutate an old published projection during checking.
  const scratch = path.join(work, crypto.randomUUID());
  const testHome = path.join(scratch, 'home');
  const candidate = path.join(testHome, 'profiles', effective);
  fs.mkdirSync(candidate, { recursive: true });
  let disabled = [], failures = new Set();
  try {
  const official = officialPackages(slot);
  // Recheck edited effective profiles without throwing their local changes away.
  const base = cached ? destination : source.dir;
  copyModules(path.join(base, 'node_modules'), path.join(candidate, 'node_modules'), official);
  for (const [name, target] of official) {
    const link = path.join(candidate, 'node_modules', name);
    fs.mkdirSync(path.dirname(link), { recursive: true });
    fs.symlinkSync(target, link, process.platform === 'win32' ? 'junction' : 'dir');
  }
  for (const name of profileFiles.filter(name => name !== 'package.json')) {
    const from = path.join(base, name);
    if (fs.existsSync(from)) fs.copyFileSync(from, path.join(candidate, name));
  }
  for (const name of homeFiles) {
    const from = path.join(home, name);
    if (fs.existsSync(from)) fs.copyFileSync(from, path.join(testHome, name));
  }
  const manifest = cached ? readJson(path.join(base, 'package.json')) : structuredClone(source.manifest);
  manifest.name = 'dsh-profile-' + effective;
  disabled = cached ? cached.disabled.filter(item => !manifest.dsh.profile.bundles.includes(item.package)) : [];
  for (const name of source.manualDisabled) {
    if (!disabled.some(item => item.package === name)) disabled.push({package: name, reason: 'Disabled by user'});
    manifest.dsh.profile.bundles = manifest.dsh.profile.bundles.filter(p => p !== name);
    if (manifest.dependencies) delete manifest.dependencies[name];
  }
  for (let attempt = 0; attempt <= Math.min(source.manifest.dsh.profile.bundles.length, 12); attempt++) {
    fs.writeFileSync(path.join(candidate, 'package.json'), JSON.stringify(manifest, null, 2));
    const result = await probe(node, path.join(slot, 'apps/cli/lib/bin.js'), testHome, effective, timeout_ms, patches);
    if (result.ok) {
      if (sourceInfo(home, source.source).fingerprint !== source.fingerprint) throw Error('Source profile changed during compatibility check');
      const report = { checker_version: checkerVersion, status: disabled.length ? 'isolated' : 'passed', source_profile: source.source,
        effective_profile: effective, release_id, fingerprint: source.fingerprint, checked_at_unix: Math.floor(Date.now() / 1000), disabled,
        trigger, last_trigger: trigger, last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: false };
      // Remove generated top-level module links through the disposable HOME.
      const modules = path.join(candidate, 'node_modules');
      for (const name of fs.readdirSync(modules)) {
        const first = path.join(modules, name);
        const entries = name.startsWith('@') && !fs.lstatSync(first).isSymbolicLink()
          ? fs.readdirSync(first).map(child => path.join(first, child)) : [first];
        for (const entry of entries) if (fs.lstatSync(entry).isSymbolicLink()) {
          const target = path.resolve(path.dirname(entry), fs.readlinkSync(entry));
          if (within(path.resolve(scratch), target)) fs.unlinkSync(entry);
        }
      }
      // Remove boot-generated paths whose links refer to the probe HOME.
      for (const name of ['.dsh-module-fallback', '.dsh-market', 'cordis.yml']) {
        const owned = path.resolve(candidate, name);
        if (!within(path.resolve(candidate), owned)) throw Error('Invalid generated cleanup path');
        fs.rmSync(owned, {recursive:true, force:true});
      }
      // The checker owns only generated directories carrying our marker.
      if (fs.existsSync(destination)) {
        if (profileFingerprint(home, destination) !== projectionBefore) throw Error('Effective profile changed during compatibility check');
        const currentManifest = readJson(path.join(destination, 'package.json'));
        if (JSON.stringify(currentManifest) !== JSON.stringify(manifest)) {
          const backup = path.join(destination, 'package.nexus-backup-' + crypto.randomUUID() + '.json');
          fs.copyFileSync(path.join(destination, 'package.json'), backup, fs.constants.COPYFILE_EXCL);
          atomicJson(path.join(destination, 'package.json'), manifest);
        }
        report.projection_fingerprint = profileFingerprint(home, destination);
        atomicJson(metadataFile, report);
      } else {
        report.projection_fingerprint = profileFingerprint(home, candidate);
        atomicJson(path.join(candidate, marker), report);
        // Same volume is required for the atomic profile publication.
        fs.renameSync(candidate, destination);
      }
      atomicJson(output, report);
      return report;
    }
    failures = loaderFailures(result.text, manifest.dsh.profile.bundles);
    const rejected = incompatibleBundles(result.text, manifest.dsh.profile.bundles);
    if (!rejected.length) throw Error('Startup check needs a user decision: choose third-party plugins to disable, then retry the switch; original profile preserved');
    disabled.push(...rejected);
    const names = new Set(rejected.map(x => x.package));
    manifest.dsh.profile.bundles = manifest.dsh.profile.bundles.filter(x => !names.has(x));
    for (const name of names) if (manifest.dependencies) delete manifest.dependencies[name];
  }
  throw Error('Compatibility retry limit reached; original profile preserved');
  } catch (error) {
    // A failed attempt is actionable evidence, not a successful publication.
    // No raw startup log or credential is copied into the public report.
    atomicJson(output, {checker_version: checkerVersion, status: 'needs_choice',
      source_profile: source.source, effective_profile: effective, release_id,
      fingerprint: source.fingerprint, checked_at_unix: Math.floor(Date.now() / 1000), disabled,
      trigger, last_trigger: trigger, last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: false,
      error: String(error.message).slice(0, 600),
      candidates: source.manifest.dsh.profile.bundles.filter(p => !p.startsWith('@deepseek-ai/')).map(packageName => ({
        package: packageName, reason: failures.has(packageName)
          ? 'DSH reported a loader error for this plugin'
          : 'Not identified as faulty; optional isolation for troubleshooting',
      })),
    });
    throw error;
  } finally {
    if (fs.existsSync(scratch)) {
      if (!within(fs.realpathSync(work), fs.realpathSync(scratch)) || fs.lstatSync(scratch).isSymbolicLink()) throw Error('Unsafe compatibility cleanup path');
      fs.rmSync(scratch, {recursive:true, force:true});
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const options = readJson(process.argv[2]);
  try { await check(options); }
  catch (error) { console.error(String(error.message).slice(0, 600)); process.exitCode = 1; }
}
