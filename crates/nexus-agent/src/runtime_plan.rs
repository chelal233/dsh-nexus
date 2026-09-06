//! Read-only planning for one registered release's runtime requirements.

use std::io;

use nexus_core::{
    runtime_requirements::load_runtime_requirements, ConfigStore, ReleaseStore, RuntimeConfig,
};
use nexus_protocol::{
    encode_json, RuntimeInstallMode, RuntimeListResponse, RuntimeOwnership, RuntimePlanAction,
    RuntimePlanActionKind, RuntimePlanRequest, RuntimePlanResponse, RuntimePlanTool,
    RuntimePlanToolState, RuntimeRequirements, RuntimeSource, RuntimeToolStatus, API_VERSION,
};

/// Accepted deviation between the release's exact pnpm requirement and the
/// bundled pnpm's version when both share the same major.
pub(crate) const WARNING_BUNDLED_PNPM_MAJOR_SKEW: &str = "bundled_pnpm_major_skew";

pub(crate) fn assemble_runtime_plan(
    release_id: String,
    source: RuntimeSource,
    mode: RuntimeInstallMode,
    requirements: RuntimeRequirements,
    observed: RuntimeListResponse,
    config: Option<&RuntimeConfig>,
) -> io::Result<RuntimePlanResponse> {
    let mut tools = Vec::with_capacity(3);
    let mut suggested_actions = Vec::with_capacity(3);
    for name in ["git", "node", "pnpm"] {
        let observed = observed
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .cloned()
            .unwrap_or_else(|| RuntimeToolStatus {
                name: name.to_owned(),
                available: false,
                version: None,
                source: None,
                path: None,
                reason: Some("not_found".to_owned()),
            });
        let pinned = config.and_then(|config| config.pin(name));
        let requirements_for_tool = match name {
            "git" => vec!["available".to_owned()],
            "node" => requirements
                .node
                .iter()
                .map(|requirement| requirement.range.clone())
                .collect(),
            "pnpm" => vec![requirements.package_manager.spec.clone()],
            _ => unreachable!(),
        };
        let bundled = observed.source.as_deref() == Some("bundled");
        let mut warning = None;
        let compatible = match (name, observed.version.as_deref()) {
            ("git", Some(_)) => true,
            ("node", Some(version)) => {
                requirements
                    .node
                    .iter()
                    .try_fold(true, |matches, requirement| {
                        Ok::<_, io::Error>(
                            matches
                                && nexus_core::runtime_requirements::node_version_satisfies(
                                    &requirement.range,
                                    version,
                                )?,
                        )
                    })?
            }
            ("pnpm", Some(version)) => {
                let required = requirements.package_manager.version.clone();
                if nexus_core::runtime_requirements::package_manager_version_matches(
                    &required, version,
                )? {
                    true
                } else if bundled
                    && nexus_core::runtime_requirements::package_manager_same_major(
                        &required, version,
                    )?
                {
                    // Bundled pnpm policy: same major is accepted with a
                    // visible warning; the bundled copy is refreshed with
                    // each Nexus release.
                    warning = Some(WARNING_BUNDLED_PNPM_MAJOR_SKEW.to_owned());
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        let state = if observed.available && compatible {
            RuntimePlanToolState::Reusable
        } else if observed.available {
            RuntimePlanToolState::Incompatible
        } else if matches!(
            observed.reason.as_deref(),
            None | Some("not_found") | Some("configured_path_missing")
        ) {
            RuntimePlanToolState::Missing
        } else {
            RuntimePlanToolState::Unverifiable
        };
        let ownership =
            pinned
                .map(|pin| pin.ownership)
                .or_else(|| match observed.source.as_deref() {
                    Some("system") => Some(RuntimeOwnership::System),
                    Some("nexus") => Some(RuntimeOwnership::Nexus),
                    Some("bundled") => Some(RuntimeOwnership::Bundled),
                    _ => None,
                });
        let reason = match state {
            RuntimePlanToolState::Reusable => None,
            RuntimePlanToolState::Incompatible => Some("version_incompatible".to_owned()),
            RuntimePlanToolState::Missing | RuntimePlanToolState::Unverifiable => observed
                .reason
                .clone()
                .or_else(|| Some("not_found".to_owned())),
        };
        let action = match state {
            RuntimePlanToolState::Reusable if pinned.is_some() => RuntimePlanActionKind::UsePinned,
            RuntimePlanToolState::Reusable => RuntimePlanActionKind::UseExisting,
            _ if name == "git" => RuntimePlanActionKind::UseExisting,
            // Runtime provisioning by download is retired: node and pnpm come
            // from a pin, the system, or the bundled copy. Without any of
            // those the plan asks the user to configure paths explicitly.
            _ => RuntimePlanActionKind::ConfigureExternal,
        };
        let action_reason = if name == "git" && state != RuntimePlanToolState::Reusable {
            "nexus_embedded_git_only_external_cli_unavailable".to_owned()
        } else { match action {
            RuntimePlanActionKind::UsePinned => "configured_pin_verified".to_owned(),
            RuntimePlanActionKind::UseExisting if warning.is_some() => {
                "compatible_bundled_pnpm_major_skew".to_owned()
            }
            RuntimePlanActionKind::UseExisting => "compatible_existing_runtime".to_owned(),
            RuntimePlanActionKind::ConfigureExternal => reason
                .clone()
                .unwrap_or_else(|| "runtime_unresolvable_specify_paths".to_owned()),
            RuntimePlanActionKind::ProvisionPortable | RuntimePlanActionKind::InstallSystem => {
                reason
                    .clone()
                    .unwrap_or_else(|| "runtime_provisioning_required".to_owned())
            }
        }};
        tools.push(RuntimePlanTool {
            name: name.to_owned(),
            requirements: requirements_for_tool,
            state,
            version: observed.version,
            path: observed.path,
            ownership,
            reason,
            warning,
        });
        suggested_actions.push(RuntimePlanAction {
            tool: name.to_owned(),
            action,
            reason: action_reason,
        });
    }
    let mut response = RuntimePlanResponse {
        api_version: API_VERSION.to_owned(),
        plan_id: String::new(),
        release_id,
        source,
        mode,
        requirements,
        tools,
        suggested_actions,
    };
    response.plan_id = format!(
        "runtime-plan-v1:{}",
        String::from_utf8(
            encode_json(&response).map_err(|error| io::Error::other(error.to_string()))?
        )
        .map_err(|error| io::Error::other(error.to_string()))?
    );
    Ok(response)
}

pub(crate) async fn plan_registered_release(
    releases: &ReleaseStore,
    config_store: &ConfigStore,
    request: RuntimePlanRequest,
    runtime_request: &super::runtime::RuntimeRequestContext,
) -> io::Result<RuntimePlanResponse> {
    let release_path = releases.paths().release_pointers_file.clone();
    let owned_releases = releases.clone();
    let release_id = request.release_id.clone();
    let release_root = runtime_request
        .run_blocking_io(
            super::runtime::BlockingStage::ReleaseRoot,
            release_path,
            move || owned_releases.release_root(&release_id),
        )
        .await?;
    let requirements_path = release_root.clone();
    let requirements = runtime_request
        .run_blocking_io(
            super::runtime::BlockingStage::Requirements,
            requirements_path,
            move || load_runtime_requirements(&release_root),
        )
        .await?;
    let config_path = config_store.paths().config_file.clone();
    let owned_config_store = config_store.clone();
    let config = runtime_request
        .run_blocking_io(
            super::runtime::BlockingStage::ConfigFile,
            config_path,
            move || owned_config_store.load(),
        )
        .await?;
    let observed = super::runtime::observe_runtime_selection_until(
        config_store.paths(),
        config.runtime.as_ref(),
        runtime_request,
    )
    .await;
    assemble_runtime_plan(
        request.release_id,
        request.source,
        request.mode,
        requirements,
        observed,
        config.runtime.as_ref(),
    )
}

pub(crate) async fn plan_candidate_release(
    release_root: &std::path::Path,
    request: RuntimePlanRequest,
    config_store: &ConfigStore,
    runtime_request: &super::runtime::RuntimeRequestContext,
) -> io::Result<RuntimePlanResponse> {
    let release_root = std::fs::canonicalize(release_root)?;
    let downloads_root = std::fs::canonicalize(&config_store.paths().downloads_dir)?;
    if release_root == downloads_root || !nexus_core::is_within(&downloads_root, &release_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cold candidate must remain below Nexus downloads",
        ));
    }
    let requirements_root = release_root.clone();
    let requirements = runtime_request
        .run_blocking_io(
            super::runtime::BlockingStage::Requirements,
            release_root,
            move || load_runtime_requirements(&requirements_root),
        )
        .await?;
    let config_path = config_store.paths().config_file.clone();
    let owned_config_store = config_store.clone();
    let config = runtime_request
        .run_blocking_io(
            super::runtime::BlockingStage::ConfigFile,
            config_path,
            move || owned_config_store.load(),
        )
        .await?;
    let observed = super::runtime::observe_runtime_selection_until(
        config_store.paths(),
        config.runtime.as_ref(),
        runtime_request,
    )
    .await;
    assemble_runtime_plan(
        request.release_id,
        request.source,
        request.mode,
        requirements,
        observed,
        config.runtime.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use nexus_core::{
        ConfigStore, NexusConfigFile, NexusPaths, ReleaseStore, RuntimeConfig, RuntimePin,
    };
    use nexus_protocol::{
        RuntimeInstallMode, RuntimeListResponse, RuntimeNodeRequirement, RuntimeOwnership,
        RuntimePackageManagerRequirement, RuntimePlanActionKind, RuntimePlanToolState,
        RuntimeRequirements, RuntimeSource, RuntimeToolStatus,
    };

    use super::{assemble_runtime_plan, plan_registered_release};

    #[test]
    fn plan_fixture_is_deterministic_and_classifies_each_tool() {
        let requirements = RuntimeRequirements {
            node: vec![RuntimeNodeRequirement {
                manifest: "package.json".to_owned(),
                range: "^22.19.0 || >=24.0.0".to_owned(),
            }],
            package_manager: RuntimePackageManagerRequirement {
                manifest: "package.json".to_owned(),
                spec: "pnpm@11.7.0".to_owned(),
                name: "pnpm".to_owned(),
                version: "11.7.0".to_owned(),
            },
        };
        let observed = RuntimeListResponse::new(vec![
            RuntimeToolStatus {
                name: "git".to_owned(),
                available: false,
                version: None,
                source: None,
                path: None,
                reason: Some("not_found".to_owned()),
            },
            RuntimeToolStatus {
                name: "node".to_owned(),
                available: true,
                version: Some("23.0.0".to_owned()),
                source: Some("system".to_owned()),
                path: Some("C:\\runtime\\node.exe".to_owned()),
                reason: None,
            },
            RuntimeToolStatus {
                name: "pnpm".to_owned(),
                available: true,
                version: Some("11.7.0".to_owned()),
                source: Some("nexus".to_owned()),
                path: Some("C:\\runtime\\pnpm.cjs".to_owned()),
                reason: None,
            },
        ]);
        let config = RuntimeConfig {
            pnpm: Some(RuntimePin {
                path: "C:\\runtime\\pnpm.cjs".into(),
                ownership: RuntimeOwnership::Nexus,
            }),
            ..RuntimeConfig::default()
        };
        let first = assemble_runtime_plan(
            "release-a".to_owned(),
            RuntimeSource::Official,
            RuntimeInstallMode::Portable,
            requirements.clone(),
            observed.clone(),
            Some(&config),
        )
        .expect("plan assembles");
        let second = assemble_runtime_plan(
            "release-a".to_owned(),
            RuntimeSource::Official,
            RuntimeInstallMode::Portable,
            requirements,
            observed,
            Some(&config),
        )
        .expect("plan reassembles");

        assert_eq!(first, second);
        let empty = assemble_runtime_plan(
            "first-install".to_owned(), RuntimeSource::Official,
            RuntimeInstallMode::Portable, first.requirements.clone(),
            RuntimeListResponse::new(Vec::new()), None,
        ).expect("an empty machine still produces a runtime supply plan");
        assert!(empty.tools.iter().all(|tool| tool.state == RuntimePlanToolState::Missing));
        assert_eq!(empty.suggested_actions.iter().find(|action| action.tool == "git").unwrap().action,
            RuntimePlanActionKind::UseExisting);
        assert!(empty.suggested_actions.iter().filter(|action| action.tool != "git")
            .all(|action| action.action == RuntimePlanActionKind::ConfigureExternal));
        assert_eq!(empty.requirements, first.requirements);
        assert!(first.plan_id.starts_with("runtime-plan-v1:"));
        assert_eq!(first.tools[0].state, RuntimePlanToolState::Missing);
        assert_eq!(first.tools[1].state, RuntimePlanToolState::Incompatible);
        assert_eq!(first.tools[2].state, RuntimePlanToolState::Reusable);
        assert_eq!(
            first.suggested_actions[2].action,
            RuntimePlanActionKind::UsePinned
        );
    }

    #[test]
    fn bundled_pnpm_same_major_is_reusable_with_warning_and_skew_action_reason() {
        let requirements = RuntimeRequirements {
            node: vec![RuntimeNodeRequirement {
                manifest: "package.json".to_owned(),
                range: ">=20.0.0".to_owned(),
            }],
            package_manager: RuntimePackageManagerRequirement {
                manifest: "package.json".to_owned(),
                spec: "pnpm@11.7.0".to_owned(),
                name: "pnpm".to_owned(),
                version: "11.7.0".to_owned(),
            },
        };
        let observed = RuntimeListResponse::new(vec![RuntimeToolStatus {
            name: "pnpm".to_owned(),
            available: true,
            version: Some("11.9.4".to_owned()),
            source: Some("bundled".to_owned()),
            path: Some("C:\\install\\runtime\\pnpm\\pnpm.cjs".to_owned()),
            reason: None,
        }]);
        let plan = assemble_runtime_plan(
            "release-a".to_owned(),
            RuntimeSource::Official,
            RuntimeInstallMode::Portable,
            requirements.clone(),
            observed,
            None,
        )
        .expect("plan assembles");
        let pnpm = plan.tools.iter().find(|tool| tool.name == "pnpm").unwrap();
        assert_eq!(pnpm.state, RuntimePlanToolState::Reusable);
        assert_eq!(pnpm.ownership, Some(RuntimeOwnership::Bundled));
        assert_eq!(
            pnpm.warning.as_deref(),
            Some(super::WARNING_BUNDLED_PNPM_MAJOR_SKEW)
        );
        let action = plan
            .suggested_actions
            .iter()
            .find(|action| action.tool == "pnpm")
            .unwrap();
        assert_eq!(action.action, RuntimePlanActionKind::UseExisting);
        assert_eq!(action.reason, "compatible_bundled_pnpm_major_skew");

        let cross_major = RuntimeListResponse::new(vec![RuntimeToolStatus {
            name: "pnpm".to_owned(),
            available: true,
            version: Some("12.0.0".to_owned()),
            source: Some("bundled".to_owned()),
            path: Some("C:\\install\\runtime\\pnpm\\pnpm.cjs".to_owned()),
            reason: None,
        }]);
        let plan = assemble_runtime_plan(
            "release-a".to_owned(),
            RuntimeSource::Official,
            RuntimeInstallMode::Portable,
            requirements,
            cross_major,
            None,
        )
        .expect("plan assembles");
        let pnpm = plan.tools.iter().find(|tool| tool.name == "pnpm").unwrap();
        assert_eq!(pnpm.state, RuntimePlanToolState::Incompatible);
        assert_eq!(pnpm.warning, None);
        let action = plan
            .suggested_actions
            .iter()
            .find(|action| action.tool == "pnpm")
            .unwrap();
        assert_eq!(action.action, RuntimePlanActionKind::ConfigureExternal);
    }

    #[tokio::test]
    async fn registered_release_is_the_only_manifest_source() {
        let root = std::env::temp_dir().join(format!(
            "nexus-runtime-plan-release-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is after epoch")
                .as_nanos()
        ));
        let paths = NexusPaths::from_root(root.clone());
        let releases = ReleaseStore::new(paths.clone());
        releases
            .register("release-a", "tag-a", None, None)
            .expect("release registers");
        let release_root = releases
            .release_root("release-a")
            .expect("release resolves");
        fs::create_dir_all(release_root.join("apps/cli")).expect("CLI directory creates");
        fs::write(
            release_root.join("package.json"),
            br#"{"engines":{"node":"^22.19.0 || >=24.0.0"},"packageManager":"pnpm@11.7.0"}"#,
        )
        .expect("root package writes");
        fs::write(
            release_root.join("apps/cli/package.json"),
            br#"{"engines":{"node":">=22.19.0"}}"#,
        )
        .expect("CLI package writes");
        let missing = paths.runtimes_dir.join("missing");
        let config = ConfigStore::new(paths.clone());
        config
            .write(&NexusConfigFile {
                runtime: Some(RuntimeConfig {
                    node: Some(RuntimePin {
                        path: missing.join("node.exe"),
                        ownership: RuntimeOwnership::Nexus,
                    }),
                    pnpm: Some(RuntimePin {
                        path: missing.join("pnpm.cjs"),
                        ownership: RuntimeOwnership::Nexus,
                    }),
                    git: Some(RuntimePin {
                        path: missing.join("git.exe"),
                        ownership: RuntimeOwnership::Nexus,
                    }),
                    ..RuntimeConfig::default()
                }),
                ..NexusConfigFile::default()
            })
            .expect("runtime config writes");

        let plan = plan_registered_release(
            &releases,
            &config,
            nexus_protocol::RuntimePlanRequest {
                release_id: "release-a".to_owned(),
                source: RuntimeSource::Official,
                mode: RuntimeInstallMode::Portable,
            },
            &super::super::runtime::RuntimeRequestContext::production(),
        )
        .await
        .expect("registered release plans");
        assert_eq!(plan.requirements.package_manager.version, "11.7.0");
        assert!(plan
            .tools
            .iter()
            .all(|tool| tool.state == RuntimePlanToolState::Missing));
        let git = plan.suggested_actions.iter().find(|tool| tool.tool == "git").unwrap();
        assert_eq!(git.action, RuntimePlanActionKind::UseExisting);
        assert_eq!(git.reason, "nexus_embedded_git_only_external_cli_unavailable");
        let error = plan_registered_release(
            &releases,
            &config,
            nexus_protocol::RuntimePlanRequest {
                release_id: "C:/outside/package.json".to_owned(),
                source: RuntimeSource::Official,
                mode: RuntimeInstallMode::Portable,
            },
            &super::super::runtime::RuntimeRequestContext::production(),
        )
        .await
        .expect_err("unregistered release cannot select a manifest");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
        ));
        let _ = fs::remove_dir_all(root);
    }
}
