//! Stage self-contained built-in packages without editing the chosen Harness.
use std::io;
use nexus_core::{NexusPaths, HarnessLaunchSpec};

// Only flags whose startup semantics Nexus owns can use the single-start path.
pub(crate) fn single_start_arguments_supported(args: &[String], profile: &str) -> bool {
    let mut selected = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" if !selected && args.get(i + 1).is_some_and(|value| value == profile) => { selected = true; i += 2; },
            "--port" if args.get(i + 1).is_some_and(|value| value.parse::<u16>().is_ok()) => i += 2,
            "--no-open" => i += 1,
            value if value.strip_prefix("--port=").is_some_and(|port| port.parse::<u16>().is_ok()) => i += 1,
            _ => return false,
        }
    }
    selected
}

// This is a startup protocol capability, not support for every optional setting.
// Unsupported artifacts use ordinary readiness observation, never a pre-start diagnostic.
pub(crate) fn single_start_supported(root: &std::path::Path, home: &std::path::Path, profile: &str) -> bool {
    let supported = || -> io::Result<bool> {
        let read = |file: &std::path::Path| nexus_core::read_regular_file_bounded(file, 2 * 1024 * 1024)
            .and_then(|bytes| String::from_utf8(bytes.ok_or_else(|| io::Error::other("missing startup artifact"))?).map_err(io::Error::other));
        let manifest: serde_json::Value = serde_json::from_str(&read(&root.join("package.json"))?)?;
        if manifest["name"] != "@deepseek-ai/dsh-root" { return Ok(false); }
        let profile_manifest = home.join("profiles").join(profile).join("package.json");
        if profile_manifest.exists() {
            let value: serde_json::Value = serde_json::from_str(&read(&profile_manifest)?)?;
            if !value.pointer("/dsh/profile/bundles").and_then(|v| v.as_array())
                .is_some_and(|bundles| bundles.iter().any(|v| v == "@deepseek-ai/dsh-web-app")) { return Ok(false); }
        } else if profile != "web" { return Ok(false); }
        let cli = root.join("apps/cli/lib");
        let facade = read(&cli.join("profile-boot.js"))?;
        // Inspect only the relative chunk referenced by the public facade.
        let Some(chunk) = facade.split('"').find_map(|part| part.strip_prefix("./profile-boot-")
            .filter(|name| name.ends_with(".js") && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))) else { return Ok(false); };
        let boot = read(&cli.join(format!("profile-boot-{chunk}")))?;
        Ok(boot.contains("appReady.commit()") && boot.contains("onReady(listener)") && boot.contains("await boot(")
            && read(&root.join("packages/boot/app-boot/lib/index.js"))?.contains("startupDiagnostic")
            && read(&cli.join("bin.js"))?.contains("reportStartupFailure"))
    };
    match supported() {
        Ok(true) => true,
        Ok(false) => {
            tracing::info!(profile, "Required Web startup readiness/diagnostic capability is unavailable; using ordinary startup observation");
            false
        }
        Err(error) => {
            tracing::info!(profile, reason = %error, "Cannot verify Web startup capability; using ordinary startup observation");
            false
        }
    }
}

fn host_startup(paths: &NexusPaths, run: &str) -> Option<serde_json::Value> {
    let bytes = nexus_core::read_regular_file_bounded(&paths.run_dir.join("host-startup.json"), 16384).ok()??;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    (value["run"].as_str() == Some(run) && value["state"] == "ready"
        && value["pid"].as_u64().is_some_and(|pid| pid > 1 && pid <= u32::MAX as u64)).then_some(value)
}

pub(crate) fn host_startup_pid(paths: &NexusPaths, run: &str) -> Option<u32> {
    host_startup(paths, run)?["pid"].as_u64().map(|pid| pid as u32)
}

pub(crate) fn browser_open_deferred(paths: &NexusPaths, run: &str) -> bool {
    host_startup(paths, run).is_some_and(|value| value["auto_open"] == true)
}

pub(crate) fn host_skipped_bundles(paths: &NexusPaths, run: &str, pid: u32) -> Option<serde_json::Value> {
    let host = host_startup(paths, run)?;
    if host["pid"].as_u64() != Some(pid as u64) { return None; }
    let value = host.get("skipped_bundles")?;
    if value.as_object()?.len() != 2 || !value["truncated"].is_boolean() { return None; }
    let entries = value["entries"].as_array()?;
    if entries.len() > 48 || serde_json::to_vec(entries).ok()?.len() > 8000 { return None; }
    for entry in entries {
        if entry.as_object()?.len() != 2 || entry["package"].as_str()?.encode_utf16().count() > 240
            || entry["reason"].as_str()?.encode_utf16().count() > 400 { return None; }
    }
    Some(value.clone())
}

pub(crate) fn stage_startup_observer(paths: &NexusPaths) -> io::Result<String> {
    let directory = paths.run_dir.join("plugins/nexus-startup-observer");
    std::fs::create_dir_all(&directory)?;
    for (name, bytes) in [
        ("startup-observer.mjs", include_bytes!("startup-observer.mjs").as_slice()),
        ("startup-diagnosis.mjs", include_bytes!("startup-diagnosis.mjs").as_slice()),
    ] { nexus_core::write_private_bytes_atomic(&paths.root, &directory.join(name), bytes)?; }
    module_url(&directory.join("startup-observer.mjs"))
}

pub(crate) fn browser_health(paths: &NexusPaths, run: &str) -> serde_json::Value {
    use std::io::Read;
    let read = || -> io::Result<serde_json::Value> {
        let file = paths.run_dir.join("browser-health.json");
        let meta = std::fs::symlink_metadata(&file)?;
        if !meta.is_file() || nexus_core::path_is_reparse(&meta) || meta.len() > 65536 {
            return Err(io::Error::other("Invalid browser health evidence"));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(file)?.take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 { return Err(io::Error::other("Browser health evidence too large")); }
        Ok(serde_json::from_slice(&bytes)?)
    };
    if let Ok(mut value) = read() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        if value["run"].as_str() == Some(run) && value["observed_at"].as_u64().is_some_and(|time| time <= now && now - time <= 20000)
            && matches!(value["state"].as_str(), Some("checking" | "blocked" | "active" | "limited" | "unverified"))
            && value["entries"].as_array().is_some_and(|entries| entries.len() <= 128) {
            value.as_object_mut().unwrap().remove("run");
            // The browser cannot supply native host observations. harness_ui
            // attaches these only from the separately validated ready record.
            value.as_object_mut().unwrap().remove("host_skipped_bundles");
            return value;
        }
    }
    serde_json::json!({"state":"unverified"})
}

pub(crate) fn module_url(path: &std::path::Path) -> io::Result<String> {
    let path = std::path::PathBuf::from(nexus_core::node_script_argument(path));
    reqwest::Url::from_file_path(path).map(|url| url.to_string())
        .map_err(|_| io::Error::other("Built-in plugin requires an absolute module path"))
}

pub(crate) fn stage(paths: &NexusPaths, home: &std::path::Path, profile: &str) -> io::Result<std::path::PathBuf> {
    let directory = paths.run_dir.join("plugins/nexus-desktop-compat");
    std::fs::create_dir_all(&directory)?;
    for (name, bytes) in [
        ("package.json", include_bytes!("../../../plugins/nexus-desktop-compat/package.json").as_slice()),
        ("index.mjs", include_bytes!("../../../plugins/nexus-desktop-compat/index.mjs").as_slice()),
    ] { nexus_core::write_private_bytes_atomic(&paths.root, &directory.join(name), bytes)?; }
    let patch = paths.run_dir.join("desktop-compat-plugin.json");
    let mut rows = vec![serde_json::json!({ "insert": [{ "id": "nexus-desktop-compat", "name": module_url(&directory.join("index.mjs"))? }] })];
    // dshmarket checks this service once during apply. Explicit dependency avoids
    // module-import timing selecting its unmanaged restart/process path.
    let market = match crate::dsh::native_profile(home, profile) {
        Ok(profile) => profile.bundles.iter().any(|name| name == "dshmarket"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if market {
        rows.push(serde_json::json!({"id":"dsh-market", "inject":["desktopProfiles"]}));
    }
    nexus_core::write_private_bytes_atomic(&paths.root, &patch, &serde_json::to_vec(&rows)?)?;
    Ok(patch)
}

pub(crate) fn context(paths: &NexusPaths, home: &std::path::Path, profile: &str,
    node: &std::path::Path, slot: &std::path::Path, pnpm: Option<&std::path::Path>) -> io::Result<String> {
    let bridge = std::env::current_exe()?.with_file_name(if cfg!(windows) { "nexus-desktop-bridge.exe" } else { "nexus-desktop-bridge" });
    Ok(serde_json::json!({ "root": paths.root, "home": home, "profile": profile,
        "node": node, "entry": nexus_core::node_script_argument(&slot.join("apps/cli/lib/bin.js")).to_string_lossy(),
        "pnpm": pnpm.map(|path| nexus_core::node_script_argument(path).to_string_lossy().into_owned()), "bridge": bridge }).to_string())
}

pub(crate) fn prepare(paths: &NexusPaths, home: &std::path::Path, profile: &str, spec: &mut HarnessLaunchSpec) -> io::Result<()> {
    if !nexus_core::harness_preferences_cli_supported(spec) { return Ok(()); }
    let directory = paths.run_dir.join("plugins/nexus-desktop-bridge");
    std::fs::create_dir_all(&directory)?;
    for (name, bytes) in [
        ("package.json", include_bytes!("../../../plugins/nexus-desktop-bridge/package.json").as_slice()),
        ("index.mjs", include_bytes!("../../../plugins/nexus-desktop-bridge/index.mjs").as_slice()),
        ("client.js", include_bytes!("../../../plugins/nexus-desktop-bridge/client.js").as_slice()),
    ] { nexus_core::write_private_bytes_atomic(&paths.root, &directory.join(name), bytes)?; }
    let patch = paths.run_dir.join("desktop-plugin.json");
    let rows = serde_json::json!([{ "insert": [{ "id": "nexus-desktop-bridge", "name": module_url(&directory.join("index.mjs"))? }] }]);
    nexus_core::write_private_bytes_atomic(&paths.root, &patch, &serde_json::to_vec(&rows)?)?;
    let start = usize::from(spec.mode == nexus_protocol::HarnessLaunchMode::Node);
    let compat = stage(paths, home, profile)?;
    spec.args.splice(start..start, ["--patch".into(), compat.to_string_lossy().into_owned(), "--patch".into(), patch.to_string_lossy().into_owned()]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn web_flags_do_not_disable_single_start_but_unknown_arguments_do() {
        for flags in [vec!["--port", "0"], vec!["--port=3851", "--no-open"], vec![]] {
            let args: Vec<String> = [vec!["bin.js", "--profile", "web"], flags].concat().into_iter().map(str::to_owned).collect();
            assert!(single_start_arguments_supported(&args, "web"));
        }
        for flags in [vec!["--port", "65536"], vec!["--patch", "custom.yml"], vec!["--profile", "other"], vec!["--unknown"]] {
            let args: Vec<String> = [vec!["bin.js", "--profile", "web"], flags].concat().into_iter().map(str::to_owned).collect();
            assert!(!single_start_arguments_supported(&args, "web"));
        }
    }

    #[test]
    fn host_commit_requires_the_current_process_and_run() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-host-commit-{}", nexus_core::unix_time_nanos_for_update())));
        paths.ensure_directories().unwrap();
        assert_eq!(host_startup_pid(&paths, "current"), None);
        let file = paths.run_dir.join("host-startup.json");
        std::fs::write(&file, r#"{"run":"current","pid":42,"state":"ready"}"#).unwrap();
        assert_eq!(host_startup_pid(&paths, "current"), Some(42));
        assert_eq!(host_startup_pid(&paths, "old"), None);
        std::fs::write(&file, "malformed").unwrap();
        assert_eq!(host_startup_pid(&paths, "current"), None);
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn host_skips_require_bounded_current_ready_evidence() {
        let paths = NexusPaths::from_root(std::env::temp_dir().join(format!("nexus-host-skips-{}", nexus_core::unix_time_nanos_for_update())));
        paths.ensure_directories().unwrap();
        let file = paths.run_dir.join("host-startup.json");
        let entries: Vec<_> = (0..5).map(|i| serde_json::json!({"package":format!("p{i}"), "reason":"界".repeat(400)})).collect();
        let mut host = serde_json::json!({"run":"current", "pid":42, "state":"ready", "auto_open":true,
            "skipped_bundles":{"entries":entries,"truncated":false}});
        let write = |value: &serde_json::Value| std::fs::write(&file, serde_json::to_vec(value).unwrap()).unwrap();
        write(&host);
        assert!(std::fs::metadata(&file).unwrap().len() > 4096);
        assert_eq!(host_startup_pid(&paths, "current"), Some(42));
        assert!(browser_open_deferred(&paths, "current"));
        assert_eq!(host_skipped_bundles(&paths, "current", 42), Some(host["skipped_bundles"].clone()));
        assert!(host_skipped_bundles(&paths, "old", 42).is_none());
        assert!(host_skipped_bundles(&paths, "current", 41).is_none());
        host["state"] = "starting".into(); write(&host);
        assert!(host_skipped_bundles(&paths, "current", 42).is_none());
        host["state"] = "ready".into();
        host["skipped_bundles"]["entries"][0]["reason"] = "x".repeat(401).into(); write(&host);
        assert!(host_skipped_bundles(&paths, "current", 42).is_none());
        host["skipped_bundles"]["entries"] = serde_json::json!([]);
        host["skipped_bundles"]["truncated"] = "true".into(); write(&host);
        assert!(host_skipped_bundles(&paths, "current", 42).is_none());
        host["skipped_bundles"]["truncated"] = true.into(); write(&host);
        assert!(host_skipped_bundles(&paths, "current", 42).is_some());
        std::fs::write(&file, " ".repeat(16385)).unwrap();
        assert!(host_skipped_bundles(&paths, "current", 42).is_none());
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn single_start_requires_known_web_artifacts_and_falls_back_on_changes() {
        let root = std::env::temp_dir().join(format!("nexus-single-start-{}", nexus_core::unix_time_nanos_for_update()));
        let home = root.join("home");
        for directory in ["apps/cli/lib", "packages/boot/app-boot/lib"] { std::fs::create_dir_all(root.join(directory)).unwrap(); }
        std::fs::write(root.join("package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.6-alpha.2"}"#).unwrap();
        std::fs::write(root.join("apps/cli/lib/profile-boot.js"), "export { runProfile } from \"./profile-boot-fixture.js\";").unwrap();
        std::fs::write(root.join("apps/cli/lib/profile-boot-fixture.js"), "await boot(); onReady(listener); appReady.commit()").unwrap();
        std::fs::write(root.join("apps/cli/lib/bin.js"), "reportStartupFailure").unwrap();
        std::fs::write(root.join("packages/boot/app-boot/lib/index.js"), "startupDiagnostic").unwrap();
        assert!(single_start_supported(&root, &home, "web"));
        for version in ["0.1.7-alpha.1", "9.0.0-future"] {
            std::fs::write(root.join("package.json"), format!(r#"{{"name":"@deepseek-ai/dsh-root","version":"{version}"}}"#)).unwrap();
            assert!(single_start_supported(&root, &home, "web"));
        }
        assert!(!single_start_supported(&root, &home, "headless"));
        std::fs::create_dir_all(home.join("profiles/web")).unwrap();
        std::fs::write(home.join("profiles/web/package.json"), r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-headless"]}}}"#).unwrap();
        assert!(!single_start_supported(&root, &home, "web"));
        std::fs::remove_file(home.join("profiles/web/package.json")).unwrap();
        std::fs::write(root.join("apps/cli/lib/profile-boot-fixture.js"), "unsupported API").unwrap();
        assert!(!single_start_supported(&root, &home, "web"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn browser_health_requires_current_run_and_fresh_evidence() {
        let root = std::env::temp_dir().join(format!("nexus-browser-health-{}", nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root);
        paths.ensure_directories().unwrap();
        let file = paths.run_dir.join("browser-health.json");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let mut report = serde_json::json!({"run":"current", "observed_at":now, "state":"blocked", "entries":[],
            "host_skipped_bundles":{"entries":[{"package":"browser-supplied","reason":"not host evidence"}],"truncated":false}});
        std::fs::write(&file, serde_json::to_vec(&report).unwrap()).unwrap();
        assert_eq!(browser_health(&paths, "current")["state"], "blocked");
        assert!(browser_health(&paths, "current").get("host_skipped_bundles").is_none());
        assert_eq!(browser_health(&paths, "next")["state"], "unverified");
        report["observed_at"] = serde_json::json!(now - 30000);
        std::fs::write(&file, serde_json::to_vec(&report).unwrap()).unwrap();
        assert_eq!(browser_health(&paths, "current")["state"], "unverified");
        std::fs::write(&file, b"{broken").unwrap();
        assert_eq!(browser_health(&paths, "current")["state"], "unverified");
        std::fs::remove_dir_all(paths.root).unwrap();
    }
    #[test]
    fn generated_plugin_json_uses_string_paths_and_loads_in_upstream_loader() {
        let root = std::env::temp_dir().join(format!("nexus-plugin-json 空格#-{}-{}", std::process::id(), nexus_core::unix_time_nanos_for_update()));
        let paths = NexusPaths::from_root(root.join("nexus"));
        paths.ensure_directories().unwrap();
        let home = root.join("home");
        let observer = reqwest::Url::parse(&stage_startup_observer(&paths).unwrap()).unwrap().to_file_path().unwrap();
        assert_eq!(std::fs::read(&observer).unwrap(), include_bytes!("startup-observer.mjs"));
        assert_eq!(std::fs::read(observer.with_file_name("startup-diagnosis.mjs")).unwrap(), include_bytes!("startup-diagnosis.mjs"));
        let mut spec = HarnessLaunchSpec::new("node".into());
        spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        spec.args = vec![root.join("apps/cli/lib/bin.js").to_string_lossy().into_owned(), "--profile".into(), "web".into()];
        prepare(&paths, &home, "web", &mut spec).unwrap();
        assert!(crate::notifications::prepare(&paths, &mut spec).unwrap());
        for name in ["desktop-compat-plugin.json", "desktop-plugin.json", "notification-plugin.json"] {
            let value: serde_json::Value = serde_json::from_slice(&std::fs::read(paths.run_dir.join(name)).unwrap()).unwrap();
            let module = value[0]["insert"][0]["name"].as_str().expect("Loader module names must be JSON strings, never serialized OsString objects");
            assert!(reqwest::Url::parse(module).unwrap().to_file_path().unwrap().is_file());
        }
        let json: serde_json::Value = serde_json::from_str(&context(&paths, &home, "web", &root.join("node"), &root, Some(&root.join("pnpm.cjs"))).unwrap()).unwrap();
        for key in ["node", "entry", "pnpm", "bridge", "root", "home"] { assert!(json[key].is_string(), "{key} must be a string"); }
        if let Some(harness) = std::env::var_os("NEXUS_TEST_DSH_ROOT") {
            let script = root.join("verify.mjs");
            std::fs::write(&script, r#"
import fs from 'node:fs'; import path from 'node:path'; import { createRequire } from 'node:module'; import { pathToFileURL } from 'node:url';
const require = createRequire(path.join(process.argv[2], 'apps/cli/package.json'));
const { Context } = await import(pathToFileURL(require.resolve('@deepseek-ai/cordis')));
const { Loader } = await import(pathToFileURL(require.resolve('@deepseek-ai/cordis-plugin-loader')));
const ctx = new Context(); const owner = ctx.plugin(Loader); await new Promise(resolve => setTimeout(resolve, 20)); const loader = ctx.loader; loader.write = () => {};
for (const file of ['desktop-compat-plugin.json','desktop-plugin.json','notification-plugin.json']) {
  const rows = JSON.parse(fs.readFileSync(path.join(process.argv[3], file)));
  for (const row of rows) for (const entry of row.insert || []) await loader.create(entry);
}
await loader.root.stop(); await owner.dispose();
"#).unwrap();
            let result = std::process::Command::new("node").arg("--expose-internals").arg(script).arg(harness).arg(&paths.run_dir)
                .env_remove("NEXUS_DESKTOP_CONTEXT").env_remove("NEXUS_NOTIFICATION_FILE").output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
