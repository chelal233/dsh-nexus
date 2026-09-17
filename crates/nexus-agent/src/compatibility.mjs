// Runs in a Nexus-owned process tree. This is dependency isolation, not a
// security sandbox: installed plugins execute during the real DSH boot probe.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import semver from './vendor/semver.cjs';

export const checkerVersion = 5;
const marker = '.nexus-compatibility.json';
// Native resolution accepts Windows canonical (\\?\) paths without stripping
// their namespace or weakening the containment checks below.
const readJson = p => JSON.parse(fs.readFileSync(p, 'utf8'));
const within = (root, p) => { const rel = path.relative(root, p); return rel === '' || (!rel.startsWith('..' + path.sep) && rel !== '..' && !path.isAbsolute(rel)); };
const validName = name => typeof name === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(name) && !name.endsWith('.');
const validPackage = name => /^(?:@[\w.-]+\/)?[\w.-]+$/.test(name) && !name.split('/').some(p => p === '.' || p === '..');
const profileFiles = ['package.json', 'pnpm-lock.yaml', 'cordis.patch.yml', 'pnpm-workspace.yaml'];
const homeFiles = ['settings.yaml', 'cordis.patch.yml', '.env'];
const retiredDirectory = '.nexus-retired-profiles';
function atomicJson(file, value) {
  const tmp = file + '.' + crypto.randomUUID() + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(value, null, 2), { flag: 'wx' });
  fs.renameSync(tmp, file);
}

export function sourceInfo(home, selected) {
  if (!validName(selected)) throw Error('Invalid source profile name');
  let source = selected;
  const seen = new Set();
  while (true) {
    const live = path.join(home, 'profiles', source, marker);
    const file = fs.existsSync(live) ? live : path.join(home, retiredDirectory, source, marker);
    if (!fs.existsSync(file)) break;
    if (seen.has(source) || seen.size > 4) throw Error('Compatibility profile source cycle');
    seen.add(source);
    const record = readJson(file);
    if (record.effective_profile !== source || !['passed', 'isolated'].includes(record.status)) throw Error('Invalid legacy profile identity');
    source = record.source_profile;
    if (!validName(source)) throw Error('Invalid compatibility source');
  }
  const dir = fs.realpathSync.native(path.join(home, 'profiles', source));
  if (!within(fs.realpathSync.native(path.join(home, 'profiles')), dir)) throw Error('Source profile escapes profiles directory');
  const manifest = readJson(path.join(dir, 'package.json'));
  const bundles = manifest.dsh?.profile?.bundles;
  if (!Array.isArray(bundles) || !bundles.every(x => typeof x === 'string' && validPackage(x))) throw Error('Unsupported profile bundle manifest');
  const policy = path.join(home, 'profiles', '.nexus-plugin-isolation', source + '.json');
  let manualDisabled = [], fingerprint = profileFingerprint(home, dir);
  if (manifest.dsh?.profile?.nexusIsolationPolicyVersion !== 1 && fs.existsSync(policy)) {
    if (fs.lstatSync(policy).isSymbolicLink()) throw Error('Plugin isolation policy cannot be a link');
    const bytes = fs.readFileSync(policy);
    const choices = JSON.parse(bytes.toString('utf8'));
    if (!Array.isArray(choices) || !choices.every(p => typeof p === 'string' && validPackage(p) && !p.startsWith('@deepseek-ai/'))) throw Error('Invalid plugin isolation policy');
    manualDisabled = [...new Set(choices)].filter(p => bundles.includes(p));
    fingerprint = crypto.createHash('sha256').update(fingerprint).update(bytes).digest('hex');
  }
  const saved = manifest.dsh?.profile?.nexusDisabledBundles ?? [];
  if (!Array.isArray(saved) || !saved.every(item => typeof item.package === 'string' && validPackage(item.package))) throw Error('Invalid disabled bundle metadata');
  manualDisabled = [...new Set([...manualDisabled, ...saved.map(item => item.package)])];
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
      const target = fs.realpathSync.native(dir);
      if (!within(slot, target)) throw Error('Official package escapes target release');
      if (packages.has(pkg.name) && packages.get(pkg.name) !== target) throw Error('Duplicate official package in release');
      packages.set(pkg.name, target);
    }
  }
  if (!packages.size) throw Error('Target release has no supported official package layout');
  return packages;
}

// Read data only: never import plugin code or fetch registry metadata. Preserve
// npm's prerelease semantics; incomplete declarations remain unknown.
export function declarationChecks(source, slot, official) {
  // Unsupported release layouts are diagnosed by the startup check itself.
  // Declaration reporting must not prevent that check from writing its report.
  if (!official) { try { official = officialPackages(slot); } catch { official = new Map(); } }
  const read = file => { try { return readJson(file); } catch { return null; } };
  const hostVersion = read(path.join(slot, 'apps/cli/package.json'))?.version;
  const results = [];
  for (const name of source.manifest.dsh.profile.bundles) {
    if (name.startsWith('@deepseek-ai/') || source.manualDisabled.includes(name)) continue;
    const manifest = read(path.join(source.dir, 'node_modules', name, 'package.json'));
    const declarations = [];
    const add = (dependency, required, actual, optional = false) => {
      const range = typeof required === 'string' ? required : null;
      const version = typeof actual === 'string' ? actual : null;
      const valid = range && semver.validRange(range) !== null && version && semver.valid(version);
      declarations.push({ dependency, required: range, actual: version, optional,
        status: valid ? semver.satisfies(version, range) ? 'match' : 'mismatch' : 'unknown' });
    };
    const engine = manifest?.engines?.dsh ?? manifest?.dsh?.engines?.dsh;
    if (engine !== undefined) add('dsh', engine, hostVersion);
    for (const [dependency, required] of Object.entries(manifest?.peerDependencies ?? {})) {
      if (!dependency.startsWith('@deepseek-ai/')) continue;
      const target = official.get(dependency);
      add(dependency, required, target ? read(path.join(target, 'package.json'))?.version : null,
        manifest?.peerDependenciesMeta?.[dependency]?.optional === true);
    }
    results.push({ package: name, version: typeof manifest?.version === 'string' ? manifest.version : null,
      status: declarations.some(d => d.status === 'mismatch' && !d.optional) ? 'mismatch'
        : declarations.length && declarations.every(d => d.status === 'match') ? 'match' : 'unknown', declarations });
  }
  return results;
}

function moduleFilter(source, official, planning = false) {
  const canonical = fs.realpathSync.native(source), started = Date.now(); let count = 0;
  return p => {
    if (++count > 150000 || (planning && Date.now() - started > 10000)) throw Error('Profile dependency scan exceeds file or time limit');
    const rel = path.relative(source, p).split(path.sep);
    if (rel[0] === '.bin' || rel[0] === '.pnpm') return false;
    const offset = rel.lastIndexOf('node_modules') + 1;
    const name = rel[offset]?.startsWith('@') ? rel.slice(offset, offset + 2).join('/') : rel[offset];
    if (official.has(name)) return false;
    if (!within(canonical, fs.realpathSync.native(p))) throw Error('Dependency link escapes source node_modules');
    return true;
  };
}
function copyModules(source, target, official) {
  if (!fs.existsSync(source)) return;
  fs.cpSync(source, target, { recursive: true, dereference: true, filter: moduleFilter(source, official) });
}

export function planCanary(options) {
  const source = sourceInfo(options.home, options.selected), official = officialPackages(fs.realpathSync.native(options.slot));
  const modules = path.join(source.dir, 'node_modules'); let bytes = 0, files = 0;
  const add = file => { const size = fs.statSync(file).size; bytes += size + 4096; files++; if (!Number.isSafeInteger(bytes)) throw Error('Copy size exceeds supported budget'); };
  if (fs.existsSync(modules)) {
    const filter = moduleFilter(modules, official, true), queue = [modules], began = Date.now(); let scheduled = 1;
    while (queue.length) {
      const file = queue.pop(); if (!filter(file)) continue;
      const stat = fs.statSync(file);
      if (stat.isDirectory()) {
        bytes += 4096; const directory = fs.opendirSync(file);
        try { let entry; while ((entry = directory.readSync()) !== null) {
          if (++scheduled > 150000 || Date.now() - began > 10000) throw Error('Profile dependency scan exceeds file or time limit');
          queue.push(path.join(file,entry.name));
        } } finally {directory.closeSync();}
      }
      else if (stat.isFile()) add(file); else throw Error('Unsupported dependency file');
    }
  }
  for (const file of [...profileFiles.map(name => path.join(source.dir,name)), ...homeFiles.map(name => path.join(options.home,name))]) if (fs.existsSync(file)) add(file);
  // Single-round copies are reclaimed before the next one. Reserve working/log overhead, not twenty copies.
  const report = {copy_bytes:bytes, files, required_bytes:Math.ceil(bytes * 1.1) + 64 * 1024 * 1024, fingerprint:source.fingerprint};
  atomicJson(options.output,report); return report;
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

async function stopProbe(child, grouped = true) {
  if (!child.pid || child.exitCode !== null || child.signalCode !== null) return;
  // Windows taskkill is scoped to the exact child tree created by this probe.
  if (process.platform === 'win32') {
    await new Promise((resolve, reject) => {
      const killer = spawn(path.join(process.env.SystemRoot || 'C:/Windows', 'System32/taskkill.exe'), ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
      killer.on('error', reject); killer.on('exit', resolve);
    });
  } else { try { process.kill(grouped ? -child.pid : child.pid, 'SIGKILL'); } catch (e) { if (e.code !== 'ESRCH') throw e; } }
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

export async function probe(node, entry, home, profile, timeoutMs, patches = [], owned = false) {
  // Node's main-module loader cannot consume Windows verbatim paths.
  // Resolve only the entry at the child boundary; keep source identity unchanged.
  entry = fs.realpathSync.native(entry);
  const child = spawn(node, [entry, '--profile', profile, ...patches.flatMap(p => ['--patch', p]), '--no-open', '--host', '127.0.0.1', '--port', '0'], {
    cwd: home, windowsHide: true, detached: process.platform !== 'win32' && !owned,
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
  } finally { await stopProbe(child, !owned); }
}

export function boundDeclarationReport(report) {
  const rows = report.declarations ?? [];
  // Reserve room for failure text/candidates added by the caller. Measure the
  // indented cache envelope too: its extra indentation costs real bytes.
  const bounded = { ...report, declarations: [], declarations_omitted: rows.length + (report.declarations_omitted ?? 0) };
  const order = { mismatch: 0, unknown: 1, match: 2 };
  for (const row of [...rows].sort((a, b) => (order[a.status] ?? 1) - (order[b.status] ?? 1))) {
    bounded.declarations.push(row);
    bounded.declarations_omitted--;
    if (Buffer.byteLength(JSON.stringify({ key: '0'.repeat(64), report: bounded }, null, 2)) > 32 * 1024) {
      bounded.declarations.pop();
      bounded.declarations_omitted++;
    }
  }
  return bounded;
}

export async function check(options) {
  const { home, selected, release_id, node, output, work, force = false, timeout_ms = 45000 } = options;
  if (options.finalize) throw Error('Legacy projection publication is no longer supported; retry the check');
  const trigger = ['version_switch', 'profile_switch', 'startup', 'manual_check'].includes(options.trigger) ? options.trigger : null;
  const slot = fs.realpathSync.native(options.slot), source = sourceInfo(home, selected);
  const declarations = declarationChecks(source, slot);
  const patches = [...(options.builtin_patches ?? []), ...(options.patches ?? [])];
  const fingerprint = crypto.createHash('sha256').update(JSON.stringify([checkerVersion, slot, release_id, source.source, source.fingerprint,
    options.preferences_env ?? {}, options.preference_capabilities ?? null, options.builtin_fingerprint ?? null, declarations]));
  for (const file of patches) fingerprint.update(file).update(fs.readFileSync(file));
  const key = fingerprint.digest('hex');
  const cache = options.cache;
  if (cache && !force && fs.existsSync(cache)) {
    if (fs.lstatSync(cache).isSymbolicLink()) throw Error('Invalid compatibility result cache');
    // Older checkers could write oversized advisory details. Ignore that cache
    // and regenerate; a disposable optimization must not prevent startup.
    const cached = fs.statSync(cache).size <= 65536 ? readJson(cache) : {};
    if (cached.key === key && cached.report?.checker_version === checkerVersion && ['passed', 'isolated'].includes(cached.report.status)) {
      const report = { ...cached.report, last_trigger: trigger, last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: true };
      atomicJson(output, report); return report;
    }
  }
  const scratch = path.join(work, crypto.randomUUID()), testHome = path.join(scratch, 'home');
  const candidate = path.join(testHome, 'profiles', source.source);
  fs.mkdirSync(candidate, { recursive: true });
  const disabled = source.manualDisabled.map(packageName => ({ package: packageName, reason: 'Disabled by user' }));
  let failureText = '';
  const report = status => boundDeclarationReport({ checker_version: checkerVersion, status, source_profile: source.source,
    effective_profile: source.source, release_id, fingerprint: source.fingerprint, checked_at_unix: Math.floor(Date.now() / 1000),
    disabled, declarations, checked_disabled_plugins: source.manualDisabled, trigger, last_trigger: trigger,
    last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: false });
  try {
    const official = officialPackages(slot);
    copyModules(path.join(source.dir, 'node_modules'), path.join(candidate, 'node_modules'), official);
    for (const [name, target] of official) {
      const link = path.join(candidate, 'node_modules', name);
      fs.mkdirSync(path.dirname(link), { recursive: true });
      fs.symlinkSync(target, link, process.platform === 'win32' ? 'junction' : 'dir');
    }
    for (const name of profileFiles) {
      const from = path.join(source.dir, name);
      if (fs.existsSync(from)) fs.copyFileSync(from, path.join(candidate, name));
    }
    for (const name of homeFiles) {
      const from = path.join(home, name);
      if (fs.existsSync(from)) fs.copyFileSync(from, path.join(testHome, name));
    }
    // A check sees the same manifest as the real launch. Disabling a plugin is
    // an explicit, atomic edit to the user's profile, never a hidden projection.
    const result = await probe(node, path.join(slot, 'apps/cli/lib/bin.js'), testHome, source.source, timeout_ms, patches, options.owned_round === true);
    failureText = result.text;
    if (!result.ok && /failed to (?:import|apply) loader entry nexus-(?:desktop-compat|desktop-bridge|notifications)\b/.test(result.text)) {
      throw Error('A Nexus built-in plugin failed to load. Update or repair Nexus; do not disable third-party plugins. Original error: ' + result.text.slice(-4000));
    }
    if (!result.ok) throw Error('Startup check needs an explicit plugin decision; original profile preserved. Original error: ' + result.text.slice(-4000));
    if (sourceInfo(home, selected).fingerprint !== source.fingerprint) throw Error('Source profile changed during compatibility check');
    const passed = report(disabled.length ? 'isolated' : 'passed');
    if (cache) atomicJson(cache, { key, report: passed });
    atomicJson(output, passed); return passed;
  } catch (error) {
    const failures = loaderFailures(failureText, source.manifest.dsh.profile.bundles);
    atomicJson(output, boundDeclarationReport({ ...report('needs_choice'), error: String(error.message).slice(0, 4600),
      candidates: source.manifest.dsh.profile.bundles.filter(p => !p.startsWith('@deepseek-ai/')).map(packageName => ({ package: packageName,
        reason: failures.has(packageName) ? 'DSH reported a loader error for this plugin' : 'Not identified as faulty; optional isolation for troubleshooting' })) }));
    throw error;
  } finally {
    // The Rust owner first reconciles the entire child tree, including a crash,
    // before removing an owned round. Standalone probes await stopTree above.
    if (!options.owned_round && fs.existsSync(scratch)) {
      if (!within(fs.realpathSync.native(work), fs.realpathSync.native(scratch)) || fs.lstatSync(scratch).isSymbolicLink()) throw Error('Unsafe compatibility cleanup path');
      fs.rmSync(scratch, { recursive: true, force: true });
    }
  }
}

export async function canarySearch(candidates, test, mode = 'bisect', limit = 20) {
  const rounds = [];
  const run = async enabled => {
    if (rounds.length >= limit) return null;
    const result = await test(enabled);
    rounds.push({ enabled_bundles: [...enabled], ...result });
    return result.outcome;
  };
  const original = await run(candidates);
  if (original !== 'failed' || mode === 'diagnostic_only') return { outcome: original ?? 'inconclusive', rounds, suspect_combination: [] };
  if (await run([]) !== 'passed') return { outcome: 'inconclusive', rounds, suspect_combination: [], reason: 'The baseline also failed or could not be verified.' };
  let selected = [...candidates], width = 2;
  while (selected.length > 1 && rounds.length < limit - 1) {
    const size = Math.ceil(selected.length / width);
    const chunks = Array.from({ length: Math.ceil(selected.length / size) }, (_, i) => selected.slice(i * size, (i + 1) * size));
    let reduced = false;
    for (const chunk of [...chunks, ...chunks.map(chunk => selected.filter(x => !chunk.includes(x)))]) {
      if (!chunk.length || chunk.length === selected.length) continue;
      const outcome = await run(chunk);
      if (outcome === null || outcome === 'inconclusive') return { outcome: 'inconclusive', rounds, suspect_combination: selected, reason: 'The execution budget or a probe limitation prevented attribution.' };
      if (outcome === 'failed') { selected = chunk; width = 2; reduced = true; break; }
    }
    if (reduced) continue;
    if (width >= selected.length) break;
    width = Math.min(selected.length, width * 2);
  }
  const confirmed = await run(selected);
  return { outcome: confirmed === 'failed' ? 'failed' : 'inconclusive', rounds, suspect_combination: selected,
    reason: 'A reproduced combination is evidence of a conflict, not proof that one plugin is defective.' };
}

export async function checkCanary(options) {
  const { home, selected, node, slot, work, output, mode, patches = [] } = options;
  if (!['diagnostic_only', 'bisect'].includes(mode)) throw Error('Unsupported Canary mode');
  const source = sourceInfo(home, selected), official = officialPackages(fs.realpathSync.native(slot));
  const bundles = source.manifest.dsh.profile.bundles.filter(name => !source.manualDisabled.includes(name));
  const allCandidates = bundles.filter(name => !name.startsWith('@deepseek-ai/'));
  const candidates = options.subset ?? allCandidates;
  if (!Array.isArray(candidates) || candidates.some(name => !allCandidates.includes(name)) || new Set(candidates).size !== candidates.length) throw Error('Invalid Canary subset');
  const patchHash = file => {
    const fd = fs.openSync(file, 'r');
    try {
      const buffer = Buffer.alloc(1024 * 1024 + 1);
      const size = fs.readSync(fd, buffer, 0, buffer.length, 0);
      if (size > 1024 * 1024) throw Error('Canary patch exceeds size limit');
      return crypto.createHash('sha256').update(buffer.subarray(0, size)).digest('hex');
    } finally { fs.closeSync(fd); }
  };
  const probePatches = [...(options.builtin_patches ?? []), ...patches];
  const patchHashes = probePatches.map(file => ({ file, sha256: patchHash(file) }));
  const verifyPatches = () => { for (const {file, sha256} of patchHashes) if (patchHash(file) !== sha256) throw Error('Patch changed during Canary; attribution invalid'); };
  const started = Date.now();
  const result = await canarySearch(candidates, async enabled => {
    verifyPatches();
    const scratch = path.join(work, crypto.randomUUID()), testHome = path.join(scratch, 'home');
    const candidate = path.join(testHome, 'profiles', 'canary');
    const began = Date.now();
    fs.mkdirSync(candidate, { recursive: true });
    try {
      copyModules(path.join(source.dir, 'node_modules'), path.join(candidate, 'node_modules'), official);
      for (const [name, target] of official) {
        const link = path.join(candidate, 'node_modules', name); fs.mkdirSync(path.dirname(link), { recursive: true });
        fs.symlinkSync(target, link, process.platform === 'win32' ? 'junction' : 'dir');
      }
      for (const name of profileFiles.filter(name => name !== 'package.json')) {
        const from = path.join(source.dir, name); if (fs.existsSync(from)) fs.copyFileSync(from, path.join(candidate, name));
      }
      for (const name of homeFiles) { const from = path.join(home, name); if (fs.existsSync(from)) fs.copyFileSync(from, path.join(testHome, name)); }
      const manifest = structuredClone(source.manifest);
      manifest.dsh.profile.bundles = bundles.filter(name => name.startsWith('@deepseek-ai/') || enabled.includes(name));
      fs.writeFileSync(path.join(candidate, 'package.json'), JSON.stringify(manifest));
      const remaining = 540000 - (Date.now() - started);
      if (remaining <= 0) return { outcome: 'inconclusive', reason: 'Total Canary budget exceeded', duration_ms: Date.now() - began };
      const result = await probe(node, path.join(slot, 'apps/cli/lib/bin.js'), testHome, 'canary', Math.min(45000, remaining), probePatches, options.owned_round === true);
      return { outcome: result.ok ? 'passed' : 'failed', reason: result.ok ? 'Loader and HTML readiness passed' : 'Process exited or reported a loader failure', raw_error: result.ok ? undefined : result.text.slice(-4000), duration_ms: Date.now() - began };
    } catch (error) {
      // Rust redacts the bounded report before publishing it to the API.
      return { outcome: 'inconclusive', reason: 'Probe timed out or could not be prepared; no plugin attribution was made', raw_error: String(error.message).slice(-4000), duration_ms: Date.now() - began };
    } finally {
      if (!options.owned_round && fs.existsSync(scratch)) { if (!within(fs.realpathSync.native(work), fs.realpathSync.native(scratch))) throw Error('Unsafe Canary cleanup path'); fs.rmSync(scratch, { recursive: true, force: true }); }
    }
  }, mode, 20);
  verifyPatches();
  if (sourceInfo(home, selected).fingerprint !== source.fingerprint) throw Error('Source profile changed during Canary verification');
  const original = result.rounds[0]?.outcome ?? 'inconclusive';
  const report = { ...result, source_profile: source.source, release_id: options.release_id, fingerprint: source.fingerprint,
    all_candidates: allCandidates, patches: patchHashes, checks: { startup: original, loader_and_web_document: original, feature: 'unsupported' },
    feature_reason: 'No verified command, panel or interaction adapter is available for this Harness version.',
    isolation: 'Temporary profile and HOME only; plugins retain operating-system and network access.',
    source_profile_modified_by_nexus: false };
  atomicJson(output, report); return report;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const options = readJson(process.argv[2]);
  try { await (options.canary_plan ? planCanary(options) : options.mode ? checkCanary(options) : check(options)); }
  catch (error) { console.error(String(error.message).slice(0, 4600)); process.exitCode = 1; }
}
