//! Stage self-contained built-in packages without editing the chosen Harness.
use std::io;
use nexus_core::{NexusPaths, HarnessLaunchSpec};

// This is a startup protocol capability, not support for every optional setting.
// Unknown/older artifacts retain the isolated compatibility check.
pub(crate) fn single_start_supported(root: &std::path::Path, home: &std::path::Path, profile: &str) -> bool {
    let supported = || -> io::Result<bool> {
        let read = |file: &std::path::Path| nexus_core::read_regular_file_bounded(file, 2 * 1024 * 1024)
            .and_then(|bytes| String::from_utf8(bytes.ok_or_else(|| io::Error::other("missing startup artifact"))?).map_err(io::Error::other));
        let manifest: serde_json::Value = serde_json::from_str(&read(&root.join("package.json"))?)?;
        if manifest["name"] != "@deepseek-ai/dsh-root" || manifest["version"] != "0.1.6-alpha.2" { return Ok(false); }
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
    supported().unwrap_or(false)
}

pub(crate) fn host_startup_pid(paths: &NexusPaths, run: &str) -> Option<u32> {
    let read = || -> io::Result<Option<u32>> {
        let Some(bytes) = nexus_core::read_regular_file_bounded(&paths.run_dir.join("host-startup.json"), 4096)? else { return Ok(None); };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        Ok((value["run"].as_str() == Some(run) && value["state"] == "ready").then(|| value["pid"].as_u64()
            .and_then(|pid| u32::try_from(pid).ok()).filter(|pid| *pid > 1)).flatten())
    };
    read().ok().flatten()
}

pub(crate) fn browser_open_deferred(paths: &NexusPaths, run: &str) -> bool {
    let read = || -> io::Result<bool> {
        let Some(bytes) = nexus_core::read_regular_file_bounded(&paths.run_dir.join("host-startup.json"), 4096)? else { return Ok(false); };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        Ok(value["run"].as_str() == Some(run) && value["state"] == "ready" && value["auto_open"] == true)
    };
    read().unwrap_or(false)
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
        let mut report = serde_json::json!({"run":"current", "observed_at":now, "state":"blocked", "entries":[]});
        std::fs::write(&file, serde_json::to_vec(&report).unwrap()).unwrap();
        assert_eq!(browser_health(&paths, "current")["state"], "blocked");
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
