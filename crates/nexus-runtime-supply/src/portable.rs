use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
};

use nexus_core::{runtime_requirements::node_version_satisfies, RuntimeConfig, RuntimePin};
use nexus_protocol::{
    RuntimeInstallMode, RuntimeOwnership, RuntimePlanResponse, RuntimePlanTool,
    RuntimePlanToolState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::{
    archive::{remove_owned_staging, ArchiveLimits},
    extract_node_zip, extract_pnpm_tarball,
    source::{
        fetch_file, node_filename, resolve_node_artifact, resolve_pnpm_artifact,
        resolve_pnpm_script, select_node_version, sha256_hex, validate_exact_version,
        verify_file_digest, DownloadClient, SourcePolicy, MAX_METADATA_BYTES,
        MAX_NODE_ARTIFACT_BYTES, MAX_PNPM_ARTIFACT_BYTES,
    },
    system::{
        classify_system_install, default_system_node_path, default_system_pnpm_path, probe_request,
        verify_probe, ProcessRunner, SystemInstallResult, SystemInstallSpec, TypedProcessRequest,
    },
    ArtifactIdentity, ArtifactKind, CancellationToken, HostPlatform, PlannedTool, Result,
    SupplyDisposition, SupplyError, SupplyOutcome, SupplyPlan, SOURCE_POLICY_REVISION,
};

const CACHE_MANIFEST: &str = ".nexus-runtime.json";
const MAX_CACHE_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_COREPACK_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_RUNTIME_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(1);
static PROCESS_SUPPLY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorepackPnpmCandidate {
    pub version: String,
    pub entry_path: PathBuf,
    pub cache_identity: String,
}

#[derive(Debug, Deserialize)]
struct CorepackPackage {
    name: String,
    version: String,
    bin: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct OwnedCacheManifest {
    schema_version: u32,
    tool: String,
    version: String,
    entry: String,
    entry_sha256: String,
    artifacts: Vec<ArtifactIdentity>,
}

pub fn discover_corepack_pnpm(
    root: &Path,
    expected_version: &str,
) -> Result<Option<CorepackPnpmCandidate>> {
    validate_exact_version(expected_version)?;
    if !root.is_absolute() || !root.exists() {
        return Ok(None);
    }
    let canonical_root = fs::canonicalize(root)?;
    reject_reparse(&canonical_root)?;
    let candidates = [
        root.join("v1").join("pnpm").join(expected_version),
        root.join("pnpm").join(expected_version),
    ];
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        let canonical_candidate = fs::canonicalize(&candidate)?;
        if !canonical_candidate.starts_with(&canonical_root) {
            return Err(SupplyError::Integrity(
                "Corepack cache candidate escapes its configured root".to_owned(),
            ));
        }
        validate_path_chain(&canonical_root, &canonical_candidate)?;
        let manifest_path = canonical_candidate.join("package.json");
        let bytes = read_regular_bounded(&manifest_path, MAX_COREPACK_MANIFEST_BYTES)?;
        let package: CorepackPackage = serde_json::from_slice(&bytes).map_err(|error| {
            SupplyError::Integrity(format!("invalid Corepack package.json: {error}"))
        })?;
        if package.name != "pnpm" || package.version != expected_version {
            continue;
        }
        let entry = package
            .bin
            .get("pnpm")
            .and_then(serde_json::Value::as_str)
            .or_else(|| package.bin.as_str());
        if entry != Some("bin/pnpm.mjs") {
            continue;
        }
        let entry_path = canonical_candidate.join("bin").join("pnpm.mjs");
        let canonical_entry = fs::canonicalize(&entry_path)?;
        if !canonical_entry.starts_with(&canonical_candidate) {
            return Err(SupplyError::Integrity(
                "Corepack pnpm entry escapes its package directory".to_owned(),
            ));
        }
        validate_path_chain(&canonical_candidate, &canonical_entry)?;
        let entry_digest = hash_regular_bounded(&canonical_entry, MAX_RUNTIME_ENTRY_BYTES)?;
        let identity = sha256_hex(
            format!(
                "corepack-v1\0{}\0{}\0{}",
                canonical_candidate.display(),
                sha256_hex(&bytes),
                entry_digest
            )
            .as_bytes(),
        );
        return Ok(Some(CorepackPnpmCandidate {
            version: expected_version.to_owned(),
            entry_path: canonical_entry,
            cache_identity: format!("sha256:{identity}"),
        }));
    }
    Ok(None)
}

pub(crate) async fn build_supply_plan<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    host: HostPlatform,
    cache_root: &Path,
    corepack_root: Option<&Path>,
    runtime_plan: &RuntimePlanResponse,
    cancellation: &CancellationToken,
) -> Result<SupplyPlan> {
    if runtime_plan.source != policy.source() {
        return Err(SupplyError::InvalidPlan(
            "runtime plan source does not match source policy".to_owned(),
        ));
    }
    let pnpm_version = runtime_plan.requirements.package_manager.version.clone();
    validate_exact_version(&pnpm_version)?;

    let reusable_node = reusable_tool(runtime_plan, "node")?;
    let reusable_pnpm = reusable_tool(runtime_plan, "pnpm")?;
    if let Some(node) = reusable_node {
        let version = node
            .version
            .as_deref()
            .ok_or_else(|| SupplyError::InvalidPlan("reusable Node lacks a version".to_owned()))?;
        let version = normalize_node_version(version)?;
        for requirement in &runtime_plan.requirements.node {
            if !node_version_satisfies(&requirement.range, &version)? {
                return Err(SupplyError::InvalidPlan(
                    "foundation marked an incompatible Node runtime reusable".to_owned(),
                ));
            }
        }
    }
    if let Some(pnpm) = reusable_pnpm {
        if pnpm.version.as_deref() != Some(&pnpm_version) {
            return Err(SupplyError::InvalidPlan(
                "foundation marked a non-exact pnpm runtime reusable".to_owned(),
            ));
        }
    }

    let corepack = if reusable_pnpm.is_none() {
        corepack_root
            .map(|root| discover_corepack_pnpm(root, &pnpm_version))
            .transpose()?
            .flatten()
    } else {
        None
    };

    if let (Some(node), Some(pnpm)) = (reusable_node, reusable_pnpm) {
        return Ok(SupplyPlan {
            schema_version: 1,
            foundation_plan_id: runtime_plan.plan_id.clone(),
            release_id: runtime_plan.release_id.clone(),
            source_policy_revision: SOURCE_POLICY_REVISION.to_owned(),
            source: runtime_plan.source,
            mode: runtime_plan.mode,
            host,
            destination_root: cache_root.to_owned(),
            node: existing_tool_with_version(
                node,
                normalize_node_version(node.version.as_deref().ok_or_else(|| {
                    SupplyError::InvalidPlan("reusable Node lacks a version".to_owned())
                })?)?,
            )?,
            pnpm: existing_tool(pnpm)?,
            supply_plan_id: String::new(),
        });
    }

    cancellation.check()?;
    let node_version = match reusable_node.and_then(|tool| tool.version.clone()) {
        Some(version) => normalize_node_version(&version)?,
        None => {
            select_node_version(
                downloader,
                policy,
                &runtime_plan.requirements,
                host,
                runtime_plan.mode,
                cancellation,
            )
            .await?
        }
    };

    let node = if let Some(existing) = reusable_node {
        existing_tool_with_version(existing, node_version.clone())?
    } else {
        let identity = resolve_node_artifact(
            downloader,
            policy,
            &node_version,
            host,
            runtime_plan.mode,
            cancellation,
        )
        .await?;
        match runtime_plan.mode {
            RuntimeInstallMode::Portable => {
                let root = node_cache_root(cache_root, &node_version, host);
                if owned_cache_matches(
                    &root,
                    "node",
                    &node_version,
                    std::slice::from_ref(&identity),
                )? {
                    owned_tool(
                        "node",
                        node_version.clone(),
                        root.join("node.exe"),
                        identity,
                    )
                } else {
                    PlannedTool {
                        name: "node".to_owned(),
                        version: node_version.clone(),
                        disposition: SupplyDisposition::PublishPortable,
                        path: root.join("node.exe"),
                        ownership: RuntimeOwnership::Nexus,
                        artifacts: vec![identity],
                        cache_identity: None,
                    }
                }
            }
            RuntimeInstallMode::System => PlannedTool {
                name: "node".to_owned(),
                version: node_version.clone(),
                disposition: SupplyDisposition::InstallSystem,
                path: default_system_node_path()?,
                ownership: RuntimeOwnership::System,
                artifacts: vec![identity],
                cache_identity: None,
            },
        }
    };

    let pnpm = if let Some(existing) = reusable_pnpm {
        existing_tool(existing)?
    } else if let Some(candidate) = corepack {
        PlannedTool {
            name: "pnpm".to_owned(),
            version: pnpm_version.clone(),
            disposition: SupplyDisposition::ReuseCorepackCache,
            path: candidate.entry_path,
            ownership: RuntimeOwnership::System,
            artifacts: Vec::new(),
            cache_identity: Some(candidate.cache_identity),
        }
    } else {
        let package =
            resolve_pnpm_artifact(downloader, policy, &pnpm_version, cancellation).await?;
        match runtime_plan.mode {
            RuntimeInstallMode::Portable => {
                let root = pnpm_cache_root(cache_root, &pnpm_version);
                if owned_cache_matches(
                    &root,
                    "pnpm",
                    &pnpm_version,
                    std::slice::from_ref(&package),
                )? {
                    owned_tool(
                        "pnpm",
                        pnpm_version.clone(),
                        root.join("bin").join("pnpm.mjs"),
                        package,
                    )
                } else {
                    PlannedTool {
                        name: "pnpm".to_owned(),
                        version: pnpm_version.clone(),
                        disposition: SupplyDisposition::PublishPortable,
                        path: root.join("bin").join("pnpm.mjs"),
                        ownership: RuntimeOwnership::Nexus,
                        artifacts: vec![package],
                        cache_identity: None,
                    }
                }
            }
            RuntimeInstallMode::System => {
                let script = resolve_pnpm_script(downloader, policy, cancellation).await?;
                PlannedTool {
                    name: "pnpm".to_owned(),
                    version: pnpm_version,
                    disposition: SupplyDisposition::InstallSystem,
                    path: default_system_pnpm_path()?,
                    ownership: RuntimeOwnership::System,
                    artifacts: vec![package, script],
                    cache_identity: None,
                }
            }
        }
    };

    Ok(SupplyPlan {
        schema_version: 1,
        foundation_plan_id: runtime_plan.plan_id.clone(),
        release_id: runtime_plan.release_id.clone(),
        source_policy_revision: SOURCE_POLICY_REVISION.to_owned(),
        source: runtime_plan.source,
        mode: runtime_plan.mode,
        host,
        destination_root: cache_root.to_owned(),
        node,
        pnpm,
        supply_plan_id: String::new(),
    })
}

fn reusable_tool<'a>(
    runtime_plan: &'a RuntimePlanResponse,
    name: &str,
) -> Result<Option<&'a RuntimePlanTool>> {
    let matches = runtime_plan
        .tools
        .iter()
        .filter(|tool| tool.name == name && tool.state == RuntimePlanToolState::Reusable)
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(SupplyError::InvalidPlan(format!(
            "foundation runtime plan contains duplicate reusable {name} tools"
        )));
    }
    Ok(matches.into_iter().next())
}

fn existing_tool(tool: &RuntimePlanTool) -> Result<PlannedTool> {
    let version = tool.version.clone().ok_or_else(|| {
        SupplyError::InvalidPlan(format!("reusable {} lacks a version", tool.name))
    })?;
    existing_tool_with_version(tool, version)
}

fn existing_tool_with_version(tool: &RuntimePlanTool, version: String) -> Result<PlannedTool> {
    let path = tool
        .path
        .as_ref()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            SupplyError::InvalidPlan(format!("reusable {} path is not absolute", tool.name))
        })?;
    Ok(PlannedTool {
        name: tool.name.clone(),
        version,
        disposition: SupplyDisposition::ReuseExisting,
        path,
        ownership: tool.ownership.ok_or_else(|| {
            SupplyError::InvalidPlan(format!("reusable {} lacks ownership", tool.name))
        })?,
        artifacts: Vec::new(),
        cache_identity: None,
    })
}

fn normalize_node_version(version: &str) -> Result<String> {
    let normalized = version.strip_prefix('v').unwrap_or(version);
    validate_exact_version(normalized)?;
    Ok(normalized.to_owned())
}

fn owned_tool(
    name: &str,
    version: String,
    path: PathBuf,
    artifact: ArtifactIdentity,
) -> PlannedTool {
    PlannedTool {
        name: name.to_owned(),
        version,
        disposition: SupplyDisposition::ReuseOwnedCache,
        path,
        ownership: RuntimeOwnership::Nexus,
        artifacts: vec![artifact],
        cache_identity: None,
    }
}

pub(crate) fn validate_plan_against_policy(plan: &SupplyPlan, policy: &SourcePolicy) -> Result<()> {
    if plan.schema_version != 1 || plan.node.name != "node" || plan.pnpm.name != "pnpm" {
        return Err(SupplyError::InvalidPlan(
            "unsupported supply plan shape".to_owned(),
        ));
    }
    validate_exact_version(&plan.node.version)?;
    validate_exact_version(&plan.pnpm.version)?;
    if !plan.destination_root.is_absolute()
        || !plan.node.path.is_absolute()
        || !plan.pnpm.path.is_absolute()
    {
        return Err(SupplyError::InvalidPlan(
            "supply paths must be absolute".to_owned(),
        ));
    }
    for tool in [&plan.node, &plan.pnpm] {
        match tool.disposition {
            SupplyDisposition::ReuseExisting | SupplyDisposition::ReuseCorepackCache => {
                if !tool.artifacts.is_empty() {
                    return Err(SupplyError::InvalidPlan(
                        "existing runtime cannot contain a download artifact".to_owned(),
                    ));
                }
            }
            SupplyDisposition::ReuseOwnedCache | SupplyDisposition::PublishPortable => {
                if tool.ownership != RuntimeOwnership::Nexus || tool.artifacts.len() != 1 {
                    return Err(SupplyError::InvalidPlan(
                        "portable runtime has invalid ownership or artifact count".to_owned(),
                    ));
                }
            }
            SupplyDisposition::InstallSystem => {
                if tool.ownership != RuntimeOwnership::System || tool.artifacts.is_empty() {
                    return Err(SupplyError::InvalidPlan(
                        "system runtime has invalid ownership or artifacts".to_owned(),
                    ));
                }
            }
        }
        for artifact in &tool.artifacts {
            policy.artifact_request(artifact, &tool.version)?;
        }
    }
    let (node_kind, node_filename) = node_filename(&plan.node.version, plan.host, plan.mode)?;
    if let Some(artifact) = plan.node.artifacts.first() {
        if artifact.kind != node_kind || artifact.filename != node_filename {
            return Err(SupplyError::InvalidPlan(
                "Node artifact is not derived from host and install mode".to_owned(),
            ));
        }
    }
    if plan.mode == RuntimeInstallMode::Portable {
        let expected_node =
            node_cache_root(&plan.destination_root, &plan.node.version, plan.host).join("node.exe");
        if matches!(
            plan.node.disposition,
            SupplyDisposition::ReuseOwnedCache | SupplyDisposition::PublishPortable
        ) && plan.node.path != expected_node
        {
            return Err(SupplyError::InvalidPlan(
                "portable Node destination changed".to_owned(),
            ));
        }
        let expected_pnpm = pnpm_cache_root(&plan.destination_root, &plan.pnpm.version)
            .join("bin")
            .join("pnpm.mjs");
        if matches!(
            plan.pnpm.disposition,
            SupplyDisposition::ReuseOwnedCache | SupplyDisposition::PublishPortable
        ) && plan.pnpm.path != expected_pnpm
        {
            return Err(SupplyError::InvalidPlan(
                "portable pnpm destination changed".to_owned(),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_plan<D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized>(
    downloader: &D,
    runner: &P,
    policy: &SourcePolicy,
    cache_root: &Path,
    corepack_root: Option<&Path>,
    fresh_runtime_plan: &RuntimePlanResponse,
    plan: &SupplyPlan,
    cancellation: &CancellationToken,
) -> Result<SupplyOutcome> {
    let process_lock = PROCESS_SUPPLY_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = process_lock.lock().await;
    cancellation.check()?;
    fs::create_dir_all(cache_root)?;
    reject_reparse(cache_root)?;
    let _file_lock = SupplyFileLock::acquire(cache_root)?;

    revalidate_existing(fresh_runtime_plan, &plan.node)?;
    revalidate_existing(fresh_runtime_plan, &plan.pnpm)?;
    if plan.pnpm.disposition == SupplyDisposition::ReuseCorepackCache {
        let root = corepack_root.ok_or_else(|| {
            SupplyError::InvalidPlan("confirmed Corepack candidate has no cache root".to_owned())
        })?;
        let candidate = discover_corepack_pnpm(root, &plan.pnpm.version)?.ok_or_else(|| {
            SupplyError::InvalidPlan("confirmed Corepack candidate disappeared".to_owned())
        })?;
        if candidate.entry_path != plan.pnpm.path
            || Some(candidate.cache_identity) != plan.pnpm.cache_identity
        {
            return Err(SupplyError::InvalidPlan(
                "confirmed Corepack candidate changed".to_owned(),
            ));
        }
    }

    let mut node_path = plan.node.path.clone();
    let mut reboot_required = false;
    match plan.node.disposition {
        SupplyDisposition::ReuseExisting => {}
        SupplyDisposition::ReuseOwnedCache => {
            let fresh = resolve_node_artifact(
                downloader,
                policy,
                &plan.node.version,
                plan.host,
                RuntimeInstallMode::Portable,
                cancellation,
            )
            .await?;
            if plan.node.artifacts.as_slice() != [fresh] {
                return Err(SupplyError::InvalidPlan(
                    "owned Node cache identity changed".to_owned(),
                ));
            }
            validate_owned_tool(cache_root, &plan.node)?;
            let probe = probe_request(
                plan.node.path.clone(),
                None,
                "node",
                &plan.node.version,
                RuntimeOwnership::Nexus,
                RuntimeOwnership::Nexus,
            );
            verify_probe(&runner.run(&probe, cancellation).await?, &plan.node.version)?;
        }
        SupplyDisposition::PublishPortable => {
            node_path = publish_node(
                downloader,
                runner,
                policy,
                cache_root,
                &plan.node,
                plan.host,
                cancellation,
            )
            .await?;
        }
        SupplyDisposition::InstallSystem => {
            reboot_required |= install_node_system(
                downloader,
                runner,
                policy,
                cache_root,
                &plan.node,
                plan.host,
                cancellation,
            )
            .await?;
        }
        SupplyDisposition::ReuseCorepackCache => {
            return Err(SupplyError::InvalidPlan(
                "Node cannot use Corepack cache".to_owned(),
            ));
        }
    }

    let pnpm_path = match plan.pnpm.disposition {
        SupplyDisposition::ReuseExisting => plan.pnpm.path.clone(),
        SupplyDisposition::ReuseCorepackCache => {
            let probe = probe_request(
                node_path.clone(),
                Some(plan.pnpm.path.clone()),
                "pnpm",
                &plan.pnpm.version,
                plan.node.ownership,
                RuntimeOwnership::System,
            );
            verify_probe(&runner.run(&probe, cancellation).await?, &plan.pnpm.version)?;
            plan.pnpm.path.clone()
        }
        SupplyDisposition::ReuseOwnedCache => {
            let fresh =
                resolve_pnpm_artifact(downloader, policy, &plan.pnpm.version, cancellation).await?;
            if plan.pnpm.artifacts.as_slice() != [fresh] {
                return Err(SupplyError::InvalidPlan(
                    "owned pnpm cache identity changed".to_owned(),
                ));
            }
            validate_owned_tool(cache_root, &plan.pnpm)?;
            let probe = probe_request(
                node_path.clone(),
                Some(plan.pnpm.path.clone()),
                "pnpm",
                &plan.pnpm.version,
                plan.node.ownership,
                RuntimeOwnership::Nexus,
            );
            verify_probe(&runner.run(&probe, cancellation).await?, &plan.pnpm.version)?;
            plan.pnpm.path.clone()
        }
        SupplyDisposition::PublishPortable => {
            publish_pnpm(
                downloader,
                runner,
                policy,
                cache_root,
                &plan.pnpm,
                &node_path,
                plan.node.ownership,
                cancellation,
            )
            .await?
        }
        SupplyDisposition::InstallSystem => {
            reboot_required |= install_pnpm_system(
                downloader,
                runner,
                policy,
                cache_root,
                &plan.pnpm,
                &node_path,
                plan.node.ownership,
                plan.host,
                cancellation,
            )
            .await?;
            plan.pnpm.path.clone()
        }
    };

    let runtime = RuntimeConfig {
        node: Some(RuntimePin {
            path: node_path,
            ownership: plan.node.ownership,
        }),
        pnpm: Some(RuntimePin {
            path: pnpm_path,
            ownership: plan.pnpm.ownership,
        }),
        git: None,
        source: plan.source,
        mode: plan.mode,
    };
    runtime.validate()?;
    Ok(SupplyOutcome {
        runtime,
        node_disposition: plan.node.disposition,
        pnpm_disposition: plan.pnpm.disposition,
        reboot_required,
    })
}

fn revalidate_existing(fresh: &RuntimePlanResponse, planned: &PlannedTool) -> Result<()> {
    if planned.disposition != SupplyDisposition::ReuseExisting {
        return Ok(());
    }
    let current = reusable_tool(fresh, &planned.name)?
        .ok_or_else(|| SupplyError::InvalidPlan(format!("reusable {} changed", planned.name)))?;
    let expected = if planned.name == "node" {
        let version = current
            .version
            .as_deref()
            .ok_or_else(|| SupplyError::InvalidPlan("reusable Node lacks a version".to_owned()))?;
        existing_tool_with_version(current, normalize_node_version(version)?)?
    } else {
        existing_tool(current)?
    };
    if &expected != planned {
        return Err(SupplyError::InvalidPlan(format!(
            "reusable {} observation changed",
            planned.name
        )));
    }
    Ok(())
}

async fn publish_node<D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized>(
    downloader: &D,
    runner: &P,
    policy: &SourcePolicy,
    cache_root: &Path,
    tool: &PlannedTool,
    host: HostPlatform,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    let confirmed = tool
        .artifacts
        .first()
        .ok_or_else(|| SupplyError::InvalidPlan("Node artifact missing".to_owned()))?;
    let fresh = resolve_node_artifact(
        downloader,
        policy,
        &tool.version,
        host,
        RuntimeInstallMode::Portable,
        cancellation,
    )
    .await?;
    if &fresh != confirmed {
        return Err(SupplyError::InvalidPlan(
            "Node release identity changed".to_owned(),
        ));
    }
    if validate_owned_tool(cache_root, tool).is_ok() {
        return Ok(tool.path.clone());
    }
    let staging = create_staging(cache_root)?;
    let result = async {
        let archive = staging.join(&confirmed.filename);
        fetch_file(
            downloader,
            policy,
            policy.artifact_request(confirmed, &tool.version)?,
            &archive,
            MAX_NODE_ARTIFACT_BYTES,
            cancellation,
        )
        .await?;
        verify_file_digest(&archive, confirmed)?;
        let expected_root = format!("node-v{}-win-{}", tool.version, host.node_arch());
        let extracted = extract_node_zip(
            &archive,
            &staging.join("unpacked"),
            &expected_root,
            ArchiveLimits::default(),
            cancellation,
        )?;
        let node = extracted.join("node.exe");
        let probe = probe_request(
            node.clone(),
            None,
            "node",
            &tool.version,
            RuntimeOwnership::Nexus,
            RuntimeOwnership::Nexus,
        );
        verify_probe(&runner.run(&probe, cancellation).await?, &tool.version)?;
        write_cache_manifest(
            &extracted,
            "node",
            &tool.version,
            "node.exe",
            &tool.artifacts,
        )?;
        publish_directory(
            &extracted,
            tool.path
                .parent()
                .ok_or_else(|| SupplyError::InvalidPlan("Node target has no parent".to_owned()))?,
        )?;
        Ok(tool.path.clone())
    }
    .await;
    let cleanup = remove_owned_staging(cache_root, &staging);
    result.and_then(|value| cleanup.map(|_| value).map_err(SupplyError::Io))
}

#[allow(clippy::too_many_arguments)]
async fn publish_pnpm<D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized>(
    downloader: &D,
    runner: &P,
    policy: &SourcePolicy,
    cache_root: &Path,
    tool: &PlannedTool,
    node_path: &Path,
    node_ownership: RuntimeOwnership,
    cancellation: &CancellationToken,
) -> Result<PathBuf> {
    let confirmed = tool
        .artifacts
        .first()
        .ok_or_else(|| SupplyError::InvalidPlan("pnpm artifact missing".to_owned()))?;
    let fresh = resolve_pnpm_artifact(downloader, policy, &tool.version, cancellation).await?;
    if &fresh != confirmed {
        return Err(SupplyError::InvalidPlan(
            "pnpm package identity changed".to_owned(),
        ));
    }
    if validate_owned_tool(cache_root, tool).is_ok() {
        return Ok(tool.path.clone());
    }
    let staging = create_staging(cache_root)?;
    let result = async {
        let archive = staging.join(&confirmed.filename);
        fetch_file(
            downloader,
            policy,
            policy.artifact_request(confirmed, &tool.version)?,
            &archive,
            MAX_PNPM_ARTIFACT_BYTES,
            cancellation,
        )
        .await?;
        verify_file_digest(&archive, confirmed)?;
        let extracted = extract_pnpm_tarball(
            &archive,
            &staging.join("unpacked"),
            ArchiveLimits::default(),
            cancellation,
        )?;
        validate_pnpm_package(&extracted, &tool.version)?;
        let entry = extracted.join("bin").join("pnpm.mjs");
        let probe = probe_request(
            node_path.to_owned(),
            Some(entry),
            "pnpm",
            &tool.version,
            node_ownership,
            RuntimeOwnership::Nexus,
        );
        verify_probe(&runner.run(&probe, cancellation).await?, &tool.version)?;
        write_cache_manifest(
            &extracted,
            "pnpm",
            &tool.version,
            "bin/pnpm.mjs",
            &tool.artifacts,
        )?;
        let target_root = tool.path.parent().and_then(Path::parent).ok_or_else(|| {
            SupplyError::InvalidPlan("pnpm target has no package root".to_owned())
        })?;
        publish_directory(&extracted, target_root)?;
        Ok(tool.path.clone())
    }
    .await;
    let cleanup = remove_owned_staging(cache_root, &staging);
    result.and_then(|value| cleanup.map(|_| value).map_err(SupplyError::Io))
}

async fn install_node_system<D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized>(
    downloader: &D,
    runner: &P,
    policy: &SourcePolicy,
    cache_root: &Path,
    tool: &PlannedTool,
    host: HostPlatform,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let confirmed = tool
        .artifacts
        .first()
        .ok_or_else(|| SupplyError::InvalidPlan("Node MSI artifact missing".to_owned()))?;
    let fresh = resolve_node_artifact(
        downloader,
        policy,
        &tool.version,
        host,
        RuntimeInstallMode::System,
        cancellation,
    )
    .await?;
    if &fresh != confirmed {
        return Err(SupplyError::InvalidPlan(
            "Node MSI identity changed".to_owned(),
        ));
    }
    let staging = create_staging(cache_root)?;
    let result = async {
        let msi = staging.join(&confirmed.filename);
        fetch_file(
            downloader,
            policy,
            policy.artifact_request(confirmed, &tool.version)?,
            &msi,
            MAX_NODE_ARTIFACT_BYTES,
            cancellation,
        )
        .await?;
        verify_file_digest(&msi, confirmed)?;
        let spec = SystemInstallSpec::node_msi(
            msi,
            tool.version.clone(),
            tool.path.clone(),
            host.arch,
            policy.source(),
        );
        let outcome = runner
            .run(&TypedProcessRequest::SystemInstall(spec), cancellation)
            .await?;
        let reboot = match classify_system_install(&outcome) {
            SystemInstallResult::Success => false,
            SystemInstallResult::RebootRequired => true,
            other => return Err(system_failure("Node MSI", other)),
        };
        let probe = probe_request(
            tool.path.clone(),
            None,
            "node",
            &tool.version,
            RuntimeOwnership::System,
            RuntimeOwnership::System,
        );
        verify_probe(&runner.run(&probe, cancellation).await?, &tool.version)?;
        Ok(reboot)
    }
    .await;
    let cleanup = remove_owned_staging(cache_root, &staging);
    result.and_then(|value| cleanup.map(|_| value).map_err(SupplyError::Io))
}

#[allow(clippy::too_many_arguments)]
async fn install_pnpm_system<D: DownloadClient + ?Sized, P: ProcessRunner + ?Sized>(
    downloader: &D,
    runner: &P,
    policy: &SourcePolicy,
    cache_root: &Path,
    tool: &PlannedTool,
    node_path: &Path,
    node_ownership: RuntimeOwnership,
    host: HostPlatform,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let package = tool
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == ArtifactKind::PnpmTarball)
        .ok_or_else(|| SupplyError::InvalidPlan("pnpm package identity missing".to_owned()))?;
    let script = tool
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == ArtifactKind::PnpmInstallScript)
        .ok_or_else(|| SupplyError::InvalidPlan("pnpm installer identity missing".to_owned()))?;
    let fresh_package =
        resolve_pnpm_artifact(downloader, policy, &tool.version, cancellation).await?;
    let fresh_script = resolve_pnpm_script(downloader, policy, cancellation).await?;
    if &fresh_package != package || &fresh_script != script {
        return Err(SupplyError::InvalidPlan(
            "pnpm system identity changed".to_owned(),
        ));
    }
    let staging = create_staging(cache_root)?;
    let result = async {
        let script_path = staging.join("install.ps1");
        fetch_file(
            downloader,
            policy,
            policy.artifact_request(script, &tool.version)?,
            &script_path,
            MAX_METADATA_BYTES,
            cancellation,
        )
        .await?;
        verify_file_digest(&script_path, script)?;
        let spec = SystemInstallSpec::pnpm_user_script(
            script_path,
            tool.version.clone(),
            tool.path.clone(),
            host.arch,
            policy.source(),
        );
        let outcome = runner
            .run(&TypedProcessRequest::SystemInstall(spec), cancellation)
            .await?;
        let reboot = match classify_system_install(&outcome) {
            SystemInstallResult::Success => false,
            SystemInstallResult::RebootRequired => true,
            other => return Err(system_failure("pnpm user installer", other)),
        };
        let probe = probe_request(
            node_path.to_owned(),
            Some(tool.path.clone()),
            "pnpm",
            &tool.version,
            node_ownership,
            RuntimeOwnership::System,
        );
        verify_probe(&runner.run(&probe, cancellation).await?, &tool.version)?;
        Ok(reboot)
    }
    .await;
    let cleanup = remove_owned_staging(cache_root, &staging);
    result.and_then(|value| cleanup.map(|_| value).map_err(SupplyError::Io))
}

fn system_failure(label: &str, result: SystemInstallResult) -> SupplyError {
    let detail = match result {
        SystemInstallResult::UserCancelled => "user_cancelled".to_owned(),
        SystemInstallResult::UacDenied => "uac_denied".to_owned(),
        SystemInstallResult::SpawnFailed(code) => format!("spawn_failed_{code}"),
        SystemInstallResult::Failed(code) => format!("exit_code_{code}"),
        SystemInstallResult::NeedsVerification(reason) => format!("needs_verification: {reason}"),
        SystemInstallResult::Success => "unexpected_success_mapping".to_owned(),
        SystemInstallResult::RebootRequired => "unexpected_reboot_mapping".to_owned(),
    };
    SupplyError::Process(format!("{label}: {detail}; no runtime pin was produced"))
}

fn validate_pnpm_package(root: &Path, expected_version: &str) -> Result<()> {
    let bytes = read_regular_bounded(&root.join("package.json"), MAX_COREPACK_MANIFEST_BYTES)?;
    let package: CorepackPackage = serde_json::from_slice(&bytes).map_err(|error| {
        SupplyError::Integrity(format!("invalid extracted pnpm package.json: {error}"))
    })?;
    if package.name != "pnpm"
        || package.version != expected_version
        || package.bin.get("pnpm").and_then(serde_json::Value::as_str) != Some("bin/pnpm.mjs")
    {
        return Err(SupplyError::Integrity(
            "extracted pnpm package identity is wrong".to_owned(),
        ));
    }
    read_regular_bounded(&root.join("bin").join("pnpm.mjs"), MAX_RUNTIME_ENTRY_BYTES)?;
    Ok(())
}

fn node_cache_root(cache_root: &Path, version: &str, host: HostPlatform) -> PathBuf {
    cache_root
        .join("node")
        .join(format!("v{version}-win-{}", host.node_arch()))
}

fn pnpm_cache_root(cache_root: &Path, version: &str) -> PathBuf {
    cache_root.join("pnpm").join(version)
}

pub(crate) fn owned_cache_matches(
    root: &Path,
    tool: &str,
    version: &str,
    artifacts: &[ArtifactIdentity],
) -> Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    let bytes = match read_regular_bounded(&root.join(CACHE_MANIFEST), MAX_CACHE_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    let manifest: OwnedCacheManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    if manifest.schema_version != 1
        || manifest.tool != tool
        || manifest.version != version
        || manifest.artifacts != artifacts
    {
        return Ok(false);
    }
    let entry = safe_relative(&manifest.entry)?;
    let entry_path = root.join(entry);
    Ok(hash_regular_bounded(&entry_path, runtime_entry_limit(tool)?)
        .is_ok_and(|digest| digest == manifest.entry_sha256))
}

fn validate_owned_tool(cache_root: &Path, tool: &PlannedTool) -> Result<()> {
    let root = match tool.name.as_str() {
        "node" => tool.path.parent(),
        "pnpm" => tool.path.parent().and_then(Path::parent),
        _ => None,
    }
    .ok_or_else(|| SupplyError::InvalidPlan("owned tool path has no cache root".to_owned()))?;
    let canonical_cache = fs::canonicalize(cache_root)?;
    let canonical_root = fs::canonicalize(root)?;
    if !canonical_root.starts_with(&canonical_cache) {
        return Err(SupplyError::Integrity(
            "owned cache escapes runtimes_dir".to_owned(),
        ));
    }
    if !owned_cache_matches(root, &tool.name, &tool.version, &tool.artifacts)? {
        return Err(SupplyError::Integrity(format!(
            "owned {} cache is incomplete or changed",
            tool.name
        )));
    }
    Ok(())
}

pub(crate) fn write_cache_manifest(
    root: &Path,
    tool: &str,
    version: &str,
    entry: &str,
    artifacts: &[ArtifactIdentity],
) -> Result<()> {
    let entry_path = root.join(safe_relative(entry)?);
    let manifest = OwnedCacheManifest {
        schema_version: 1,
        tool: tool.to_owned(),
        version: version.to_owned(),
        entry: entry.to_owned(),
        entry_sha256: hash_regular_bounded(&entry_path, runtime_entry_limit(tool)?)?,
        artifacts: artifacts.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| SupplyError::Integrity(error.to_string()))?;
    let path = root.join(CACHE_MANIFEST);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    #[cfg(not(windows))]
    sync_directory(root)?;
    Ok(())
}

pub(crate) fn publish_directory(staging_root: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        return Err(SupplyError::Busy(format!(
            "runtime target already exists: {}",
            target.display()
        )));
    }
    let parent = target
        .parent()
        .ok_or_else(|| SupplyError::InvalidPlan("runtime target has no parent".to_owned()))?;
    fs::create_dir_all(parent)?;
    reject_reparse(parent)?;
    #[cfg(windows)]
    publish_directory_windows(staging_root, target)?;
    #[cfg(not(windows))]
    {
        sync_directory(staging_root)?;
        fs::rename(staging_root, target)?;
        sync_directory(parent)?;
    }
    Ok(())
}

#[cfg(windows)]
fn publish_directory_windows(staging_root: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let source = staging_root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // Windows does not support flushing directory handles with FlushFileBuffers.
    // Every extracted file and the manifest are flushed before this point; the
    // supported write-through move supplies the durable namespace publication.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(SupplyError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

pub(crate) fn create_staging(cache_root: &Path) -> Result<PathBuf> {
    let id = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = cache_root.join(format!(".staging-{}-{id}", std::process::id()));
    fs::create_dir(&path)?;
    reject_reparse(&path)?;
    Ok(path)
}

struct SupplyFileLock {
    path: PathBuf,
    file: Option<File>,
}

impl SupplyFileLock {
    fn acquire(cache_root: &Path) -> Result<Self> {
        let path = cache_root.join(".runtime-supply.lock");
        let file = open_supply_lock(&path).map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
            ) {
                SupplyError::Busy("another process owns the runtime supply lock".to_owned())
            } else {
                SupplyError::Io(error)
            }
        })?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }
}

#[cfg(windows)]
fn open_supply_lock(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
}

#[cfg(not(windows))]
fn open_supply_lock(path: &Path) -> std::io::Result<File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

impl Drop for SupplyFileLock {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn safe_relative(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SupplyError::Integrity(
            "cache entry path is unsafe".to_owned(),
        ));
    }
    Ok(path.to_owned())
}

fn read_regular_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    reject_reparse(path)?;
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(SupplyError::Integrity("expected a regular file".to_owned()));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(SupplyError::Integrity(format!(
            "{} exceeds its size limit",
            path.display()
        )));
    }
    Ok(bytes)
}

fn runtime_entry_limit(tool: &str) -> Result<u64> {
    match tool {
        "node" => Ok(ArchiveLimits::default().max_entry_bytes),
        "pnpm" => Ok(MAX_RUNTIME_ENTRY_BYTES),
        _ => Err(SupplyError::InvalidPlan(format!("unknown runtime cache tool: {tool}"))),
    }
}

fn hash_regular_bounded(path: &Path, maximum: u64) -> Result<String> {
    reject_reparse(path)?;
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SupplyError::Integrity(format!(
            "runtime entry is not a regular file: {} (size={}, limit={maximum})",
            path.display(), metadata.len(),
        )));
    }
    if metadata.len() > maximum {
        return Err(SupplyError::Integrity(format!(
            "runtime entry exceeds its size limit: {} (size={}, limit={maximum})",
            path.display(), metadata.len(),
        )));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > maximum {
            return Err(SupplyError::Integrity(format!(
                "runtime entry exceeds its size limit: {} (size={total}, limit={maximum})",
                path.display(),
            )));
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn validate_path_chain(root: &Path, target: &Path) -> Result<()> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| SupplyError::Integrity("path escapes configured root".to_owned()))?;
    let mut current = root.to_owned();
    reject_reparse(&current)?;
    for component in relative.components() {
        current.push(component.as_os_str());
        reject_reparse(&current)?;
    }
    Ok(())
}

fn reject_reparse(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(SupplyError::Integrity(format!(
            "reparse path is forbidden: {}",
            path.display()
        )));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(SupplyError::Integrity(format!(
                "reparse path is forbidden: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
