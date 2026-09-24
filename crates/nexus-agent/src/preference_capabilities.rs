//! Version and bundle evidence for Nexus-owned optional Harness settings.
use std::{fs, io::{self, Read}, path::Path};
use nexus_core::{HarnessProfileCapabilities, HarnessLaunchSpec};
use nexus_protocol::HarnessPreferencesPayload;

pub(crate) const ADAPTER_VERSION: &str = "artifact-settings-v2";
#[cfg(test)]
const VERIFIED_VERSION: &str = "0.1.2-rc.1";

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PreferenceCapabilityEvidence {
    pub version: String,
    pub profile: String,
    pub bundles: Vec<String>,
    pub capabilities: HarnessProfileCapabilities,
}

fn unsupported(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn resolve(root: Option<&Path>, home: &Path, profile: &str, preferences: &HarnessPreferencesPayload) -> io::Result<HarnessProfileCapabilities> {
    let mut effective = preferences.clone();
    effective.patch_entries = None; // Metadata and disabled entries are not overrides.
    let preferences = nexus_core::normalize_harness_preferences(effective)?;
    if preferences == HarnessPreferencesPayload::default() { return Ok(HarnessProfileCapabilities::default()); }
    let root = root.ok_or_else(|| unsupported("Harness settings are unverified without an installed release. Clear explicit overrides to use the original configuration."))?;
    let evidence = inspect(root, home, profile)?;
    {
        for (enabled, file, token, name) in [
            (preferences.home.is_some(), "packages/util/home-paths/src/index.ts", "env[DSH_HOME_ENV]", "home"),
            (preferences.deepseek_base_url.is_some(), "packages/llm/llm-deepseek/src/config.ts", "DEEPSEEK_BASE_URL", "deepseek_base_url"),
            (preferences.search_base_url.is_some(), "packages/web/web-search-deepseek/src/index.ts", "DEEPSEEK_SEARCH_BASE_URL", "search_base_url"),
            (preferences.search_provider.is_some(), "packages/web/web/src/index.ts", "process.env.DSH_WEB_SEARCH_PROVIDER", "search_provider"),
            (preferences.fetch_provider.is_some(), "packages/web/web/src/index.ts", "process.env.DSH_WEB_FETCH_PROVIDER", "fetch_provider"),
            (preferences.agents_home.is_some(), "packages/skill/skill-filesystem/src/index.ts", "process.env.DSH_AGENTS_HOME", "agents_home"),
            (preferences.bundled_skill_dir.is_some(), "packages/skill/skill-filesystem/src/index.ts", "process.env.DSH_BUNDLED_SKILL_DIR", "bundled_skill_dir"),
            (preferences.permission_mode.is_some(), "packages/bundle/base/cordis.patch.yml", "process.env.DSH_PERMISSION_MODE", "permission_mode"),
            (preferences.telemetry_disabled.is_some(), "packages/boot/app-boot/src/profile-context.ts", "resolveTelemetryPatch", "telemetry_disabled"),
        ] {
            if enabled && !artifact_contains(root, file, token) {
                return Err(unsupported(format!("Harness setting {name} is unverified in this artifact; clear it to inherit upstream behavior.")));
            }
        }
    }
    evidence.capabilities.validate_preferences(&preferences)?;
    Ok(evidence.capabilities)
}

// Resolve against the selected artifact, never a global migration marker: old
// and new Harness releases may coexist against different profiles.
pub(crate) fn settings_document(root: Option<&Path>, home: &Path, profile: &str) -> io::Result<std::path::PathBuf> {
    nexus_core::validate_profile_name(profile)?;
    let root = root.ok_or_else(|| unsupported("Cannot determine settings layout without a selected Harness artifact"))?;
    let read = |relative: &str| -> io::Result<String> {
        let bytes = nexus_core::read_regular_file_bounded(&root.join(relative), 512 * 1024)?;
        bytes.map(String::from_utf8).transpose().map_err(io::Error::other).map(Option::unwrap_or_default)
    };
    // Built-only external distributions are supported too. Unknown is not legacy.
    for relative in ["packages/settings/settings/lib/index.js", "packages/settings/settings/src/index.ts"] {
        let text = read(relative)?;
        if text.contains("configEditor.documentPath") {
            return Ok(home.join("profiles").join(profile).join("cordis.patch.yml"));
        }
    }
    for relative in ["packages/settings/settings-file/lib/index.js", "packages/settings/settings-file/src/index.ts"] {
        let text = read(relative)?;
        if text.contains("settings.yaml") && text.contains("documentPath") {
            return Ok(home.join("settings.yaml"));
        }
    }
    Err(unsupported("Unknown Harness settings layout; open the profile directory or use Harness settings"))
}

fn artifact_contains(root: &Path, relative: &str, token: &str) -> bool {
    nexus_core::read_regular_file_bounded(&root.join(relative), 512 * 1024)
        .ok().flatten().and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|text| text.contains(token))
}

// Verify the CLI-to-service contract rather than extending a release allowlist.
fn web_cli_capability(root: &Path) -> bool {
    let read = |relative: &str| nexus_core::read_regular_file_bounded(&root.join(relative), 512 * 1024)
        .ok().flatten().and_then(|bytes| String::from_utf8(bytes).ok()).unwrap_or_default();
    let startup = read("packages/bundle/web-app/src/startup.ts");
    let patch = read("packages/bundle/web-app/cordis.patch.yml");
    startup.contains(".option('--port <port>'") && startup.contains("port: Number(options.port)")
        && startup.contains("options.port !== undefined") && startup.contains("parseCmdline(ctx, program)")
        && startup.contains(".option('--no-open'") && patch.contains("ctx.webStartup.port ?? 3080")
}

/// Exposes verified version and original profile identity for configuration explanations.
pub(crate) fn inspect(root: &Path, home: &Path, profile: &str) -> io::Result<PreferenceCapabilityEvidence> {
    let manifest_path = root.join("package.json");
    let metadata = fs::symlink_metadata(&manifest_path)?;
    if !metadata.is_file() || nexus_core::path_is_reparse(&metadata) || metadata.len() > 1024 * 1024 {
        return Err(unsupported("Harness version manifest is not an ordinary bounded file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(&manifest_path)?.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 { return Err(unsupported("Harness version manifest is too large")); }
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let version = manifest.get("version").and_then(|v| v.as_str()).unwrap_or("");
    let web_cli_supported = web_cli_capability(root);
    if manifest.get("name").and_then(|v| v.as_str()) != Some("@deepseek-ai/dsh-root")
        || version.is_empty() {
        return Err(unsupported("Harness settings require a valid managed Harness artifact identity."));
    }
    let bundles = match crate::dsh::native_profile(home, profile) {
        Ok(inventory) => {
            for plugin in &inventory.plugins {
                if inventory.bundles.contains(&plugin.package) && matches!(plugin.package.as_str(),
                    "@deepseek-ai/dsh-web-app" | "@deepseek-ai/dsh-headless" | "@deepseek-ai/dsh-sdk-app" | "@deepseek-ai/dsh-sdk-minimal")
                    && plugin.version.as_deref().is_some_and(|version| version != manifest["version"].as_str().unwrap_or("")) {
                    return Err(unsupported(format!("Explicit settings are unverified for the installed {} bundle version", plugin.package)));
                }
            }
            inventory.bundles
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let bundle = match profile {
                "web" => "@deepseek-ai/dsh-web-app", "headless" => "@deepseek-ai/dsh-headless",
                "sdk" => "@deepseek-ai/dsh-sdk-app", "sdk-minimal" => "@deepseek-ai/dsh-sdk-minimal",
                "acp" => "@deepseek-ai/dsh-acp-app",
                _ => return Err(unsupported("The selected profile has no manifest; its settings capabilities are unverified")),
            };
            vec![bundle.to_owned()]
        },
        Err(error) => return Err(error),
    };
    let mut capabilities = HarnessProfileCapabilities::from_bundles(&bundles);
    {
        capabilities.tools_mode &= bundles.iter().any(|bundle| {
            let file = match bundle.as_str() {
                "@deepseek-ai/dsh-web-app" => "packages/bundle/web-app/cordis.patch.yml",
                "@deepseek-ai/dsh-headless" => "packages/bundle/headless/cordis.patch.yml",
                _ => return false,
            };
            artifact_contains(root, file, "process.env.DSH_TOOLS_MODE")
        });
        capabilities.sdk_minimal &= artifact_contains(root, "packages/bundle/sdk-minimal/cordis.patch.yml", "process.env.DSH_CONTEXT_WINDOW")
            && artifact_contains(root, "packages/bundle/sdk-minimal/cordis.patch.yml", "process.env.DSH_SYSTEM_PROMPT");
        capabilities.sdk_app &= artifact_contains(root, "packages/bundle/sdk-app/cordis.patch.yml", "process.env.DSH_MAX_TOKENS_AS_SUCCESS");
        capabilities.web &= web_cli_supported;
    }
    Ok(PreferenceCapabilityEvidence { version: version.to_owned(), profile: profile.to_owned(), bundles, capabilities })
}

pub(crate) fn validate_launch(spec: &HarnessLaunchSpec, p: &HarnessPreferencesPayload, root: Option<&Path>) -> io::Result<()> {
    crate::runtime_patches::validate(p)?;
    if p.port.is_none() && p.open_browser.is_none() && !p.patches.as_ref().is_some_and(|v| !v.is_empty()) { return Ok(()); }
    let managed = root.is_some_and(|root| {
        let raw = if spec.mode == nexus_protocol::HarnessLaunchMode::Node {
            spec.args.first().map(String::as_str)
        } else { spec.program.to_str() };
        let Some(raw) = raw else { return false; };
        let rendered = raw.replace("{release_root}", &root.to_string_lossy());
        let entry = Path::new(&rendered);
        let resolved = if entry.is_absolute() { entry.to_owned() } else {
            let Some(cwd) = &spec.working_dir else { return false; };
            std::path::PathBuf::from(cwd.to_string_lossy().replace("{release_root}", &root.to_string_lossy())).join(entry)
        };
        fs::canonicalize(&resolved).ok().zip(fs::canonicalize(root.join("apps/cli/lib/bin.js")).ok())
            .is_some_and(|(entry, expected)| entry == expected)
    });
    if !managed {
        return Err(unsupported("Harness port, browser and patch overrides require the managed Harness CLI. Clear these settings for a custom command."));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_web_contract_fixture(root: &Path) {
    for (file, text) in [
        ("packages/bundle/web-app/src/startup.ts", ".option('--port <port>' .option('--no-open' options.port !== undefined port: Number(options.port) parseCmdline(ctx, program)"),
        ("packages/bundle/web-app/cordis.patch.yml", "ctx.webStartup.port ?? 3080 process.env.DSH_TOOLS_MODE"),
        ("packages/util/home-paths/src/index.ts", "env[DSH_HOME_ENV]"),
        ("packages/boot/app-boot/src/profile-context.ts", "resolveTelemetryPatch"),
    ] {
        let path = root.join(file); fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, text).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("nexus-capabilities-{}-{}", std::process::id(), nexus_core::unix_time_nanos_for_update()));
            fs::create_dir_all(root.join("slot")).unwrap();
            fs::write(root.join("slot/package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
            for (file, text) in [
                ("packages/bundle/web-app/src/startup.ts", ".option('--port <port>' .option('--no-open' options.port !== undefined port: Number(options.port) parseCmdline(ctx, program)"),
                ("packages/bundle/web-app/cordis.patch.yml", "ctx.webStartup.port ?? 3080 process.env.DSH_TOOLS_MODE"),
                ("packages/bundle/headless/cordis.patch.yml", "process.env.DSH_TOOLS_MODE"),
                ("packages/bundle/sdk-minimal/cordis.patch.yml", "process.env.DSH_CONTEXT_WINDOW process.env.DSH_SYSTEM_PROMPT"),
                ("packages/bundle/sdk-app/cordis.patch.yml", "process.env.DSH_MAX_TOKENS_AS_SUCCESS"),
            ] {
                let file = root.join("slot").join(file);
                fs::create_dir_all(file.parent().unwrap()).unwrap(); fs::write(file, text).unwrap();
            }
            Self(root)
        }
        fn profile(&self, name: &str, bundle: &str) {
            let dir = self.0.join("home/profiles").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("package.json"), serde_json::to_vec(&serde_json::json!({"dependencies":{},"dsh":{"profile":{"bundles":[bundle]}}})).unwrap()).unwrap();
        }
        fn resolve(&self, name: &str, p: &HarnessPreferencesPayload) -> io::Result<HarnessProfileCapabilities> {
            resolve(Some(&self.0.join("slot")), &self.0.join("home"), name, p)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[test]
    fn custom_web_bundle_applies_flags_without_renaming_profile() {
        let f = Fixture::new(); f.profile("work", "@deepseek-ai/dsh-web-app");
        let p = HarnessPreferencesPayload { port: Some(0), open_browser: Some(false), tools_mode: Some("ptc".into()), ..Default::default() };
        let capabilities = f.resolve("work", &p).unwrap();
        let mut spec = HarnessLaunchSpec::new("node".into());
        spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        spec.args = vec!["{release_root}/apps/cli/lib/bin.js".into(), "--profile".into(), "work".into()];
        nexus_core::apply_harness_preferences(&mut spec, &p, &capabilities);
        assert_eq!(spec.args, ["{release_root}/apps/cli/lib/bin.js", "--profile", "work", "--port", "0", "--no-open"]);
        assert!(spec.readiness_url.is_none());
        assert_eq!(crate::compatibility::source_profile(&f.0.join("home"), "work").unwrap(), "work");
        let evidence = inspect(&f.0.join("slot"), &f.0.join("home"), "work").unwrap();
        assert_eq!(evidence.version, VERIFIED_VERSION); assert_eq!(evidence.profile, "work");
    }

    #[test]
    fn actual_effective_bundle_and_first_use_template_control_capabilities() {
        let f = Fixture::new();
        let p = HarnessPreferencesPayload { port: Some(0), open_browser: Some(false), ..Default::default() };
        assert!(f.resolve("web", &p).unwrap().web);
        assert!(!f.0.join("home").exists(), "capability inspection must not initialize data");
        f.profile("web", "@deepseek-ai/dsh-headless");
        assert!(f.resolve("web", &p).unwrap_err().to_string().contains("port, open_browser"));
        f.profile("derived", "@deepseek-ai/dsh-headless");
        assert!(f.resolve("derived", &p).is_err(), "a compatibility projection without the Web bundle cannot retain Web flags");
    }

    #[test]
    fn unknown_versions_inherit_only_when_overrides_are_empty() {
        let f = Fixture::new();
        fs::write(f.0.join("slot/package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.3"}"#).unwrap();
        assert!(f.resolve("web", &HarnessPreferencesPayload::default()).is_ok());
        assert!(f.resolve("web", &HarnessPreferencesPayload { home: Some(" ".into()), ..Default::default() }).is_ok());
        assert!(f.resolve("web", &HarnessPreferencesPayload { telemetry_disabled: Some(false), ..Default::default() }).unwrap_err().to_string().contains("unverified"));
        fs::remove_file(f.0.join("slot/packages/bundle/web-app/src/startup.ts")).unwrap();
        assert!(f.resolve("web", &HarnessPreferencesPayload { port: Some(0), ..Default::default() }).is_err());
    }

    #[test]
    fn web_port_zero_uses_artifact_capabilities_on_new_releases() {
        let f = Fixture::new();
        let bundle = f.0.join("slot/packages/bundle/web-app");
        fs::create_dir_all(bundle.join("src")).unwrap();
        fs::write(bundle.join("src/startup.ts"), ".option('--port <port>' .option('--no-open' options.port !== undefined port: Number(options.port) parseCmdline(ctx, program)").unwrap();
        fs::write(bundle.join("cordis.patch.yml"), "port: !!js ctx.webStartup.port ?? 3080").unwrap();
        for version in ["0.1.6-alpha.2", "0.1.7-alpha.1", "9.0.0"] {
            fs::write(f.0.join("slot/package.json"), format!(r#"{{"name":"@deepseek-ai/dsh-root","version":"{version}"}}"#)).unwrap();
            let preferences = HarnessPreferencesPayload { port: Some(0), open_browser: Some(false), ..Default::default() };
            let capabilities = f.resolve("web", &preferences).unwrap();
            let mut spec = HarnessLaunchSpec::new("node".into());
            spec.mode = nexus_protocol::HarnessLaunchMode::Node;
            spec.args = vec!["{release_root}/apps/cli/lib/bin.js", "--profile", "web", "--port", "3851"].into_iter().map(str::to_owned).collect();
            nexus_core::apply_harness_preferences(&mut spec, &preferences, &capabilities);
            assert!(spec.args.windows(2).any(|args| args == ["--port", "0"]));
            assert!(!spec.args.contains(&"3851".to_owned()));
            assert!(spec.readiness_url.is_none());
        }
        fs::write(bundle.join("cordis.patch.yml"), "port: 3080").unwrap();
        assert!(f.resolve("web", &HarnessPreferencesPayload { port: Some(0), ..Default::default() }).is_err());
    }

    #[test]
    fn custom_sdk_bundles_receive_only_their_supported_environment() {
        let f = Fixture::new(); f.profile("minimal", "@deepseek-ai/dsh-sdk-minimal");
        let p = HarnessPreferencesPayload { context_window: Some(1024), system_prompt: Some("test prompt".into()), ..Default::default() };
        let env = nexus_core::harness_preferences_environment(&p, &f.resolve("minimal", &p).unwrap());
        assert_eq!(env.len(), 2);
        f.profile("sdk-work", "@deepseek-ai/dsh-sdk-app");
        assert!(f.resolve("sdk-work", &p).is_err());
        let p = HarnessPreferencesPayload { max_tokens_as_success: Some(false), ..Default::default() };
        assert_eq!(nexus_core::harness_preferences_environment(&p, &f.resolve("sdk-work", &p).unwrap()), vec![("DSH_MAX_TOKENS_AS_SUCCESS".into(), "false".into())]);
    }

    #[test]
    #[ignore = "requires NEXUS_SETTINGS_AUDIT_ROOT pointing to an unpacked official artifact"]
    fn official_artifact_settings_contract() {
        let root = std::path::PathBuf::from(std::env::var_os("NEXUS_SETTINGS_AUDIT_ROOT").expect("artifact root"));
        let f = Fixture::new();
        let common = HarnessPreferencesPayload {
            home: Some(f.0.join("home").display().to_string()),
            deepseek_base_url: Some("https://model.example".into()), search_base_url: Some("https://search.example".into()),
            search_provider: Some("test-search".into()), fetch_provider: Some("test-fetch".into()),
            agents_home: Some(f.0.join("agents").display().to_string()), bundled_skill_dir: Some(f.0.join("skills").display().to_string()),
            permission_mode: Some("read-only".into()), telemetry_disabled: Some(false), ..Default::default()
        };
        for profile in ["web", "headless", "sdk", "sdk-minimal"] {
            let mut p = common.clone();
            match profile {
                "web" => { p.port = Some(0); p.open_browser = Some(false); p.tools_mode = Some("ptc".into()); },
                "headless" => p.tools_mode = Some("native".into()),
                "sdk" => p.max_tokens_as_success = Some(false),
                _ => { p.context_window = Some(12345); p.system_prompt = Some("audit".into()); },
            }
            let caps = resolve(Some(&root), &f.0.join("home"), profile, &p).unwrap();
            let env = nexus_core::harness_preferences_environment(&p, &caps);
            assert!(env.iter().any(|(key, value)| key == "DSH_TELEMETRY_DISABLED" && value.is_empty()));
            assert!(env.iter().any(|(key, value)| key == "DSH_PERMISSION_MODE" && value == "read-only"));
        }
        assert!(resolve(Some(&root), &f.0.join("home"), "web", &HarnessPreferencesPayload {
            context_window: Some(12345), ..Default::default()
        }).is_err(), "SDK-only settings must not be silently accepted for Web");
    }

    #[test]
    fn custom_command_cannot_silently_discard_cli_overrides() {
        let spec = HarnessLaunchSpec::new("custom.exe".into());
        assert!(validate_launch(&spec, &HarnessPreferencesPayload::default(), None).is_ok());
        assert!(validate_launch(&spec, &HarnessPreferencesPayload { port: Some(0), ..Default::default() }, None).is_err());
        let f = Fixture::new();
        let outside = f.0.join("custom/apps/cli/lib/bin.js");
        let actual = f.0.join("slot/apps/cli/lib/bin.js");
        fs::create_dir_all(outside.parent().unwrap()).unwrap(); fs::write(&outside, "custom").unwrap();
        fs::create_dir_all(actual.parent().unwrap()).unwrap(); fs::write(&actual, "managed").unwrap();
        let mut spec = HarnessLaunchSpec::new("node".into()); spec.mode = nexus_protocol::HarnessLaunchMode::Node;
        spec.args = vec![outside.to_string_lossy().into_owned()];
        let p = HarnessPreferencesPayload { port: Some(0), ..Default::default() };
        assert!(validate_launch(&spec, &p, Some(&f.0.join("slot"))).is_err());
        spec.args = vec!["{release_root}/apps/cli/lib/bin.js".into()];
        assert!(validate_launch(&spec, &p, Some(&f.0.join("slot"))).is_ok());
    }
}

#[cfg(test)]
mod settings_document_tests {
    use super::*;
    #[test]
    fn selected_artifact_controls_settings_path_without_migrating_data() {
        let base = std::env::temp_dir().join(format!("nexus-settings-{}", nexus_core::new_instance_id()));
        let source = base.join("packages/settings/settings/src");
        fs::create_dir_all(&source).unwrap();
        let home = base.join("home");
        assert!(settings_document(Some(&base), &home, "web").is_err());
        let legacy = base.join("packages/settings/settings-file/lib");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("index.js"), "get documentPath() { return 'settings.yaml' }").unwrap();
        assert_eq!(settings_document(Some(&base), &home, "desktop").unwrap(), home.join("settings.yaml"));
        let built = base.join("packages/settings/settings/lib");
        fs::create_dir_all(&built).unwrap();
        fs::write(built.join("index.js"), "get documentPath(){return this.ownerContext.configEditor.documentPath}").unwrap();
        assert_eq!(settings_document(Some(&base), &home, "desktop").unwrap(), home.join("profiles/desktop/cordis.patch.yml"));
        assert!(settings_document(None, &home, "web").is_err());
        assert!(settings_document(Some(&base), &home, "../escape").is_err());
        assert!(!home.exists());
        fs::remove_dir_all(base).unwrap();
    }
}
