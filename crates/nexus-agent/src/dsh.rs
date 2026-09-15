//! Native DSH home/profile resolution and owned dependency materialization.

use std::{
    collections::{BTreeMap, HashSet},
    env,
    ffi::OsString,
    fs,
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use nexus_core::{
    build_pnpm_args, build_runtime_child_env, resolve_runtime_command, validate_profile_name,
    ConfigStore, NexusPaths,
};
use nexus_protocol::{NativeProfilePayload, ProfilePluginPayload};

pub(crate) const DSH_HOME_ENV: &str = "DSH_HOME";
pub(crate) const DEFAULT_MATERIALIZATION_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_PROFILE_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PLUGIN_OUTPUT_BYTES: usize = 64 * 1024;
static PLUGIN_RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct PluginCommandSpec {
    program: PathBuf,
    args: Vec<OsString>,
    current_dir: PathBuf,
    env: BTreeMap<OsString, OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginCommandOutcome {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

trait PluginCommandRunner {
    fn run(&self, paths: &NexusPaths, spec: &PluginCommandSpec)
        -> io::Result<PluginCommandOutcome>;
}

struct SystemPluginCommandRunner;

pub(crate) fn resolve_dsh_home() -> io::Result<PathBuf> {
    if let Some(value) = env::var_os(DSH_HOME_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DSH_HOME must be an absolute path",
            ));
        }
        return Ok(path);
    }
    let native_home = if cfg!(windows) {
        env::var_os("USERPROFILE")
    } else {
        env::var_os("HOME")
    }
    .filter(|value| !value.is_empty())
    .map(PathBuf::from)
    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "native home is unavailable"))?;
    if !native_home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "native home must be an absolute path",
        ));
    }
    Ok(native_home.join(".dsh"))
}

pub(crate) fn resolve_dsh_home_for_paths(paths: &NexusPaths) -> io::Result<PathBuf> {
    match nexus_core::load_harness_preferences(paths)?.home {
        Some(home) => Ok(PathBuf::from(home)),
        None => resolve_dsh_home(),
    }
}

pub(crate) fn canonical_dsh_home(path: &Path) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH home must be an existing ordinary directory",
        ));
    }
    fs::canonicalize(path)
}

pub(crate) fn same_native_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        fn comparable(path: &Path) -> String {
            let mut value = path.to_string_lossy().replace('/', "\\");
            if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
                value = format!(r"\\{unc}");
            } else if let Some(dos) = value.strip_prefix(r"\\?\") {
                value = dos.to_owned();
            }
            value.to_ascii_lowercase()
        }
        comparable(left) == comparable(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

pub(crate) fn profile_directory(dsh_home: &Path, profile: &str) -> io::Result<PathBuf> {
    validate_profile_name(profile)?;
    let home = canonical_dsh_home(dsh_home)?;
    let profiles = home.join("profiles");
    let profile_dir = profiles.join(profile);
    let metadata = fs::symlink_metadata(&profile_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DSH profile must be an existing ordinary directory",
        ));
    }
    let canonical = fs::canonicalize(&profile_dir)?;
    if !canonical.starts_with(&profiles) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "DSH profile resolves outside DSH_HOME/profiles",
        ));
    }
    Ok(canonical)
}

/// Locate the official built CLI entry without invoking Corepack or a shell.
#[allow(dead_code)] // Shared now so the subsequent plugin-control phase cannot diverge.
pub(crate) fn locate_built_cli(release_root: &Path) -> io::Result<PathBuf> {
    let release_root = fs::canonicalize(release_root)?;
    let manifest = release_root.join("apps/cli/package.json");
    let metadata = fs::symlink_metadata(&manifest)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PROFILE_MANIFEST_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "built DSH CLI package manifest is not a bounded ordinary file",
        ));
    }
    let manifest = fs::canonicalize(manifest)?;
    if !manifest.starts_with(&release_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "built DSH CLI package manifest resolves outside its release root",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(manifest)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let bin_matches = match value.get("bin") {
        Some(serde_json::Value::String(path)) => path == "lib/bin.js",
        Some(serde_json::Value::Object(entries)) => entries
            .values()
            .any(|value| value.as_str() == Some("lib/bin.js")),
        _ => false,
    };
    if !bin_matches {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "apps/cli package bin does not identify lib/bin.js",
        ));
    }
    let candidate = release_root.join("apps/cli/lib/bin.js");
    let metadata = fs::symlink_metadata(&candidate)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "built DSH CLI apps/cli/lib/bin.js is not an ordinary file",
        ));
    }
    let candidate = fs::canonicalize(candidate)?;
    if !candidate.starts_with(&release_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "built DSH CLI resolves outside its release root",
        ));
    }
    Ok(candidate)
}

pub(crate) fn profile_is_initialized(dsh_home: &Path, profile: &str) -> io::Result<bool> {
    let profile_dir = match profile_directory(dsh_home, profile) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let package = profile_dir.join("package.json");
    let metadata = match fs::symlink_metadata(&package) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Ok(false);
    }
    let bytes = fs::read(package)?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(value.is_object())
}

pub(crate) fn native_profiles(dsh_home: &Path) -> io::Result<Vec<NativeProfilePayload>> {
    let home = match canonical_dsh_home(dsh_home) {
        Ok(home) => home,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let profiles_dir = home.join("profiles");
    let metadata = match fs::symlink_metadata(&profiles_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DSH profiles directory must be an ordinary directory",
        ));
    }
    let mut profiles = Vec::new();
    for entry in fs::read_dir(&profiles_dir)? {
        let entry = entry?;
        if profiles.len() >= 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DSH profile inventory exceeds 256 entries",
            ));
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if validate_profile_name(&name).is_err() {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        if let Ok(profile) = native_profile(&home, &name) {
            profiles.push(profile);
        }
    }
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(profiles)
}

pub(crate) fn native_profile(dsh_home: &Path, profile: &str) -> io::Result<NativeProfilePayload> {
    let profile_dir = profile_directory(dsh_home, profile)?;
    let package_path = profile_dir.join("package.json");
    let metadata = fs::symlink_metadata(&package_path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PROFILE_MANIFEST_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "profile package.json must be an ordinary file no larger than 1 MiB",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(package_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let bundles_value = value
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "profile manifest has no dsh.profile.bundles array",
            )
        })?;
    let mut bundles = Vec::with_capacity(bundles_value.len());
    let mut seen = HashSet::new();
    for value in bundles_value {
        let bundle = value
            .as_str()
            .filter(|value| valid_package_name(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "profile bundle is not a valid package name",
                )
            })?;
        if !seen.insert(bundle.to_owned()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "profile bundles contain a duplicate",
            ));
        }
        bundles.push(bundle.to_owned());
    }
    let empty_dependencies = serde_json::Map::new();
    let dependencies = match value.get("dependencies") {
        None => &empty_dependencies,
        Some(value) => value.as_object().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "profile manifest dependencies must be an object",
            )
        })?,
    };
    let mut versions = BTreeMap::new();
    for (package, version) in dependencies {
        if !valid_package_name(package) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "profile dependency has an invalid package name",
            ));
        }
        let version = version.as_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "profile dependency version must be text",
            )
        })?;
        versions.insert(package.clone(), version.to_owned());
    }
    let mut plugins = Vec::new();
    for bundle in &bundles {
        let version = versions.remove(bundle);
        let builtin = fixed_bundle(bundle) || version.is_none();
        plugins.push(ProfilePluginPayload {
            package: bundle.clone(),
            version,
            builtin,
            removable: !builtin,
        });
    }
    plugins.extend(
        versions
            .into_iter()
            .map(|(package, version)| {
                let builtin = fixed_bundle(&package);
                ProfilePluginPayload { package, version: Some(version), builtin, removable: !builtin }
            }),
    );
    Ok(NativeProfilePayload {
        name: profile.to_owned(),
        order_undo_id: None,
        source_profile: generated_source_profile(&profile_dir, profile)?,
        bundles,
        plugins,
    })
}

const FIXED_BUNDLES: [&str; 2] = ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"];

fn fixed_bundle(package: &str) -> bool { FIXED_BUNDLES.contains(&package) }

fn generated_source_profile(directory: &Path, profile: &str) -> io::Result<Option<String>> {
    let marker = directory.join(".nexus-compatibility.json");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid compatibility profile marker"));
    }
    let report: nexus_protocol::CompatibilityReport = serde_json::from_slice(&fs::read(marker)?).map_err(io::Error::other)?;
    validate_profile_name(&report.source_profile)?;
    if report.effective_profile != profile || report.source_profile == profile
        || !matches!(report.status.as_str(), "passed" | "isolated") {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Compatibility profile marker does not match this profile"));
    }
    Ok(Some(report.source_profile))
}

pub(crate) fn move_profile_plugin(paths: &NexusPaths, home: &Path, profile: &str, package: &str, target: Option<&str>) -> io::Result<NativeProfilePayload> {
    let directory = profile_directory(home, profile)?;
    let manifest = directory.join("package.json");
    let inventory = native_profile(home, profile)?;
    let before = nexus_core::read_regular_file_bounded(&manifest, MAX_PROFILE_MANIFEST_BYTES)?
        .ok_or_else(|| io::Error::other("Profile manifest is missing"))?;
    if let Some(source) = &inventory.source_profile {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,
            format!("Generated compatibility profiles are read-only; reorder source profile {source}")));
    }
    if fixed_bundle(package) || !inventory.bundles.iter().any(|bundle| bundle == package) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Only non-fixed loaded bundles can be moved"));
    }
    if target.is_some_and(|target| fixed_bundle(target) || target == package || !inventory.bundles.iter().any(|bundle| bundle == target)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Move target must be another non-fixed loaded bundle"));
    }
    let mut ordered: Vec<String> = FIXED_BUNDLES.iter().filter(|fixed| inventory.bundles.iter().any(|bundle| bundle == **fixed))
        .map(|fixed| (*fixed).to_owned()).collect();
    ordered.extend(inventory.bundles.iter().filter(|bundle| !fixed_bundle(bundle) && *bundle != package).cloned());
    let position = target.and_then(|target| ordered.iter().position(|bundle| bundle == target)).unwrap_or(ordered.len());
    ordered.insert(position, package.to_owned());
    let mut value: serde_json::Value = serde_json::from_slice(&before).map_err(io::Error::other)?;
    if value.pointer("/dsh/profile/bundles") != Some(&serde_json::json!(inventory.bundles)) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "Profile bundles changed during reorder; refresh and retry"));
    }
    *value.pointer_mut("/dsh/profile/bundles").ok_or_else(|| io::Error::other("Profile bundles disappeared"))? = serde_json::json!(ordered);
    if nexus_core::read_regular_file_bounded(&manifest,MAX_PROFILE_MANIFEST_BYTES)?.as_deref() != Some(before.as_slice()) {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "Profile manifest changed during reorder; refresh and retry"));
    }
    let after = nexus_protocol::encode_json(&value).map_err(io::Error::other)?;
    if after.len() as u64 > MAX_PROFILE_MANIFEST_BYTES { return Err(io::Error::other("Profile manifest is too large")); }
    if before == after { return native_profile(home, profile); }
    let record = OrderUndo { schema:1, profile:profile.into(), home:directory_identity(home)?, directory:directory_identity(&directory)?, before, after };
    let id=format!("order-{}", nexus_core::unix_time_nanos_for_update());
    let record_path=order_record_path(paths,profile,&id,true)?;
    let temp=record_path.with_extension("tmp");
    nexus_core::write_private_bytes_atomic(record_path.parent().unwrap(),&temp,&serde_json::to_vec(&record).map_err(io::Error::other)?)?;
    let published=fs::hard_link(&temp,&record_path);let _=fs::remove_file(&temp);published?;
    if nexus_core::read_regular_file_bounded(&manifest,MAX_PROFILE_MANIFEST_BYTES)?.as_deref()!=Some(record.before.as_slice()) {
        let _=fs::remove_file(&record_path);
        return Err(io::Error::new(io::ErrorKind::WouldBlock,"Profile manifest changed during reorder"));
    }
    if let Err(error)=nexus_core::write_private_bytes_atomic(&directory,&manifest,&record.after) {
        if nexus_core::read_regular_file_bounded(&manifest,MAX_PROFILE_MANIFEST_BYTES).ok().flatten().as_deref()==Some(record.before.as_slice()) { let _=fs::remove_file(&record_path); }
        return Err(error);
    }
    // Keep the newest successful undo; a failed new operation never touches it.
    for old in order_records(paths,profile)? {
        if old!=record_path {
            if let Ok(saved)=read_order_record(&old) {
                if saved.home==record.home && saved.directory==record.directory { let _=fs::remove_file(old); }
            }
        }
    }
    let mut inventory=native_profile(home,profile)?;inventory.order_undo_id=Some(id);Ok(inventory)
}

#[derive(serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderUndo { schema:u32, profile:String, home:String, directory:String, before:Vec<u8>, after:Vec<u8> }
fn directory_identity(path:&Path)->io::Result<String> { nexus_core::data_root_identity(&NexusPaths::from_root(path.to_path_buf())) }
fn order_record_path(paths:&NexusPaths,profile:&str,id:&str,create:bool)->io::Result<PathBuf> {
    validate_profile_name(profile)?;
    if !id.starts_with("order-") || id.len()>80 || !id[6..].bytes().all(|v|v.is_ascii_digit()) {return Err(io::Error::other("Invalid order operation ID"));}
    if create { paths.ensure_directories()?; }
    let root=paths.root.join("plugin-order-undo");let directory=root.join(profile);
    for path in [&root,&directory] {
        match fs::symlink_metadata(path) {
            Ok(meta) if !meta.is_dir() || nexus_core::path_is_reparse(&meta)=>return Err(io::Error::other("Unsafe order undo directory")),
            Ok(_)=>{}, Err(error) if error.kind()==io::ErrorKind::NotFound && create=>fs::create_dir(path)?,
            Err(error) if error.kind()==io::ErrorKind::NotFound=>{}, Err(error)=>return Err(error),
        }
    }
    Ok(directory.join(format!("{id}.json")))
}
fn order_records(paths:&NexusPaths,profile:&str)->io::Result<Vec<PathBuf>> {
    let probe=order_record_path(paths,profile,"order-0",false)?;
    let entries=match fs::read_dir(probe.parent().unwrap()) {Ok(v)=>v,Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(Vec::new()),Err(e)=>return Err(e)};
    let mut result=Vec::new();
    for entry in entries {let entry=entry?;let name=entry.file_name();let Some(name)=name.to_str() else {continue;};
        if let Some(id)=name.strip_suffix(".json") {if id.starts_with("order-") && id[6..].bytes().all(|v|v.is_ascii_digit()) {result.push(entry.path());}}
        if result.len()>128 {return Err(io::Error::other("Too many order recovery records; preserve them and export diagnostics"));}
    }
    result.sort();result.reverse();Ok(result)
}
fn read_order_record(path:&Path)->io::Result<OrderUndo> {
    let bytes=nexus_core::read_regular_file_bounded(path,10*1024*1024)?.ok_or_else(||io::Error::other("Order undo record is missing"))?;
    let record:OrderUndo=serde_json::from_slice(&bytes).map_err(|_|io::Error::other("Invalid order undo record"))?;
    if record.schema!=1 || record.before.len() as u64>MAX_PROFILE_MANIFEST_BYTES || record.after.len() as u64>MAX_PROFILE_MANIFEST_BYTES {return Err(io::Error::other("Invalid order undo bounds"));}
    Ok(record)
}
pub(crate) fn order_undo_id(paths:&NexusPaths,home:&Path,profile:&str)->io::Result<Option<String>> {
    let directory=profile_directory(home,profile)?;
    let current=nexus_core::read_regular_file_bounded(&directory.join("package.json"),MAX_PROFILE_MANIFEST_BYTES)?;
    let home_id=directory_identity(home)?;let directory_id=directory_identity(&directory)?;
    for path in order_records(paths,profile)? {
        let saved=read_order_record(&path)?;
        if saved.profile==profile && saved.home==home_id && saved.directory==directory_id && current.as_deref()==Some(saved.after.as_slice()) {
            return Ok(path.file_stem().and_then(|v|v.to_str()).map(str::to_owned));
        }
    }
    Ok(None)
}
pub(crate) fn undo_profile_order(paths:&NexusPaths,home:&Path,profile:&str,id:&str)->io::Result<NativeProfilePayload> {
    let directory=profile_directory(home,profile)?;let manifest=directory.join("package.json");
    if native_profile(home,profile)?.source_profile.is_some() {return Err(io::Error::other("Generated profiles are read-only"));}
    let path=order_record_path(paths,profile,id,false)?;let saved=read_order_record(&path)?;
    if saved.profile!=profile || saved.home!=directory_identity(home)? || saved.directory!=directory_identity(&directory)? {return Err(io::Error::other("Order undo belongs to a different profile location"));}
    let current=nexus_core::read_regular_file_bounded(&manifest,MAX_PROFILE_MANIFEST_BYTES)?.ok_or_else(||io::Error::other("Profile manifest is missing"))?;
    if current!=saved.after {return Err(io::Error::new(io::ErrorKind::WouldBlock,"Profile manifest changed after reorder; undo will not overwrite it"));}
    let _:serde_json::Value=serde_json::from_slice(&saved.before).map_err(|_|io::Error::other("Invalid original profile manifest"))?;
    nexus_core::write_private_bytes_atomic(&directory,&manifest,&saved.before)?;
    let _=fs::remove_file(path);
    native_profile(home,profile)
}

fn valid_package_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 214
        && !value.chars().any(char::is_whitespace)
        && !value.chars().any(char::is_control)
        && !value.contains(['\\', ':'])
        && !value.starts_with('.')
        && !value.ends_with('/')
        && match value.strip_prefix('@') {
            Some(scoped) => scoped.split_once('/').is_some_and(|(scope, name)| {
                !scope.is_empty() && !name.is_empty() && !name.contains('/')
            }),
            None => !value.contains('/'),
        }
}

pub(crate) fn remove_profile_plugin(
    paths: &NexusPaths,
    dsh_home: &Path,
    release_root: &Path,
    profile: &str,
    package: &str,
) -> io::Result<(PluginCommandOutcome, NativeProfilePayload)> {
    remove_profile_plugin_with_runner(
        paths,
        dsh_home,
        release_root,
        profile,
        package,
        &SystemPluginCommandRunner,
    )
}

fn remove_profile_plugin_with_runner(
    paths: &NexusPaths,
    dsh_home: &Path,
    release_root: &Path,
    profile: &str,
    package: &str,
    runner: &dyn PluginCommandRunner,
) -> io::Result<(PluginCommandOutcome, NativeProfilePayload)> {
    let inventory = native_profile(dsh_home, profile)?;
    if !inventory
        .plugins
        .iter()
        .any(|item| item.package == package && item.removable)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "package is not a removable dependency of the selected profile",
        ));
    }
    let profile_dir = profile_directory(dsh_home, profile)?;
    let config = ConfigStore::new(paths.clone()).load()?;
    let runtime = config
        .runtime
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "runtime is not configured"))?;
    let node = resolve_runtime_command(&runtime, "node")?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "pinned Node runtime is unavailable",
        )
    })?;
    let cli = locate_built_cli(release_root)?;
    let mut args = node.prefix_args;
    args.extend([
        nexus_core::node_script_argument(&cli),
        OsString::from("plugin"),
        OsString::from("--profile"),
        OsString::from(profile),
        OsString::from("remove"),
        OsString::from(package),
    ]);
    let mut child_env: BTreeMap<OsString, OsString> =
        build_runtime_child_env(&runtime, env::var_os("PATH").as_deref())?
            .into_iter()
            .collect();
    let preferences = nexus_core::load_harness_preferences(paths)?;
    // Plugin removal must remain usable to repair a profile even when launch
    // overrides are incompatible. It needs only the selected data directory;
    // startup settings are validated on preflight/start, never sent to this command.
    let command_preferences = nexus_protocol::HarnessPreferencesPayload { home: preferences.home, ..Default::default() };
    child_env.extend(nexus_core::harness_preferences_environment(&command_preferences, &nexus_core::HarnessProfileCapabilities::default()));
    child_env.insert(
        OsString::from(DSH_HOME_ENV),
        dsh_home.as_os_str().to_owned(),
    );
    let outcome = runner.run(
        paths,
        &PluginCommandSpec {
            program: node.program,
            args,
            current_dir: profile_dir,
            env: child_env,
        },
    )?;
    let refreshed = native_profile(dsh_home, profile)?;
    Ok((outcome, refreshed))
}

impl PluginCommandRunner for SystemPluginCommandRunner {
    fn run(
        &self,
        paths: &NexusPaths,
        spec: &PluginCommandSpec,
    ) -> io::Result<PluginCommandOutcome> {
        fs::create_dir_all(&paths.run_dir)?;
        let sequence = PLUGIN_RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let stdout_path = paths.run_dir.join(format!(
            "plugin-remove-{}-{sequence}.stdout.tmp",
            std::process::id()
        ));
        let stderr_path = paths.run_dir.join(format!(
            "plugin-remove-{}-{sequence}.stderr.tmp",
            std::process::id()
        ));
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stdout_path)?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr_path)?;
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.current_dir)
            .envs(spec.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        let result = run_owned_process(&mut command, DEFAULT_MATERIALIZATION_TIMEOUT);
        let stdout = read_output_bounded(&stdout_path);
        let stderr = read_output_bounded(&stderr_path);
        let _ = fs::remove_file(&stdout_path);
        let _ = fs::remove_file(&stderr_path);
        let status = result?;
        Ok(PluginCommandOutcome {
            exit_code: status.code(),
            stdout: stdout?,
            stderr: stderr?,
        })
    }
}

fn read_output_bounded(path: &Path) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_PLUGIN_OUTPUT_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_PLUGIN_OUTPUT_BYTES;
    let bytes = &bytes[..bytes.len().min(MAX_PLUGIN_OUTPUT_BYTES)];
    let mut value = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        value.push_str("\n[output truncated by Nexus]");
    }
    Ok(value)
}

pub(crate) fn materialize_profile(
    paths: &NexusPaths,
    dsh_home: &Path,
    profile: &str,
) -> io::Result<()> {
    materialize_profile_with_timeout(paths, dsh_home, profile, DEFAULT_MATERIALIZATION_TIMEOUT)
}

fn materialize_profile_with_timeout(
    paths: &NexusPaths,
    dsh_home: &Path,
    profile: &str,
    timeout: Duration,
) -> io::Result<()> {
    let profile_dir = profile_directory(dsh_home, profile)?;
    let config = ConfigStore::new(paths.clone()).load()?;
    let runtime = config.runtime.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "runtime is not configured; dependency materialization remains pending",
        )
    })?;
    let command = resolve_runtime_command(&runtime, "pnpm")?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "pinned pnpm is unavailable; dependency materialization remains pending",
        )
    })?;
    let mut args = command.prefix_args;
    args.extend(build_pnpm_args(
        &runtime,
        ["install".into(), "--frozen-lockfile".into()],
    ));
    let child_env = build_runtime_child_env(&runtime, env::var_os("PATH").as_deref())?;
    let mut process = Command::new(command.program);
    process
        .args(args)
        .current_dir(profile_dir)
        .env(DSH_HOME_ENV, dsh_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in child_env {
        process.env(key, value);
    }
    let status = run_owned_process(&mut process, timeout)?;
    if !status.success() {
        return Err(io::Error::other(match status.code() {
            Some(code) => format!("pnpm materialization exited with code {code}"),
            None => "pnpm materialization exited without a code".to_owned(),
        }));
    }
    Ok(())
}

fn run_owned_process(command: &mut Command, timeout: Duration) -> io::Result<ExitStatus> {
    run_owned_process_inner(command, timeout, ProcessTreeFault::None)
}

#[derive(Clone, Copy)]
enum ProcessTreeFault {
    None,
    #[cfg(test)]
    BeforeJobCreate,
    #[cfg(test)]
    BeforeAssign,
    #[cfg(test)]
    BeforeResume,
}

fn run_owned_process_inner(
    command: &mut Command,
    timeout: Duration,
    fault: ProcessTreeFault,
) -> io::Result<ExitStatus> {
    run_owned_process_cancelled(command, timeout, fault, || false)
}

pub(crate) fn run_cold_process(
    command: &mut Command,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> io::Result<ExitStatus> {
    run_owned_process_cancelled(command, timeout, ProcessTreeFault::None, cancelled)
}

#[derive(Debug)]
struct OwnedProcessCleanupError(String);

impl std::fmt::Display for OwnedProcessCleanupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OwnedProcessCleanupError {}

fn owned_process_cleanup_error(message: String) -> io::Error {
    io::Error::other(OwnedProcessCleanupError(message))
}

pub(crate) fn cold_process_owner_quiescent(error: &io::Error) -> bool {
    !error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<OwnedProcessCleanupError>().is_some())
}

fn run_owned_process_cancelled(
    command: &mut Command,
    timeout: Duration,
    fault: ProcessTreeFault,
    cancelled: impl Fn() -> bool,
) -> io::Result<ExitStatus> {
    configure_owned_process(command);
    let job_name = command.get_envs().find(|(key, _)| *key == "NEXUS_OWNED_JOB_NAME")
        .and_then(|(_, value)| value).map(|value| value.to_string_lossy().into_owned());
    command.env_remove("NEXUS_OWNED_JOB_NAME");
    let child = command.spawn()?;
    let mut tree = OwnedProcessTree::new_named(child, fault, job_name.as_deref())?;
    if let Err(error) = tree.resume(fault) {
        return tree.cleanup_failure(error);
    }
    let deadline = Instant::now() + timeout;
    loop {
        match tree.try_wait() {
            Ok(Some(status)) => {
                tree.wait_for_tree_exit(Duration::from_secs(5))
                    .map_err(|error| {
                        owned_process_cleanup_error(format!(
                            "owned process cleanup failed: {error}"
                        ))
                    })?;
                return Ok(status);
            }
            Ok(None) => {}
            Err(error) => return tree.cleanup_failure(error),
        }
        if cancelled() {
            tree.terminate_and_wait(Duration::from_secs(10))
                .map_err(|error| {
                    owned_process_cleanup_error(format!("owned process cleanup failed: {error}"))
                })?;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cold install cancelled",
            ));
        }
        if Instant::now() >= deadline {
            tree.terminate_and_wait(Duration::from_secs(10))
                .map_err(|error| {
                    owned_process_cleanup_error(format!("owned process cleanup failed: {error}"))
                })?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "dependency materialization timed out after {}s",
                    timeout.as_secs()
                ),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn configure_owned_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }
}

struct OwnedProcessTree {
    child: Child,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl OwnedProcessTree {
    fn new_named(mut child: Child, fault: ProcessTreeFault, job_name: Option<&str>) -> io::Result<Self> {
        #[cfg(windows)]
        {
            #[cfg(not(test))]
            let _ = fault;
            #[cfg(test)]
            if matches!(fault, ProcessTreeFault::BeforeJobCreate) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("injected failure before job creation"));
            }
            let job = match create_named_kill_on_close_job(job_name) {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            #[cfg(test)]
            if matches!(fault, ProcessTreeFault::BeforeAssign) {
                let _ = child.kill();
                let _ = child.wait();
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                return Err(io::Error::other("injected failure before job assignment"));
            }
            if let Err(error) = assign_child_to_job(&child, job) {
                let _ = child.kill();
                let _ = child.wait();
                unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                return Err(error);
            }
            return Ok(Self { child, job });
        }
        #[cfg(not(windows))]
        {
            let _ = fault;
            Ok(Self { child })
        }
    }

    fn resume(&mut self, fault: ProcessTreeFault) -> io::Result<()> {
        #[cfg(windows)]
        {
            #[cfg(not(test))]
            let _ = fault;
            #[cfg(test)]
            if matches!(fault, ProcessTreeFault::BeforeResume) {
                return Err(io::Error::other("injected failure before process resume"));
            }
            resume_process_primary_thread(self.child.id())
        }
        #[cfg(not(windows))]
        {
            let _ = fault;
            Ok(())
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn terminate_and_wait(&mut self, timeout: Duration) -> io::Result<()> {
        terminate_tree(self)?;
        let deadline = Instant::now() + timeout;
        loop {
            let _ = self.child.try_wait()?;
            if tree_is_empty(self)? {
                let _ = self.child.wait();
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned materialization process tree did not terminate",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn cleanup_failure<T>(&mut self, primary: io::Error) -> io::Result<T> {
        match self.terminate_and_wait(Duration::from_secs(10)) {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(io::Error::new(
                primary.kind(),
                OwnedProcessCleanupError(format!(
                    "{primary}; owned process cleanup also failed: {cleanup}"
                )),
            )),
        }
    }

    fn wait_for_tree_exit(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let empty = match tree_is_empty(self) {
                Ok(empty) => empty,
                Err(error) => return self.cleanup_failure(error),
            };
            if empty {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.terminate_and_wait(Duration::from_secs(10))?;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let _ = windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
            let _ = self.child.try_wait();
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
        #[cfg(unix)]
        {
            let _ = terminate_tree(self);
            let _ = self.child.try_wait();
        }
    }
}

#[cfg(unix)]
fn terminate_tree(tree: &mut OwnedProcessTree) -> io::Result<()> {
    let pid = i32::try_from(tree.child.id())
        .map_err(|_| io::Error::other("child process ID exceeds i32"))?;
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn tree_is_empty(tree: &OwnedProcessTree) -> io::Result<bool> {
    let pid = i32::try_from(tree.child.id())
        .map_err(|_| io::Error::other("child process ID exceeds i32"))?;
    let result = unsafe { libc::kill(-pid, 0) };
    if result == 0 {
        Ok(false)
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(true)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
pub(crate) fn create_kill_on_close_job() -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    create_named_kill_on_close_job(None)
}

#[cfg(windows)]
fn create_named_kill_on_close_job(name: Option<&str>) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    let wide: Option<Vec<u16>> = name.map(|name| name.encode_utf16().chain(Some(0)).collect());
    let job = unsafe { CreateJobObjectW(std::ptr::null(), wide.as_ref().map_or(std::ptr::null(), |name| name.as_ptr())) };
    if !job.is_null() && name.is_some() && unsafe { windows_sys::Win32::Foundation::GetLastError() } == 183 {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "Owned operation Job already exists"));
    }
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
        return Err(error);
    }
    Ok(job)
}

#[cfg(windows)]
pub(crate) fn assign_process_to_job(
    process: windows_sys::Win32::Foundation::HANDLE,
    job: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<()> {
    let ok =
        unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job, process) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) fn assign_child_to_job(
    child: &Child,
    job: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    assign_process_to_job(child.as_raw_handle().cast(), job)
}

#[cfg(windows)]
pub(crate) fn terminate_job_tree(job: usize) -> io::Result<()> {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
    let ok = unsafe { TerminateJobObject(job as windows_sys::Win32::Foundation::HANDLE, 1) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// An owned Windows Job handle. The handle is not inherited by its members.
/// Keeping it as an integer makes its thread-safe ownership explicit without
/// lending the raw handle to tasks that could outlive this owner.
#[cfg(windows)]
pub(crate) struct WindowsJob(usize);

/// Query only a previously persisted operation Job; never terminate or adopt
/// an arbitrary process. A missing Job has no associated live processes.
pub(crate) fn named_operation_job_is_empty(name: &str) -> io::Result<bool> {
    #[cfg(windows)] {
        use windows_sys::Win32::{Foundation::{CloseHandle, GetLastError}, System::JobObjects::OpenJobObjectW};
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let job = unsafe { OpenJobObjectW(0x0004 /* JOB_OBJECT_QUERY */, 0, wide.as_ptr()) };
        if job.is_null() {
            let error = unsafe { GetLastError() };
            return if error == 2 { Ok(true) } else { Err(io::Error::from_raw_os_error(error as i32)) };
        }
        let result = job_is_empty(job);
        unsafe { CloseHandle(job) };
        result
    }
    #[cfg(not(windows))] { let _ = name; Err(io::Error::new(io::ErrorKind::Unsupported, "Named process-tree recovery requires Windows")) }
}

#[cfg(windows)]
impl WindowsJob {
    pub(crate) fn contains_pid(&self, pid: u32) -> io::Result<bool> {
        use windows_sys::Win32::{Foundation::CloseHandle, System::{Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION}, JobObjects::IsProcessInJob}};
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() { return Err(io::Error::last_os_error()); }
            let mut belongs = 0;
            let result = IsProcessInJob(process, self.0 as _, &mut belongs);
            let error = io::Error::last_os_error();
            CloseHandle(process);
            if result == 0 { Err(error) } else { Ok(belongs != 0) }
        }
    }

    pub(crate) fn new() -> io::Result<Self> {
        create_kill_on_close_job().map(|job| Self(job as usize))
    }

    pub(crate) fn assign_native_and_resume(&self, handle: std::os::windows::io::RawHandle, pid: u32) -> io::Result<()> {
        assign_process_to_job(handle.cast(), self.0 as _)?;
        resume_process_primary_thread(pid)
    }

    pub(crate) fn terminate(&self) -> io::Result<()> { terminate_job_tree(self.0) }

    pub(crate) fn is_empty(&self) -> io::Result<bool> { job_is_empty(self.0 as _) }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0 as _) };
    }
}


#[cfg(windows)]
fn resume_process_primary_thread(process_id: u32) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    let mut thread_ids = Vec::new();
    while found {
        if entry.th32OwnerProcessID == process_id {
            thread_ids.push(entry.th32ThreadID);
        }
        found = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    if thread_ids.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "suspended materialization process has {} primary-thread candidates",
                thread_ids.len()
            ),
        ));
    }
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_ids[0]) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    let resumed = unsafe { ResumeThread(thread) };
    unsafe { CloseHandle(thread) };
    if resumed == u32::MAX {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn terminate_tree(tree: &mut OwnedProcessTree) -> io::Result<()> {
    let ok = unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(tree.job, 1) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn tree_is_empty(tree: &OwnedProcessTree) -> io::Result<bool> {
    job_is_empty(tree.job)
}

#[cfg(windows)]
fn job_is_empty(job: windows_sys::Win32::Foundation::HANDLE) -> io::Result<bool> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicAccountingInformation, QueryInformationJobObject,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    };
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(accounting.ActiveProcesses == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{NexusConfigFile, RuntimeConfig, RuntimePin};
    use nexus_protocol::{RuntimeInstallMode, RuntimeOwnership, RuntimeSource};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct FakePluginRunner {
        seen: std::sync::Mutex<Vec<PluginCommandSpec>>,
        outcome: PluginCommandOutcome,
    }

    impl PluginCommandRunner for FakePluginRunner {
        fn run(
            &self,
            _paths: &NexusPaths,
            spec: &PluginCommandSpec,
        ) -> io::Result<PluginCommandOutcome> {
            self.seen
                .lock()
                .expect("fake runner lock")
                .push(spec.clone());
            Ok(self.outcome.clone())
        }
    }

    fn test_dir(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nexus-agent-owned-process-{}-{}-{}",
            label,
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("creates isolated process test directory");
        root
    }

    fn executable_on_path(name: &str) -> Option<PathBuf> {
        let path = env::var_os("PATH")?;
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    }

    fn plugin_fixture() -> (PathBuf, NexusPaths, PathBuf, PathBuf) {
        let root = test_dir("plugin");
        let paths = NexusPaths::from_root(root.join("nexus-data"));
        let home = root.join("dsh-home");
        let profile = home.join("profiles/web");
        let release = root.join("release");
        fs::create_dir_all(&profile).expect("profile creates");
        fs::create_dir_all(release.join("apps/cli/lib")).expect("CLI parent creates");
        fs::write(release.join("apps/cli/lib/bin.js"), "// fixture").expect("CLI fixture writes");
        fs::write(
            release.join("apps/cli/package.json"),
            r#"{"bin":{"dsh":"lib/bin.js"}}"#,
        )
        .expect("CLI package fixture writes");
        fs::write(
            profile.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["dsh-base","dsh-web-app"]}},"dependencies":{"dsh-extra":"1.2.3"}}"#,
        ).expect("manifest writes");
        let node = root.join(if cfg!(windows) { "node.exe" } else { "node" });
        fs::write(&node, "fixture").expect("node fixture writes");
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { external_harness: None,
                runtime: Some(RuntimeConfig {
                    node: Some(RuntimePin {
                        path: node,
                        ownership: RuntimeOwnership::System,
                    }),
                    pnpm: None,
                    git: None,
                    source: RuntimeSource::Official,
                    mode: RuntimeInstallMode::Portable,
                }),
                ..Default::default()
            })
            .expect("runtime config writes");
        (root, paths, home, release)
    }

    #[test]
    fn plugin_output_reader_consumes_only_cap_plus_sentinel() {
        let root = test_dir("plugin-output-bound");
        let path = root.join("oversized-output.log");
        let oversized = vec![b'x'; MAX_PLUGIN_OUTPUT_BYTES + 1024 * 1024];
        fs::write(&path, oversized).expect("oversized output fixture writes");
        let output = read_output_bounded(&path).expect("bounded output reads");
        assert!(output.ends_with("\n[output truncated by Nexus]"));
        assert!(
            output.len() <= MAX_PLUGIN_OUTPUT_BYTES + "\n[output truncated by Nexus]".len()
        );
        fs::remove_dir_all(root).expect("fixture removes");
    }

    #[test]
    fn first_run_profile_inventory_is_empty_without_creating_home() {
        let root = test_dir("empty-native-profiles");
        let home = root.join("new-home");
        assert!(native_profiles(&home).expect("missing home is empty").is_empty());
        assert!(!home.exists());
        fs::create_dir(&home).expect("home fixture creates");
        assert!(native_profiles(&home).expect("missing profiles is empty").is_empty());
        fs::write(home.join("profiles"), "invalid directory").expect("invalid fixture writes");
        assert!(native_profiles(&home).is_err());
        fs::remove_dir_all(root).expect("fixture removes");
    }

    #[test]
    fn dependency_managed_bundles_are_removable_but_template_and_roots_are_protected() {
        let (root, _, home, _) = plugin_fixture();
        let path = home.join("profiles/web/package.json");
        fs::write(&path, r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-web-app","template","installed"]}},"dependencies":{"@deepseek-ai/dsh-base":"1","@deepseek-ai/dsh-web-app":"1","installed":"2","extra-dependency":"3"}}"#).unwrap();
        let inventory = native_profile(&home, "web").unwrap();
        assert!(inventory.plugins[..3].iter().all(|plugin| plugin.builtin && !plugin.removable));
        assert!(!inventory.plugins[3].builtin && inventory.plugins[3].removable);
        assert_eq!(inventory.plugins[3].version.as_deref(), Some("2"));
        assert_eq!(inventory.plugins[4].package, "extra-dependency");
        assert!(inventory.plugins[4].removable);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reorder_preserves_other_json_and_enforces_fixed_roots_and_valid_targets() {
        let (root, _, home, _) = plugin_fixture();
        let path = home.join("profiles/web/package.json");
        let original = serde_json::json!({"name":"keep", "custom":{"a":[1,true]},
            "dsh":{"profile":{"other":"unchanged","bundles":["b","@deepseek-ai/dsh-web-app","a","@deepseek-ai/dsh-base","template"]}},
            "dependencies":{"a":"1","b":"2","dependency-only":"3"}});
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        let moved = move_profile_plugin(&NexusPaths::from_root(home.join(".nexus-test")), &home, "web", "a", Some("b")).unwrap();
        assert_eq!(moved.bundles, ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app", "a", "b", "template"]);
        let mut expected = original;
        expected["dsh"]["profile"]["bundles"] = serde_json::json!(moved.bundles);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(), expected);
        let moved = move_profile_plugin(&NexusPaths::from_root(home.join(".nexus-test")), &home, "web", "a", None).unwrap();
        assert_eq!(moved.bundles, ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app", "b", "template", "a"]);
        let before_invalid = fs::read(&path).unwrap();
        for (package, target) in [("@deepseek-ai/dsh-base", None), ("@deepseek-ai/dsh-web-app", None),
            ("a", Some("@deepseek-ai/dsh-base")), ("a", Some("@deepseek-ai/dsh-web-app")),
            ("dependency-only", None), ("a", Some("dependency-only")), ("a", Some("absent")), ("a", Some("a"))] {
            assert!(move_profile_plugin(&NexusPaths::from_root(home.join(".nexus-test")), &home, "web", package, target).is_err());
            assert_eq!(fs::read(&path).unwrap(), before_invalid);
        }
        let ordinary = home.join("profiles/nexus-user");
        fs::create_dir(&ordinary).unwrap();
        fs::write(ordinary.join("package.json"), &before_invalid).unwrap();
        assert!(move_profile_plugin(&NexusPaths::from_root(home.join(".nexus-test")), &home, "nexus-user", "a", Some("b")).is_ok());
        let generated = home.join("profiles/copy");
        fs::create_dir(&generated).unwrap();
        fs::write(generated.join("package.json"), &before_invalid).unwrap();
        fs::write(generated.join(".nexus-compatibility.json"), serde_json::to_vec(&serde_json::json!({
            "checker_version":1,"status":"passed","source_profile":"web","effective_profile":"copy",
            "release_id":"test","fingerprint":"fixture","checked_at_unix":1,"disabled":[]
        })).unwrap()).unwrap();
        assert_eq!(native_profile(&home, "copy").unwrap().source_profile.as_deref(), Some("web"));
        assert!(move_profile_plugin(&NexusPaths::from_root(home.join(".nexus-test")), &home, "copy", "a", Some("b")).is_err());
        assert_eq!(fs::read(generated.join("package.json")).unwrap(), before_invalid);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn order_undo_preserves_raw_fields_and_rejects_external_edits_and_other_home() {
        let (root,paths,home,_release)=plugin_fixture();
        let manifest=home.join("profiles/web/package.json");
        let original=b"{\n \"name\":\"test\",\"unknown\":{\"key\":\"original\"},\"dependencies\":{},\"dsh\":{\"profile\":{\"bundles\":[\"a\",\"b\"]}}\n}";
        // Keep intentionally unusual formatting as the exact undo target.
        let original=String::from_utf8(original.to_vec()).unwrap().replace("\\n","\n").replace("\\\"","\"").into_bytes();
        fs::write(&manifest,&original).unwrap();
        let moved=move_profile_plugin(&paths,&home,"web","a",None).unwrap();
        let id=moved.order_undo_id.unwrap();
        let record=order_record_path(&paths,"web",&id,false).unwrap();
        nexus_private_file::verify_private(&fs::File::open(&record).unwrap()).unwrap();
        assert_eq!(order_undo_id(&paths,&home,"web").unwrap().as_deref(),Some(id.as_str()));
        let changed=fs::read(&manifest).unwrap();fs::write(&manifest,b"{\"name\":\"external\",\"dependencies\":{}} ").unwrap();
        assert!(undo_profile_order(&paths,&home,"web",&id).is_err());
        assert!(order_undo_id(&paths,&home,"web").unwrap().is_none());
        fs::write(&manifest,&changed).unwrap();
        let other=root.join("other-home");fs::create_dir_all(other.join("profiles/web")).unwrap();fs::write(other.join("profiles/web/package.json"),&changed).unwrap();
        assert!(undo_profile_order(&paths,&other,"web",&id).is_err());
        undo_profile_order(&paths,&home,"web",&id).unwrap();
        assert_eq!(fs::read(&manifest).unwrap(),original);
        assert!(order_undo_id(&paths,&home,"web").unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn failed_second_order_keeps_first_undo() {
        use std::os::windows::fs::OpenOptionsExt;
        let (root,paths,home,_release)=plugin_fixture();let manifest=home.join("profiles/web/package.json");
        let original=br#"{"name":"test","dependencies":{},"dsh":{"profile":{"bundles":["a","b","c"]}}}"#;
        fs::write(&manifest,original).unwrap();
        let id=move_profile_plugin(&paths,&home,"web","a",None).unwrap().order_undo_id.unwrap();
        let lock=fs::OpenOptions::new().read(true).share_mode(3).open(&manifest).unwrap();
        assert!(move_profile_plugin(&paths,&home,"web","b",None).is_err());drop(lock);
        assert_eq!(order_undo_id(&paths,&home,"web").unwrap().as_deref(),Some(id.as_str()));
        undo_profile_order(&paths,&home,"web",&id).unwrap();
        assert_eq!(fs::read(&manifest).unwrap(),original);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_manifest_preserves_bundle_order_and_marks_only_extra_dependencies_removable() {
        let (root, _paths, home, _release) = plugin_fixture();
        let profiles = native_profiles(&home).expect("native profiles load");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].bundles, ["dsh-base", "dsh-web-app"]);
        assert_eq!(profiles[0].plugins[0].package, "dsh-base");
        assert!(profiles[0].plugins[0].builtin);
        assert!(!profiles[0].plugins[0].removable);
        assert_eq!(profiles[0].plugins[2].package, "dsh-extra");
        assert!(profiles[0].plugins[2].removable);
        assert!(native_profile(&home, "../escape").is_err());
        fs::remove_dir_all(root).expect("fixture removes");
    }

    #[test]
    fn native_manifest_accepts_dependencies_omitted_after_last_removal() {
        let (root, _paths, home, _release) = plugin_fixture();
        let package = home.join("profiles/web/package.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&package).unwrap()).unwrap();
        manifest.as_object_mut().unwrap().remove("dependencies");
        fs::write(&package, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let profile = native_profile(&home, "web").expect("no dependencies remains valid");
        assert_eq!(profile.plugins.len(), 2);
        assert!(profile.plugins.iter().all(|plugin| plugin.builtin && !plugin.removable));
        manifest["dependencies"] = serde_json::Value::Null;
        fs::write(&package, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(native_profile(&home, "web").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plugin_remove_runner_receives_fixed_argv_bound_env_and_preserves_failure() {
        let (root, paths, home, release) = plugin_fixture();
        let runner = FakePluginRunner {
            seen: std::sync::Mutex::new(Vec::new()),
            outcome: PluginCommandOutcome {
                exit_code: Some(23),
                stdout: "partial output".to_owned(),
                stderr: "pnpm failed".to_owned(),
            },
        };
        let (outcome, _) =
            remove_profile_plugin_with_runner(&paths, &home, &release, "web", "dsh-extra", &runner)
                .expect("typed runner returns its failed exit");
        assert_eq!(outcome.exit_code, Some(23));
        assert_eq!(outcome.stderr, "pnpm failed");
        let seen = runner.seen.lock().expect("fake runner lock");
        let args: Vec<String> = seen[0]
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[1..],
            ["plugin", "--profile", "web", "remove", "dsh-extra"]
        );
        assert!(args[0].replace('\\', "/").ends_with("apps/cli/lib/bin.js"));
        #[cfg(windows)]
        assert!(!args[0].starts_with(r"\\?\"));
        assert!(same_native_path(
            Path::new(
                seen[0]
                    .env
                    .get(&OsString::from(DSH_HOME_ENV))
                    .expect("DSH_HOME bound")
            ),
            &home
        ));
        assert!(same_native_path(
            &seen[0].current_dir,
            &fs::canonicalize(home.join("profiles/web")).expect("profile canonical")
        ));
        drop(seen);

        let error =
            remove_profile_plugin_with_runner(&paths, &home, &release, "web", "dsh-base", &runner)
                .expect_err("built-in bundle is rejected before execution");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(runner.seen.lock().expect("fake runner lock").len(), 1);
        fs::write(release.join("apps/cli/package.json"), r#"{"bin":"other.js"}"#)
            .expect("invalid CLI package fixture writes");
        assert!(locate_built_cli(&release).is_err());
        fs::remove_dir_all(root).expect("fixture removes");
    }

    #[test]
    fn materializer_uses_pinned_node_exact_pnpm_policy_and_bound_home() {
        let root = test_dir("argv");
        let paths = NexusPaths::from_root(root.join("nexus-data"));
        let home = root.join("dsh-home");
        let profile = home.join("profiles/demo");
        fs::create_dir_all(&profile).expect("creates synthetic profile");
        fs::write(profile.join("package.json"), r#"{"name":"demo"}"#)
            .expect("writes synthetic package");
        let fake_pnpm = root.join("fake-pnpm.js");
        fs::write(
            &fake_pnpm,
            r#"const fs = require('fs');
const path = require('path');
fs.writeFileSync(path.join(process.cwd(), 'materialized.json'), JSON.stringify({
  argv: process.argv.slice(2),
  dshHome: process.env.DSH_HOME,
  cwd: process.cwd()
}));
"#,
        )
        .expect("writes fake pnpm entry");
        let node = executable_on_path(if cfg!(windows) { "node.exe" } else { "node" })
            .expect("test host provides Node required by the DSH runtime contract");
        ConfigStore::new(paths.clone())
            .write(&NexusConfigFile { external_harness: None,
                runtime: Some(RuntimeConfig {
                    node: Some(RuntimePin {
                        path: node,
                        ownership: RuntimeOwnership::System,
                    }),
                    pnpm: Some(RuntimePin {
                        path: fake_pnpm,
                        ownership: RuntimeOwnership::System,
                    }),
                    git: None,
                    source: RuntimeSource::Official,
                    mode: RuntimeInstallMode::Portable,
                }),
                ..Default::default()
            })
            .expect("writes pinned runtime config");

        materialize_profile_with_timeout(&paths, &home, "demo", Duration::from_secs(10))
            .expect("fake pnpm materialization succeeds");
        let observed: serde_json::Value = serde_json::from_slice(
            &fs::read(profile.join("materialized.json")).expect("reads fake pnpm observation"),
        )
        .expect("parses fake pnpm observation");
        assert_eq!(
            observed["argv"],
            serde_json::json!([
                "--config.minimumReleaseAge=0",
                "--config.registry=https://registry.npmjs.org",
                "install",
                "--frozen-lockfile"
            ])
        );
        assert!(same_native_path(
            Path::new(observed["dshHome"].as_str().expect("DSH_HOME is text")),
            &home
        ));
        assert!(same_native_path(
            Path::new(observed["cwd"].as_str().expect("cwd is text")),
            &fs::canonicalize(&profile).expect("canonical profile")
        ));
        fs::remove_dir_all(root).expect("removes isolated materialization test");
    }

    #[cfg(windows)]
    fn powershell() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot exists"))
            .join("System32/WindowsPowerShell/v1.0/powershell.exe")
    }

    #[cfg(windows)]
    fn ps_literal(path: &Path) -> String {
        path.to_string_lossy().replace('\'', "''")
    }

    #[cfg(windows)]
    #[test]
    fn timeout_ends_owned_descendant_before_returning() {
        let root = test_dir("descendant");
        let marker = root.join("descendant-writes.txt");
        let child_pid = root.join("descendant.pid");
        let child_script = root.join("child.ps1");
        let parent_script = root.join("parent.ps1");
        fs::write(
            &child_script,
            r#"param([string]$Marker)
while ($true) {
  Add-Content -LiteralPath $Marker -Value 'owned'
  Start-Sleep -Milliseconds 20
}
"#,
        )
        .expect("writes child fixture");
        fs::write(
            &parent_script,
            format!(
                r#"$child = Start-Process -FilePath "$PSHOME\powershell.exe" -ArgumentList @('-NoProfile','-File','{}','{}') -PassThru
$child.Id | Set-Content -LiteralPath '{}'
while ($true) {{ Start-Sleep -Seconds 1 }}
"#,
                ps_literal(&child_script),
                ps_literal(&marker),
                ps_literal(&child_pid),
            ),
        )
        .expect("writes parent fixture");

        let mut command = Command::new(powershell());
        command.args(["-NoProfile", "-File"]).arg(&parent_script);
        let error = run_owned_process(&mut command, Duration::from_secs(3))
            .expect_err("controlled process tree times out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(child_pid.is_file(), "descendant started before cleanup");
        assert!(marker.is_file(), "descendant wrote before cleanup");
        let length = fs::metadata(&marker).expect("marker metadata").len();
        thread::sleep(Duration::from_millis(250));
        assert_eq!(
            fs::metadata(&marker)
                .expect("marker remains readable")
                .len(),
            length,
            "owned descendant cannot write after timeout returns"
        );
        fs::remove_dir_all(root).expect("removes isolated process test directory");
    }

    #[cfg(windows)]
    #[test]
    fn setup_and_resume_failures_leave_suspended_child_inert() {
        for (index, fault) in [
            ProcessTreeFault::BeforeJobCreate,
            ProcessTreeFault::BeforeAssign,
            ProcessTreeFault::BeforeResume,
        ]
        .into_iter()
        .enumerate()
        {
            let root = test_dir(&format!("fault-{index}"));
            let marker = root.join("started.txt");
            let script = format!(
                "Set-Content -LiteralPath '{}' -Value 'started'",
                ps_literal(&marker)
            );
            let mut command = Command::new(powershell());
            command.args(["-NoProfile", "-Command", &script]);
            run_owned_process_inner(&mut command, Duration::from_secs(1), fault)
                .expect_err("injected ownership failure is returned");
            thread::sleep(Duration::from_millis(100));
            assert!(
                !marker.exists(),
                "suspended child never executes after ownership failure"
            );
            fs::remove_dir_all(root).expect("removes isolated process test directory");
        }
    }
}
