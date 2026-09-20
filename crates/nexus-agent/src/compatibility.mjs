// Runs in a Nexus-owned process tree. This is dependency isolation, not a
// security sandbox: installed plugins execute during the real DSH boot probe.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import semver from './vendor/semver.cjs';

export const checkerVersion = 13;

// Signatures verified against app-boot/{index,profile}.ts, profile-resolution/
// resolver.ts, loader/config/tree.ts and CLI args.ts in dsh-v0.1.6-alpha.2.
// Classify only a failed operation, never arbitrary background log warnings.
export function diagnoseStartup(text) {
  const rules = [
    ['nexus_integration', /A Nexus built-in plugin failed to load|failed to (?:import|apply) loader entry nexus-(?:desktop-compat|desktop-bridge|notifications)\b/i, 'Nexus integration failed to load', 'Repair or update the Nexus installation, then check startup again. Do not disable unrelated Harness plugins.'],
    ['storage_full', /ENOSPC|no space left on device/i, 'Startup could not write because the disk is full', 'Free space on the affected drive, then retry. Preserve Harness profiles and session data.'],
    ['cleanup_timeout', /Probe process cleanup timed out/i, 'The startup check could not finish stopping its process', 'Inspect the startup log and confirm the previous process has stopped before retrying.'],
    ['inputs_changed', /Source profile changed during compatibility check|Startup inputs changed after verification|Harness identity or entry changed after verification/i, 'Startup inputs changed during verification', 'Run the startup check again using the current configuration. No plugin change is required.'],
    ['port_conflict', /EADDRINUSE|address already in use/i, 'Listening port is occupied', 'Choose another port or stop the known application using it.'],
    ['permission', /EACCES|EPERM|permission denied/i, 'File or port access was denied', 'Check access to the named path or port and file locks; do not delete data to bypass the error.'],
    ['profile_restriction', /profile "desktop" is managed exclusively/, 'Profile reserved by Harness', 'Select a regular profile or a compatible Harness version.'],
    ['duplicate_entry', /duplicate loader entry id:\s*([\w.-]+)/i, 'Conflicting plugin entry ID', 'Inspect the declaring bundles and disable or adjust one conflicting third-party plugin.'],
    ['configuration', /failed to (?:read|parse) (?:profile manifest|config|patches|overlay)|must (?:be a top-level YAML array|hold a JSON object)|must be a mapping|YAMLException|JSONParseError/i, 'Invalid or unreadable configuration', 'Repair the named configuration or patch file; preserve a backup before editing.'],
    ['missing_bundle', /cannot resolve profile bundle ["']([^"']+)|profile bundle .+ declares no dsh\.bundle/i, 'Profile bundle is missing or invalid', 'Repair the selected profile dependencies or select a package that declares a Harness bundle.'],
    ['module_api', /cannot resolve ESM export|ERR_PACKAGE_PATH_NOT_EXPORTED|does not provide an export named|export .+ resolves outside its package|unsupported Node module loader/i, 'Package or runtime interface is incompatible', 'Use compatible plugin, Harness and Node versions; reinstalling the same incompatible version may not help.'],
    ['missing_module', /Cannot find (?:package|module)|ERR_MODULE_NOT_FOUND|MODULE_NOT_FOUND|main entry is missing|Installed dependency link is broken/i, 'Required module is missing', 'Repair the named package in the selected profile and inspect its dependency chain.'],
    ['module_layout', /exists and is not a (?:symlink or dsh-managed module proxy|dsh-managed module proxy)|profile resolution mismatch/i, 'Installed module layout conflicts with Harness', 'Stop Harness and repair the profile dependency installation. Preserve conflicting files before replacing them.'],
    ['restart_required', /profile resolution:.+requires a process restart/i, 'Dependency changes require a restart', 'Stop Harness completely and start it again to load the new dependency generation.'],
    ['patch_target', /cannot resolve entry [\w.-]+|entry [\w.-]+ is not a group/i, 'Patch refers to an unavailable entry', 'Update or disable the named patch for this Harness version; do not disable unrelated plugins.'],
    ['required_services', /Plugins waiting for services|pending \(waiting for services?:/i, 'Required services did not become available', 'Inspect the missing service names and their provider plugins; repair the provider rather than disabling the waiting consumer.'],
    ['plugin_activation', /required plugins? did not activate|disabled expression failed|failed to (?:import|apply) loader entry/i, 'Plugin activation failed', 'Inspect the reported package and its original cause before choosing a compatible version or disabling it.'],
    ['runtime_arguments', /invalid profile name|select a profile only once|error: --|unknown option|no invocation resolved/i, 'Harness launch arguments are invalid', 'Correct the profile or launch arguments for the selected Harness version.'],
    ['package_manifest', /installed package .+ must declare a non-empty version/i, 'Installed package metadata is invalid', 'Repair or replace the named package with a complete published version.'],
    ['process_exit', /Harness process exited before readiness/i, 'Harness exited before becoming ready', 'Inspect the exit status and startup log. An early exit alone does not identify a faulty plugin.'],
    ['readiness_timeout', /Compatibility (?:startup )?probe timed out/i, 'Harness did not become ready in time', 'Inspect the startup log and pending services; a timeout alone does not identify a faulty plugin.'],
  ];
  const rule = rules.find(([, pattern]) => pattern.test(text));
  const lines = text.split(/\r?\n/).map(line => line.trim());
  const evidence = lines.filter(line => line && !/^at |^file:\/\/\/|^throw |^\^|^\[cause\]:?$/.test(line));
  const diagnostic = rule
    ? { code: rule[0], summary: rule[2], remedy: rule[3], evidence: evidence.filter(line => rule[1].test(line)).slice(0, 4) }
    : { code: 'unknown', summary: 'Harness startup failed; cause not yet identified', remedy: 'Keep the original error and startup log. Do not disable plugins without evidence.', evidence: evidence.slice(0, 2) };
  diagnostic.evidence = diagnostic.evidence.map(line => line.slice(0, 1200));
  diagnostic.level = 'blocking';
  diagnostic.certainty = rule ? 'matched_signature' : 'unconfirmed';
  diagnostic.help = ['configuration', 'patch_target', 'runtime_arguments', 'permission', 'port_conflict'].includes(diagnostic.code) ? 'settings'
    : diagnostic.code === 'profile_restriction' ? 'profiles'
    : ['duplicate_entry', 'missing_bundle', 'module_api', 'missing_module', 'module_layout', 'package_manifest', 'plugin_activation', 'required_services'].includes(diagnostic.code) ? 'plugins'
    : 'logs';
  return diagnostic;
}

// Upstream renders every inactive entry as `<id> (<package>): <reason>` under a
// `... did not activate` header, in the host audit (app-boot inactiveEntries)
// and in the browser audit (client assertEntriesActive) alike. Parse that exact
// shape so a failure names its root causes instead of reprinting consequences:
// an entry waiting for a service is blocked by whoever should provide it.
export function parseActivation(text) {
  // app-boot startupDiagnostic uses grouped fatal output; pending rows carry
  // entry IDs only. Never manufacture package ownership for those consumers.
  let groupedTruncated = false;
  const grouped = /startup failed: \d+ required plugins? did not activate/i.exec(text || '');
  if (grouped) {
    const rows = [], lines = text.slice(grouped.index + grouped[0].length).split(/\r?\n/);
    groupedTruncated = lines.length > 400;
    let section, entry;
    const flush = () => {
      if (entry?.package) rows.push(`${entry.id} (${entry.package}): ${entry.detail.join(' ').slice(0, 400) || 'Plugin activation failed'}`);
      entry = undefined;
    };
    for (const raw of lines.slice(0, 400)) {
      if (/^Failed plugins \(\d+\):$/.test(raw.trim())) { flush(); section = 'failed'; continue; }
      if (/^Plugins waiting for services \(\d+\):$/.test(raw.trim())) { flush(); section = 'pending'; continue; }
      if (section === 'failed') {
        const title = /^  (\S+)(?: \(required\))?\s*$/.exec(raw);
        if (title) { flush(); entry = { id: title[1], detail: [] }; continue; }
        const pkg = /^    Package: (\S+)\s*$/.exec(raw);
        if (pkg && entry) { entry.package = pkg[1]; continue; }
        if (entry && /^    \S/.test(raw) && !/^\s+at /.test(raw)) entry.detail.push(raw.trim());
      } else if (section === 'pending') {
        const pending = /^  (\S+)(?: \(required\))? {2,}(\S.*)$/.exec(raw);
        if (pending && pending[1] !== 'Plugin') rows.push(`${pending[1]}: pending (waiting for services: ${pending[2].trim()})`);
      }
    }
    flush();
    if (!rows.length) return null;
    text = `dsh: required startup failure: ${rows.length} entries did not activate\n${rows.join('\n')}`;
  }
  const header = /(?:warning|required startup failure|web boot):\s*\d+\s+entr(?:y|ies) did not activate/i.exec(text || '');
  if (!header) return null;
  const entries = [];
  let truncated = groupedTruncated, scanned = 0;
  for (const raw of text.slice(header.index + header[0].length).split(/\r?\n/)) {
    const line = raw.trim();
    if (!line || line.startsWith('at ') || /did not activate/i.test(line)) continue;
    if (++scanned > 400) { truncated = true; break; }
    const row = /^(\S+?)(?:\s+\(([^)]+)\))?:\s*(\S.*)$/.exec(line);
    if (!row) continue;
    if (entries.length >= 48) { truncated = true; break; }
    const waiting = /^pending \(waiting for services?:\s*([^)]*)\)$/i.exec(row[3]);
    const missing = waiting
      ? [...new Set(waiting[1].split(',').map(part => part.trim()).filter(part => part && part !== 'unknown'))]
      : [];
    if (missing.length > 32) truncated = true;
    entries.push({ id: row[1].slice(0, 240), package: (row[2] || row[1]).slice(0, 240),
      state: waiting ? 'pending' : 'failed', reason: row[3].slice(0, 400),
      missing: missing.slice(0, 32).map(service => service.slice(0, 240)) });
  }
  if (!entries.length) return null;
  // The pending report is read back under a hard byte bound; never spend it here.
  while (JSON.stringify(entries).length > 8000 && entries.length > 1) { entries.pop(); truncated = true; }
  // Report entries once. A failed entry is evidence; a pending one is only a
  // consequence, so grouping by service stays a presentation concern.
  const counts = new Map();
  for (const entry of entries) for (const service of entry.missing) counts.set(service, (counts.get(service) ?? 0) + 1);
  return { entries, missing_services: [...counts.keys()].sort((a, b) => counts.get(b) - counts.get(a)), truncated };
}
// Bounded local inventory: no registry requests or extra runtime subprocesses.
// Follow dependency links once, including pnpm's store, without following cycles.
// If an inventory cannot be completed, run the probe without caching its result.
export async function verificationIdentity(roots, environment = process.env, desktopBuild) {
  const hash = crypto.createHash('sha256');
  const visited = new Set(), deadline = performance.now() + 15000;
  let excluded;
  try { excluded = desktopBuild && resolveExistingParent(desktopBuild); } catch { return null; }
  const records = new Map();
  const pending = roots.map(file => ({ file }));
  let count = 0;
  async function visit({ file, entry }) {
    if (file === excluded) return; // Generated Desktop assets are not Web inputs.
    if (++count > 400000 || performance.now() > deadline) throw Error('inventory budget');
    let real;
    try { real = entry && !entry.isSymbolicLink() ? file : await fs.promises.realpath(file); }
    catch (error) { if (error.code === 'ENOENT') { records.set(`missing:${file}`, 'missing'); return; } throw error; }
    if (!entry || entry.isSymbolicLink()) records.set(`link:${file}`, real);
    if (real === excluded) return;
    if (visited.has(real)) return;
    visited.add(real);
    const stat = entry?.isDirectory() ? entry : await fs.promises.stat(real, { bigint: true });
    if (stat.isDirectory()) {
      records.set(`node:${real}`, 'directory');
      for (const child of await fs.promises.readdir(real, { withFileTypes: true })) {
        if (child.name !== '.git') pending.push({ file: path.join(real, child.name), entry: child });
      }
    } else if (stat.isFile()) {
      records.set(`node:${real}`, [String(stat.size), String(stat.mtimeNs), String(stat.ctimeNs), String(stat.ino)]);
    } else throw Error('unsupported file');
  }
  try {
    // Bounded parallel metadata IO avoids serial Windows path lookup costs.
    // Sort canonical records, so scheduling and link visitation order cannot
    // change the identity. Retain file-change and symlink-target invalidation.
    while (pending.length) {
      const batch = await Promise.allSettled(pending.splice(-64).map(visit));
      if (batch.some(result => result.status === 'rejected')) return null;
    }
    for (const entry of [...records].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) hash.update(JSON.stringify(entry));
    hash.update(JSON.stringify(Object.entries(environment).sort(([a], [b]) => a.localeCompare(b))));
    hash.update(JSON.stringify([process.execPath, process.version, process.platform, process.arch]));
    return hash.digest('hex');
  } catch { return null; }
}
const marker = '.nexus-compatibility.json';
// Native resolution accepts Windows canonical (\\?\) paths without stripping
// their namespace or weakening the containment checks below.
const readJson = p => JSON.parse(fs.readFileSync(p, 'utf8'));
const within = (root, p) => { const rel = path.relative(root, p); return rel === '' || (!rel.startsWith('..' + path.sep) && rel !== '..' && !path.isAbsolute(rel)); };
// Windows temp roots may use an 8.3 alias while realpath returns the long name.
// Dangling generated links need the same identity without requiring their final
// target to exist. Resolve the nearest existing ancestor and retain the suffix.
function resolveExistingParent(file) {
  let current = path.resolve(file);
  const suffix = [];
  while (true) {
    try { return path.join(fs.realpathSync.native(current), ...suffix); }
    catch (error) {
      if (error.code !== 'ENOENT') throw error;
      const parent = path.dirname(current);
      if (parent === current) throw error;
      suffix.unshift(path.basename(current)); current = parent;
    }
  }
}
const validName = name => typeof name === 'string' && /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$/.test(name) && !name.endsWith('.');
const validPackage = name => /^(?:@[\w.-]+\/)?[\w.-]+$/.test(name) && !name.split('/').some(p => p === '.' || p === '..');
const profileFiles = ['package.json', 'pnpm-lock.yaml', 'cordis.patch.yml', 'pnpm-workspace.yaml'];
const homeFiles = ['settings.yaml', 'cordis.patch.yml', '.env'];
const retiredDirectory = '.nexus-retired-profiles';
const desktopProfileError = 'This Harness version reserves the "desktop" profile for its own Electron application. Select a regular profile such as "web", or keep using the previous Harness version. The original profile and its data are preserved; disabling plugins will not resolve this restriction.';
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
  slot = fs.realpathSync.native(slot);
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

export function dependencyOrigins(profile, packages, failureText = '') {
  const roots = profile.manifest.dsh.profile.bundles;
  return [...new Set(packages)].filter(name => name.length <= 214).slice(0, 4).map(target => {
    const chains = [], direct = new Set();
    let visited = 0, incomplete = false;
    const deadline = performance.now() + 250;
    function walk(name, chain, parent) {
      if (++visited > 1000 || performance.now() > deadline || chain.length > 6 || name.length > 214 || chains.length >= 4) { incomplete = true; return; }
      if (chain.includes(name)) return;
      if (name === target) { chains.push([...chain, name]); if (chain.length) direct.add(chain.at(-1)); return; }
      let directory = parent, manifest;
      while (within(profile.dir, directory)) {
        const file = path.join(directory, 'node_modules', name, 'package.json');
        try {
          if (fs.statSync(file).size > 256 * 1024) { incomplete = true; return; }
          manifest = readJson(file); directory = path.dirname(file); break;
        } catch (error) { if (error.code !== 'ENOENT') { incomplete = true; return; } }
        const next = path.dirname(directory); if (next === directory) break; directory = next;
      }
      if (!manifest) { incomplete = true; return; }
      for (const dependency of Object.keys({...manifest.dependencies, ...manifest.optionalDependencies, ...manifest.peerDependencies}).sort()) {
        if (validPackage(dependency)) walk(dependency, [...chain, name], directory);
      }
    }
    for (const root of roots) walk(root, [], profile.dir);
    return { package: target, chains, direct_dependents: [...direct], incomplete,
      loader_failures: [...loaderFailures(failureText, roots)].filter(name => name.length <= 214).slice(0, 8) };
  });
}

export function duplicateEntrySources(source, slot, text) {
  const id = /duplicate loader entry id: ([\w.-]+)/.exec(text)?.[1];
  if (!id) return [];
  const official = officialPackages(slot), matches = [];
  // Diagnostic evidence only: inspect declared bundle patches, never execute them.
  for (const name of source.manifest.dsh.profile.bundles.slice(0, 256)) {
    try {
      const directory = official.get(name) || path.join(source.dir, 'node_modules', name);
      const manifest = readJson(path.join(directory, 'package.json'));
      const patch = manifest.dsh?.bundle?.patch;
      if (typeof patch !== 'string') continue;
      const file = path.resolve(directory, patch);
      if (!within(directory, file) || fs.statSync(file).size > 256 * 1024) continue;
      const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
      const line = lines.findIndex(line => /^\s*-?\s*id:\s*['"]?([\w.-]+)['"]?\s*(?:#.*)?$/.exec(line)?.[1] === id);
      if (line >= 0) matches.push({package: name, id, file, line: line + 1});
    } catch { /* Missing evidence must not replace the original loader error. */ }
  }
  return matches.slice(0, 12);
}

// Only static adjacent id/name rows are evidence. Dynamic YAML is deliberately
// left unknown; this never evaluates a bundle or guesses providers from names.
export function replacedOfficialEntries(source, slot) {
  const official = officialPackages(slot), owners = new Map(), replacements = [];
  for (const bundle of source.manifest.dsh.profile.bundles.slice(0, 256)) {
    try {
      const directory = official.get(bundle) || path.join(source.dir, 'node_modules', bundle);
      const patch = readJson(path.join(directory, 'package.json')).dsh?.bundle?.patch;
      if (typeof patch !== 'string') continue;
      const file = path.resolve(directory, patch);
      if (!within(directory, file) || fs.statSync(file).size > 256 * 1024) continue;
      const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
      for (let i = 0; i + 1 < lines.length; i++) {
        const id = /^\s*-\s*id:\s*['"]?([\w.-]+)['"]?\s*(?:#.*)?$/.exec(lines[i])?.[1];
        const name = /^\s*name:\s*['"]?([@\w./-]+)['"]?\s*(?:#.*)?$/.exec(lines[i + 1])?.[1];
        if (!id || !name || id.length > 240 || name.length > 240) continue;
        const original = owners.get(id);
        if (original && original.package.startsWith('@deepseek-ai/') && !bundle.startsWith('@deepseek-ai/') && original.name !== name)
          replacements.push({ package: bundle, id, original: original.name, replacement: name });
        owners.set(id, { package: bundle, name });
      }
    } catch { /* Incomplete static evidence never becomes a repair decision. */ }
  }
  return replacements.slice(0, 16);
}

export function activationRepairCandidates(activation, bundles, replacements = []) {
  const candidates = new Map();
  if (activation?.entries.some(entry => entry.state === 'failed' && ['permission', 'port_conflict', 'storage_full', 'nexus_integration'].includes(diagnoseStartup(entry.reason).code))) return [];
  for (const entry of activation?.entries || []) {
    if (entry.state !== 'failed') continue;
    const bundle = bundles.filter(name => !name.startsWith('@deepseek-ai/') &&
      (entry.package === name || entry.package.startsWith(name + '/'))).sort((a, b) => b.length - a.length)[0];
    if (bundle) candidates.set(bundle, { package: bundle, reason: entry.reason, evidence: 'activation_failure' });
  }
  if (activation?.missing_services?.length) for (const row of replacements) {
    if (!candidates.has(row.package)) candidates.set(row.package, { package: row.package,
      reason: `Replaces built-in entry ${row.id}: ${row.original} -> ${row.replacement}`, evidence: 'replaces_official_entry' });
  }
  return [...candidates.values()].slice(0, 16);
}

function moduleFilter(source, official, planning = false, missing = new Set()) {
  const canonical = fs.realpathSync.native(source), started = Date.now(); let count = 0;
  return p => {
    if (++count > 150000 || (planning && Date.now() - started > 10000)) throw Error('Profile dependency scan exceeds file or time limit');
    const rel = path.relative(source, p).split(path.sep);
    if (rel[0] === '.bin' || rel[0] === '.pnpm') return false;
    const offset = rel.lastIndexOf('node_modules') + 1;
    const name = rel[offset]?.startsWith('@') ? rel.slice(offset, offset + 2).join('/') : rel[offset];
    if (official.has(name)) return false;
    // Harness generates fallback links for both official and third-party
    // dependencies. A dangling generated link contributes no loadable module;
    // leave it untouched and let the actual boot diagnose required packages.
    if (!fs.existsSync(p) && fs.lstatSync(p).isSymbolicLink()) {
      const target = path.resolve(path.dirname(p), fs.readlinkSync(p));
      const fallback = path.join(path.dirname(source), '.dsh-module-fallback', 'node_modules');
      if (within(resolveExistingParent(fallback), resolveExistingParent(target))) return false;
      if (validPackage(name)) missing.add(name);
      throw Error(`Installed dependency link is broken: ${p}. Repair the profile dependencies before retrying; the original link is preserved.`);
    }
    if (!within(canonical, fs.realpathSync.native(p))) throw Error('Dependency link escapes source node_modules');
    return true;
  };
}
export async function copyModules(source, target, official, missing) {
  if (!fs.existsSync(source)) return;
  const filter = moduleFilter(source, official, false, missing);
  const pending = [{ source, target }];
  // Isolated real copies, never hardlinks into user dependencies. Bound the IO
  // fan-out while retaining the same containment and package exclusion checks.
  while (pending.length) {
    const results = await Promise.allSettled(pending.splice(-16).map(async ({ source, target }) => {
      if (!filter(source)) return;
      const stat = await fs.promises.stat(source);
      if (stat.isDirectory()) {
        await fs.promises.mkdir(target, { recursive: true, mode: stat.mode });
        for (const name of await fs.promises.readdir(source)) pending.push({ source: path.join(source, name), target: path.join(target, name) });
      } else if (stat.isFile()) await fs.promises.copyFile(source, target);
      else throw Error('Unsupported dependency file');
    }));
    const failed = results.find(result => result.status === 'rejected');
    // All writes have settled before callers clean up a failed check.
    if (failed) throw failed.reason;
  }
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
  // New app-boot emits grouped activation diagnostics instead of loader wrappers.
  const failedSection = text.split(/Failed plugins \(\d+\):/)[1]?.split(/Plugins waiting for services/)[0];
  if (failedSection) for (const match of failedSection.matchAll(/^\s*Package:\s*(\S+)/gm)) {
    if (!match[1].startsWith('@deepseek-ai/') && bundles.includes(match[1])) found.add(match[1]);
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
  const bootSource = path.resolve(path.dirname(entry), '../../../packages/boot/app-boot/src/index.ts');
  let upstreamActivationPolicy = false;
  try {
    if (fs.statSync(bootSource).size <= 512 * 1024) {
      const source = fs.readFileSync(bootSource, 'utf8');
      upstreamActivationPolicy = source.includes('required') && source.includes('function startupDiagnostic(') && source.includes('function activationDiagnostic(');
    }
  } catch { /* Older releases retain the conservative loader-error gate. */ }
  const fatalLoaderOutput = () => /startup failed:|plugin tree failed to load:/.test(text)
    || (!upstreamActivationPolicy && /failed to (?:import|apply) loader entry/.test(text));
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
      if (exit !== null) return { ok: false, text, exitCode: exit };
      if (fatalLoaderOutput()) {
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
            if (Date.now() - readySince >= 1000 && !fatalLoaderOutput()) {
              const warning = /dsh: warning: \d+ entr(?:y|ies) did not activate/.exec(text);
              return { ok: true, text: '', warning: upstreamActivationPolicy && warning ? text.slice(warning.index, warning.index + 2400) : null };
            }
          }
        } catch { /* readiness may precede the listener */ }
      }
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw Object.assign(Error('Compatibility startup probe timed out; no plugins were guessed or disabled'), { probeOutput: text });
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
  const identityRoots = [slot, path.join(source.dir, 'node_modules'), path.join(home, 'node_modules'), process.execPath];
  const desktopBuild = path.join(slot, 'apps/desktop/.desktop-build');
  const identity = await verificationIdentity(identityRoots, process.env, desktopBuild);
  const fingerprint = crypto.createHash('sha256').update(JSON.stringify([checkerVersion, slot, release_id, source.source, source.fingerprint,
    node, identity, options.preferences_env ?? {}, options.preference_capabilities ?? null, options.builtin_fingerprint ?? null, declarations]));
  for (const file of patches) fingerprint.update(file).update(fs.readFileSync(file));
  const key = fingerprint.digest('hex');
  const cache = options.cache;
  let previous = [];
  if (cache && fs.existsSync(cache)) {
    if (fs.lstatSync(cache).isSymbolicLink()) throw Error('Invalid compatibility result cache');
    // Older checkers could write oversized advisory details. Ignore that cache
    // and regenerate; a disposable optimization must not prevent startup.
    let saved = {};
    try { if (fs.statSync(cache).size <= 512 * 1024) saved = readJson(cache); } catch { /* Disposable cache, retry the real check. */ }
    previous = [saved, ...(Array.isArray(saved.entries) ? saved.entries : [])]
      .filter(entry => entry?.report?.checker_version === checkerVersion && ['passed', 'isolated'].includes(entry.report.status))
      .map(({key, report}) => ({key, report})).slice(0, 8);
    const cached = !force && identity && previous.find(entry => entry.key === key);
    if (cached) {
      const report = { ...cached.report, last_trigger: trigger, last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: true };
      atomicJson(output, report); return report;
    }
  }
  // A failed forced recheck must never leave an older success reusable.
  previous = previous.filter(entry => entry.key !== key);
  if (cache) atomicJson(cache, { entries: previous });
  const scratch = path.join(work, crypto.randomUUID()), testHome = path.join(scratch, 'home');
  const candidate = path.join(testHome, 'profiles', source.source);
  fs.mkdirSync(candidate, { recursive: true });
  const disabled = source.manualDisabled.map(packageName => ({ package: packageName, reason: 'Disabled by user' }));
  let failureText = '';
  let failureStage = 'dependency_preparation';
  const missing = new Set();
  const report = status => boundDeclarationReport({ checker_version: checkerVersion, status, source_profile: source.source,
    effective_profile: source.source, release_id, fingerprint: source.fingerprint, checked_at_unix: Math.floor(Date.now() / 1000),
    disabled, declarations, dependency_origins: dependencyOrigins(source, missing, failureText), checked_disabled_plugins: source.manualDisabled, trigger, last_trigger: trigger,
    last_used_at_unix: Math.floor(Date.now() / 1000), cache_reused: false });
  try {
    const argumentSource = path.join(slot, 'apps/cli/src/args.ts');
    if (selected.toLowerCase() === 'desktop' && fs.existsSync(argumentSource)
      && fs.statSync(argumentSource).size <= 1024 * 1024
      && fs.readFileSync(argumentSource, 'utf8').includes('profile "desktop" is managed exclusively by the Electron application')) {
      throw Error(desktopProfileError);
    }
    const official = officialPackages(slot);
    await copyModules(path.join(source.dir, 'node_modules'), path.join(candidate, 'node_modules'), official, missing);
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
    failureStage = 'startup_probe';
    const result = await probe(node, path.join(slot, 'apps/cli/lib/bin.js'), testHome, source.source, timeout_ms, patches, options.owned_round === true);
    failureText = result.text;
    if (!result.ok && /failed to (?:import|apply) loader entry nexus-(?:desktop-compat|desktop-bridge|notifications)\b/.test(result.text)) {
      throw Error('A Nexus built-in plugin failed to load. Update or repair Nexus; do not disable third-party plugins. Original error: ' + result.text.slice(-4000));
    }
    if (!result.ok) throw Error('Harness startup check failed; original profile preserved. Original error: ' + (result.text.trim() ? result.text.slice(-4000) : `Harness process exited before readiness (exit code: ${result.exitCode ?? 'unknown'})`));
    if (sourceInfo(home, selected).fingerprint !== source.fingerprint) throw Error('Source profile changed during compatibility check');
    const activation = parseActivation(result.warning);
    const replacements = activation ? replacedOfficialEntries(source, slot) : [];
    const repairCandidates = activationRepairCandidates(activation, source.manifest.dsh.profile.bundles, replacements);
    if (activation?.entries.some(entry => entry.state === 'pending' && entry.package.startsWith('@deepseek-ai/'))) {
      const repair = boundDeclarationReport({ ...report(repairCandidates.length ? 'needs_choice' : 'failed'), failure_stage: 'plugin_loading',
        error: 'Plugin activation failed while Harness services were unavailable', candidates: repairCandidates,
        diagnosis: repairCandidates.length ? { level: 'blocking', certainty: 'activation_and_patch_evidence', code: 'plugin_activation', help: 'plugins',
          summary: 'Plugin failures or replacements prevent Harness services from becoming ready',
          remedy: 'Temporarily disable the recommended third-party plugins, then check and start again. Installed packages and data are retained.',
          activation, replacements, repair_candidates: repairCandidates, evidence: [] } : { ...diagnoseStartup(result.warning), activation } });
      atomicJson(output, repair);
      return repair;
    }
    if (sourceInfo(home, selected).fingerprint !== source.fingerprint) throw Error('Source profile changed during compatibility check');
    const passed = report(disabled.length ? 'isolated' : 'passed');
    if (result.warning) passed.diagnosis = {
      level: 'limited', certainty: 'upstream_optional_warning', code: 'optional_plugins', help: 'plugins',
      summary: 'Harness is ready; some optional plugins did not activate',
      remedy: 'You can continue using Harness. Inspect only the listed optional plugins if you need their features.',
      evidence: result.warning.split(/\r?\n/).filter(Boolean).slice(0, 12).map(line => line.slice(0, 200)),
      activation: parseActivation(result.warning),
    };
    // Store only under the pre-probe input identity. Every reuse computes a new
    // full identity before accepting this key; edits during/after the probe make
    // it unreachable. A second full scan here only discarded such stale keys,
    // and unnecessarily delayed the first real launch.
    if (cache && identity) {
      atomicJson(cache, { key, report: passed, entries: previous.slice(0, 7) });
    }
    atomicJson(output, passed); return passed;
  } catch (error) {
    if (typeof error.probeOutput === 'string') { failureText = error.probeOutput; error = Error(`${error.message}\n${failureText.slice(-4000)}`); }
    for (const match of String(error.message).matchAll(/Cannot find (?:package|module) ['"]((?:@[\w.-]+\/)?[\w.-]+)/g)) missing.add(match[1]);
    if (/profile "desktop" is managed exclusively by the Electron application/i.test(failureText)) {
      // The CLI rejected the profile before loading any plugins. Do not
      // publish plugin-isolation candidates for a reserved profile name.
      error = Error(desktopProfileError);
      failureText = '';
    }
    const failures = loaderFailures(failureText, source.manifest.dsh.profile.bundles);
    const duplicates = duplicateEntrySources(source, slot, failureText);
    if (duplicates.length) {
      error = Error(`Duplicate loader entry id: ${duplicates[0].id}\nDeclaration sources:\n${duplicates.map(row => `${row.package} (${row.file}:${row.line})`).join('\n')}\n\n${error.message}`);
    }
    if (String(error.message).includes(desktopProfileError)) failureStage = 'profile_restriction';
    else if (/Compatibility (?:startup )?probe timed out/i.test(String(error.message))) failureStage = 'readiness_timeout';
    else if (failures.size || duplicates.length) failureStage = 'plugin_loading';
    const diagnosis = diagnoseStartup(String(error.message));
    // Name the entries the audit actually reported, not a wall of consequences.
    const activation = parseActivation(failureText) || parseActivation(String(error.message));
    if (activation) diagnosis.activation = activation;
    if (activation?.entries.some(entry => entry.state === 'failed' && /^nexus-(?:desktop-compat|desktop-bridge|notifications)$/.test(entry.id))) {
      Object.assign(diagnosis, diagnoseStartup('A Nexus built-in plugin failed to load'));
    }
    const replacements = activation ? replacedOfficialEntries(source, slot) : [];
    const repairCandidates = ['nexus_integration', 'storage_full', 'permission', 'port_conflict', 'cleanup_timeout'].includes(diagnosis.code) ? [] : activationRepairCandidates(activation, source.manifest.dsh.profile.bundles, replacements);
    if (repairCandidates.length) {
      Object.assign(diagnosis, { code: 'plugin_activation', help: 'plugins', certainty: 'activation_and_patch_evidence',
        summary: 'Plugin failures or replacements prevent Harness services from becoming ready',
        remedy: 'Temporarily disable the recommended third-party plugins, then check and start again. Installed packages and data are retained.',
        replacements, repair_candidates: repairCandidates });
      failureStage = 'plugin_loading';
    }
    const pluginChoice = ['duplicate_entry', 'plugin_activation'].includes(diagnosis.code);
    if (pluginChoice) error = Error(String(error.message).replace('Harness startup check failed; original profile preserved. Original error:', 'Startup check needs an explicit plugin decision; original profile preserved. Original error:'));
    if (!pluginChoice) error = Error(String(error.message).replace(
      'Startup check needs an explicit plugin decision; original profile preserved. Original error:',
      'Harness startup check failed; original profile preserved. Original error:'));
    atomicJson(output, boundDeclarationReport({ ...report(pluginChoice ? 'needs_choice' : 'failed'), diagnosis, failure_stage: failureStage, error: String(error.message).slice(0, 4600),
      candidates: (pluginChoice ? source.manifest.dsh.profile.bundles : []).filter(p => !p.startsWith('@deepseek-ai/')).map(packageName => ({ package: packageName,
        reason: repairCandidates.find(row => row.package === packageName)?.reason || (duplicates.some(row => row.package === packageName) ? 'Declares the duplicate loader entry ID' : failures.has(packageName) ? 'DSH reported a loader error for this plugin' : 'Not identified as faulty; optional isolation for troubleshooting') })) }));
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
      await copyModules(path.join(source.dir, 'node_modules'), path.join(candidate, 'node_modules'), official);
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
