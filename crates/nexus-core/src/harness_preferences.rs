//! Optional Harness overrides. Empty values inherit upstream configuration.
use std::{ffi::OsString, io, path::{Component, Path}};
use nexus_protocol::HarnessPreferencesPayload;
use crate::{ConfigStore, HarnessLaunchSpec, NexusPaths};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub fn normalize_harness_preferences(mut p: HarnessPreferencesPayload) -> io::Result<HarnessPreferencesPayload> {
    for value in [&mut p.home, &mut p.deepseek_base_url, &mut p.search_base_url,
        &mut p.search_provider, &mut p.fetch_provider, &mut p.agents_home,
        &mut p.bundled_skill_dir, &mut p.permission_mode, &mut p.tools_mode,
        &mut p.system_prompt] {
        *value = value.take().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        if value.as_ref().is_some_and(|s| s.len() > 32768 || s.contains('\0')) {
            return Err(invalid("Harness setting is too long or contains NUL"));
        }
    }
    p.patches = p.patches.take().map(|items| items.into_iter().map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()).collect::<Vec<_>>()).filter(|items| !items.is_empty());
    if p.patches.as_ref().is_some_and(|items| items.len() > 32) {
        return Err(invalid("At most 32 additional patches are supported"));
    }
    for (name, value) in [("home", &p.home), ("agents_home", &p.agents_home), ("bundled_skill_dir", &p.bundled_skill_dir)] {
        if let Some(value) = value { validate_path(name, value)?; }
    }
    for path in p.patches.iter().flatten() { validate_path("patch", path)?; }
    for (name, value) in [("deepseek_base_url", &p.deepseek_base_url), ("search_base_url", &p.search_base_url)] {
        if let Some(value) = value {
            let authority = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://"))
                .ok_or_else(|| invalid(format!("{name} requires an HTTP(S) URL")))?;
            if authority.split('/').next().unwrap_or("").is_empty()
                || value.chars().any(char::is_whitespace) || value.contains(['@', '?', '#', '\\']) {
                return Err(invalid(format!("{name} must have a host and no credentials, query or fragment")));
            }
        }
    }
    for (name, value) in [("search_provider", &p.search_provider), ("fetch_provider", &p.fetch_provider)] {
        if value.as_ref().is_some_and(|s| s.len() > 128 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))) {
            return Err(invalid(format!("{name} must be a provider identifier")));
        }
    }
    if p.permission_mode.as_deref().is_some_and(|s| !["read-only", "workspace-write", "danger-full-access"].contains(&s)) {
        return Err(invalid("Unknown Harness permission mode"));
    }
    if p.tools_mode.as_deref().is_some_and(|s| !["native", "ptc", "both"].contains(&s)) {
        return Err(invalid("Unknown Harness tools mode"));
    }
    if p.context_window == Some(0) { return Err(invalid("Context window must be greater than zero")); }
    Ok(p)
}

fn validate_path(name: &str, value: &str) -> io::Result<()> {
    let path = Path::new(value);
    if value.len() > 32768 || value.contains(['\0', '\r', '\n']) || !path.is_absolute()
        || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(invalid(format!("{name} must be an absolute path without parent traversal")));
    }
    Ok(())
}

pub fn load_harness_preferences(paths: &NexusPaths) -> io::Result<HarnessPreferencesPayload> {
    normalize_harness_preferences(ConfigStore::new(paths.clone()).load()?.harness_preferences.unwrap_or_default())
}

/// Capability evidence is resolved from the installed release and actual bundle list.
/// This is independent of the profile's name or compatibility source identity.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct HarnessProfileCapabilities {
    pub web: bool,
    pub tools_mode: bool,
    pub sdk_minimal: bool,
    pub sdk_app: bool,
}

impl HarnessProfileCapabilities {
    pub fn from_bundles(bundles: &[String]) -> Self {
        let has = |name: &str| bundles.iter().any(|bundle| bundle == name);
        let web = has("@deepseek-ai/dsh-web-app");
        Self { web, tools_mode: web || has("@deepseek-ai/dsh-headless"),
            sdk_minimal: has("@deepseek-ai/dsh-sdk-minimal"), sdk_app: has("@deepseek-ai/dsh-sdk-app") }
    }

    pub fn validate_preferences(&self, p: &HarnessPreferencesPayload) -> io::Result<()> {
        let mut unsupported = Vec::new();
        if !self.web {
            if p.port.is_some() { unsupported.push("port"); }
            if p.open_browser.is_some() { unsupported.push("open_browser"); }
        }
        if !self.tools_mode && p.tools_mode.is_some() { unsupported.push("tools_mode"); }
        if !self.sdk_minimal {
            if p.context_window.is_some() { unsupported.push("context_window"); }
            if p.system_prompt.is_some() { unsupported.push("system_prompt"); }
        }
        if !self.sdk_app && p.max_tokens_as_success.is_some() { unsupported.push("max_tokens_as_success"); }
        if unsupported.is_empty() { Ok(()) } else {
            Err(invalid(format!("Harness profile does not support these explicit settings: {}. Clear them to inherit the profile's configuration.", unsupported.join(", "))))
        }
    }
}

/// Scope overrides to the child; never mutate the Agent's process environment.
pub fn harness_preferences_environment(p: &HarnessPreferencesPayload, capabilities: &HarnessProfileCapabilities) -> Vec<(OsString, OsString)> {
    let mut result = Vec::new();
    for (key, value) in [
        ("DSH_HOME", p.home.as_deref()),
        ("DEEPSEEK_BASE_URL", p.deepseek_base_url.as_deref()),
        ("DEEPSEEK_SEARCH_BASE_URL", p.search_base_url.as_deref()),
        ("DSH_WEB_SEARCH_PROVIDER", p.search_provider.as_deref()),
        ("DSH_WEB_FETCH_PROVIDER", p.fetch_provider.as_deref()),
        ("DSH_AGENTS_HOME", p.agents_home.as_deref()),
        ("DSH_BUNDLED_SKILL_DIR", p.bundled_skill_dir.as_deref()),
        ("DSH_PERMISSION_MODE", p.permission_mode.as_deref()),
    ] { if let Some(value) = value { result.push((key.into(), value.into())); } }
    if let Some(disabled) = p.telemetry_disabled {
        // Upstream treats *any* nonempty string, including "false", as disabled.
        result.push(("DSH_TELEMETRY_DISABLED".into(), if disabled { "1" } else { "" }.into()));
    }
    if capabilities.tools_mode {
        if let Some(value) = &p.tools_mode { result.push(("DSH_TOOLS_MODE".into(), value.into())); }
    }
    if capabilities.sdk_minimal {
        if let Some(value) = p.context_window { result.push(("DSH_CONTEXT_WINDOW".into(), value.to_string().into())); }
        if let Some(value) = &p.system_prompt { result.push(("DSH_SYSTEM_PROMPT".into(), value.into())); }
    }
    if capabilities.sdk_app {
        if let Some(value) = p.max_tokens_as_success { result.push(("DSH_MAX_TOKENS_AS_SUCCESS".into(), value.to_string().into())); }
    }
    result
}

/// Apply only to a recognized managed CLI entry, before rendering placeholders.
/// Stored arguments and launch working directory remain untouched.
pub fn harness_preferences_cli_supported(spec: &HarnessLaunchSpec) -> bool {
    let node = spec.mode == nexus_protocol::HarnessLaunchMode::Node;
    let entry = if node { spec.args.first().map(String::as_str).unwrap_or("") }
        else { spec.program.to_str().unwrap_or("") };
    let entry = entry.replace('\\', "/").to_ascii_lowercase();
    entry.ends_with("/apps/cli/lib/bin.js") || entry.contains("deepseek-harness") || entry.contains("dsh-harness")
}

pub fn apply_harness_preferences(spec: &mut HarnessLaunchSpec, p: &HarnessPreferencesPayload, capabilities: &HarnessProfileCapabilities) {
    if !harness_preferences_cli_supported(spec) { return; }
    let node = spec.mode == nexus_protocol::HarnessLaunchMode::Node;
    let start = usize::from(node);
    let mut args = spec.args[..start.min(spec.args.len())].to_vec();
    if let Some(patches) = &p.patches {
        for patch in patches { args.extend(["--patch".to_owned(), patch.clone()]); }
    }
    let web = capabilities.web;
    let mut i = start;
    while i < spec.args.len() {
        let arg = &spec.args[i];
        if web && p.port.is_some() && arg == "--port" { i += 2; continue; }
        if web && p.port.is_some() && arg.starts_with("--port=") { i += 1; continue; }
        if web && p.open_browser.is_some() && arg == "--no-open" { i += 1; continue; }
        args.push(arg.clone()); i += 1;
    }
    if web {
        if let Some(port) = p.port {
            args.extend(["--port".to_owned(), port.to_string()]);
            // Port zero has no static endpoint. The Workbench reads the emitted URL.
            spec.readiness_url = if port == 0 { None } else { Some(format!("tcp://127.0.0.1:{port}")) };
        }
        if p.open_browser == Some(false) { args.push("--no-open".into()); }
    }
    spec.args = args;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capabilities(name: &str) -> HarnessProfileCapabilities {
        let bundle = match name { "web" => "web-app", "headless" => "headless", "sdk" => "sdk-app", "sdk-minimal" => "sdk-minimal", _ => "" };
        HarnessProfileCapabilities::from_bundles(&[format!("@deepseek-ai/dsh-{bundle}")])
    }
    #[test]
    fn empty_settings_inherit_but_false_and_zero_are_effective() {
        let p = normalize_harness_preferences(HarnessPreferencesPayload {
            home: Some("  ".into()), patches: Some(vec![" ".into()]),
            port: Some(0), open_browser: Some(false), telemetry_disabled: Some(false),
            ..Default::default()
        }).unwrap();
        assert!(p.home.is_none() && p.patches.is_none());
        assert_eq!(p.port, Some(0));
        assert_eq!(p.open_browser, Some(false));
        assert_eq!(harness_preferences_environment(&p, &capabilities("web")), vec![("DSH_TELEMETRY_DISABLED".into(), "".into())]);
        assert!(harness_preferences_environment(&HarnessPreferencesPayload::default(), &capabilities("web")).is_empty());
    }
    #[test]
    fn launch_overlay_preserves_base_and_keeps_root_patches_before_app_flags() {
        let mut base = HarnessLaunchSpec::new("node".into());
        base.mode = nexus_protocol::HarnessLaunchMode::Node;
        base.args = vec!["{release_root}/apps/cli/lib/bin.js", "--profile", "{profile}", "--port", "3080", "--no-open"].into_iter().map(str::to_owned).collect();
        base.working_dir = Some("{release_root}".into());
        let before = base.clone();
        let mut launch = base.clone();
        apply_harness_preferences(&mut launch, &HarnessPreferencesPayload {
            port: Some(0), open_browser: Some(true), patches: Some(vec!["patch.yml".into()]), ..Default::default()
        }, &capabilities("web"));
        assert_eq!(launch.args, vec!["{release_root}/apps/cli/lib/bin.js", "--patch", "patch.yml", "--profile", "{profile}", "--port", "0"]);
        assert!(launch.readiness_url.is_none());
        assert_eq!(launch.working_dir, before.working_dir);
        assert_eq!(base, before);
        apply_harness_preferences(&mut base, &HarnessPreferencesPayload::default(), &capabilities("web"));
        assert_eq!(base, before);
    }
    #[test]
    fn sdk_settings_do_not_leak_into_web_profiles() {
        let p = HarnessPreferencesPayload { context_window: Some(123), system_prompt: Some("test".into()),
            max_tokens_as_success: Some(false), tools_mode: Some("both".into()), ..Default::default() };
        let web = harness_preferences_environment(&p, &capabilities("web"));
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].0, "DSH_TOOLS_MODE");
        assert_eq!(harness_preferences_environment(&p, &capabilities("sdk-minimal")).len(), 2);
        assert_eq!(harness_preferences_environment(&p, &capabilities("sdk")), vec![("DSH_MAX_TOKENS_AS_SUCCESS".into(), "false".into())]);
    }
    #[test]
    fn invalid_overrides_are_rejected_without_io() {
        for home in ["relative", "../data"] {
            assert!(normalize_harness_preferences(HarnessPreferencesPayload { home: Some(home.into()), ..Default::default() }).is_err());
        }
        assert!(normalize_harness_preferences(HarnessPreferencesPayload { deepseek_base_url: Some("https://user:key@example.com".into()), ..Default::default() }).is_err());
    }
}
