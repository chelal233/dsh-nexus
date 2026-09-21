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
    ['configuration', /failed to (?:read|parse) (?:profile manifest|config|patches|overlay)|must (?:be a top-level YAML array|hold a JSON object)|must be a mapping|YAMLException|JSONParseError|Desktop profile (?:manifest|JSON|bundles) (?:is|are) invalid/i, 'Invalid or unreadable configuration', 'Repair the named configuration or patch file; preserve a backup before editing.'],
    ['missing_bundle', /cannot resolve profile bundle ["']([^"']+)|profile bundle .+ declares no dsh\.bundle/i, 'Profile bundle is missing or invalid', 'Repair the selected profile dependencies or select a package that declares a Harness bundle.'],
    ['module_api', /cannot resolve ESM export|ERR_PACKAGE_PATH_NOT_EXPORTED|does not provide an export named|export .+ resolves outside its package|unsupported Node module loader|typert: .+ strict codec has no create\(\) factory/i, 'Package or runtime interface is incompatible', 'Use compatible plugin, Harness and Node versions; reinstalling the same incompatible version may not help.'],
    ['missing_module', /Cannot find (?:package|module)|ERR_MODULE_NOT_FOUND|MODULE_NOT_FOUND|main entry is missing|Installed dependency link is broken/i, 'Required module is missing', 'Use the importer path to locate the missing dependency in the Harness installation or profile. Check local package files and links before reinstalling; do not disable waiting plugins.'],
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
