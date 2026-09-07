//! Version and bundle evidence for Nexus-owned optional Harness settings.
use std::{fs, io::{self, Read}, path::Path};
use nexus_core::{HarnessProfileCapabilities, HarnessLaunchSpec};
use nexus_protocol::HarnessPreferencesPayload;

pub(crate) const VERIFIED_VERSION: &str = "0.1.2-rc.1";

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
    let preferences = nexus_core::normalize_harness_preferences(preferences.clone())?;
    if preferences == HarnessPreferencesPayload::default() { return Ok(HarnessProfileCapabilities::default()); }
    let root = root.ok_or_else(|| unsupported("Harness settings are unverified without an installed release. Clear explicit overrides to use the original configuration."))?;
    let evidence = inspect(root, home, profile)?;
    evidence.capabilities.validate_preferences(&preferences)?;
    Ok(evidence.capabilities)
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
    if manifest.get("name").and_then(|v| v.as_str()) != Some("@deepseek-ai/dsh-root")
        || manifest.get("version").and_then(|v| v.as_str()) != Some(VERIFIED_VERSION) {
        return Err(unsupported("Explicit Harness settings are unverified for this version. Clear overrides to inherit the original configuration; Nexus currently verifies 0.1.2-rc.1."));
    }
    let bundles = match crate::dsh::native_profile(home, profile) {
        Ok(inventory) => {
            for plugin in &inventory.plugins {
                if inventory.bundles.contains(&plugin.package) && matches!(plugin.package.as_str(),
                    "@deepseek-ai/dsh-web-app" | "@deepseek-ai/dsh-headless" | "@deepseek-ai/dsh-sdk-app" | "@deepseek-ai/dsh-sdk-minimal")
                    && plugin.version.as_deref().is_some_and(|version| version != VERIFIED_VERSION) {
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
    let capabilities = HarnessProfileCapabilities::from_bundles(&bundles);
    Ok(PreferenceCapabilityEvidence { version: VERIFIED_VERSION.to_owned(), profile: profile.to_owned(), bundles, capabilities })
}

pub(crate) fn validate_launch(spec: &HarnessLaunchSpec, p: &HarnessPreferencesPayload, root: Option<&Path>) -> io::Result<()> {
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
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("nexus-capabilities-{}-{}", std::process::id(), nexus_core::unix_time_nanos_for_update()));
            fs::create_dir_all(root.join("slot")).unwrap();
            fs::write(root.join("slot/package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
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
