use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flate2::{write::GzEncoder, Compression};
use nexus_protocol::{
    RuntimeInstallMode, RuntimeNodeRequirement, RuntimeOwnership, RuntimePackageManagerRequirement,
    RuntimePlanResponse, RuntimePlanTool, RuntimePlanToolState, RuntimeRequirements, RuntimeSource,
    API_VERSION,
};
use tar::{Builder as TarBuilder, EntryType, Header};
use tempfile::TempDir;
use zip::{write::SimpleFileOptions, ZipWriter};

use crate::{
    archive::ArchiveLimits,
    extract_node_zip, extract_pnpm_tarball,
    portable::{create_staging, publish_directory, write_cache_manifest},
    source::{fetch_bytes, sha256_hex, verify_pnpm_metadata, DownloadKind, DownloadReceipt},
    system::{classify_system_install, ProcessOutcome, SystemInstallResult},
    ArtifactIdentity, ArtifactKind, CancellationToken, DownloadClient, HostArch, HostOs,
    HostPlatform, ProcessRunner, Result, RuntimeSupplier, RuntimeSupplyPlanner, SourcePolicy,
    SupplyDisposition, SupplyError, TypedDownloadRequest, TypedDownloadResponse,
    TypedProcessRequest,
};

const PNPM_METADATA: &str = r#"{"bin":{"pnpm":"bin/pnpm.mjs"},"dist":{"integrity":"sha512-GcyFLBIMcSV2DyRD7mvgyltA+fUFmN4aCaHxd1A+AQ5Xwjx3ZG4B52HeWb+HT7IqM5jDOrlpH8E+uUa28PTWIA==","signatures":[{"sig":"MEUCIHPuCpPJCRz4QWSyd/Qf/8Eeni+iuECdgPv5JJwAlT1BAiEA3Nv2FicH4kf7n0OzZKEqoEjO45+YYVcfFysFwSPFXYM=","keyid":"SHA256:DhQ8wR5APBvFHLF/+Tc+AYvPOdTpcIDqOhxsBHRwC7U"}]},"name":"pnpm","version":"11.7.0"}"#;
const PNPM_INTEGRITY: &str = "sha512-GcyFLBIMcSV2DyRD7mvgyltA+fUFmN4aCaHxd1A+AQ5Xwjx3ZG4B52HeWb+HT7IqM5jDOrlpH8E+uUa28PTWIA==";
const NPM_KEY_ID: &str = "SHA256:DhQ8wR5APBvFHLF/+Tc+AYvPOdTpcIDqOhxsBHRwC7U";
const NODE_V24_19_CHECKSUMS_ASC_B64: &str = "LS0tLS1CRUdJTiBQR1AgU0lHTkVEIE1FU1NBR0UtLS0tLQpIYXNoOiBTSEEyNTYKCmY0ZTM1YzEzMTY1ZGU2ODgwY2FhMjU1OGMwYWE0OGNhODhhZGE0N2ZlMjIzNGJlZDA3ZDY2ZGJiODBhNDdjOGQgIG5vZGUtdjI0LjE5LjAtYWl4LXBwYzY0LnRhci5nego0N2IxNmUxYjEwMTJiMWI5YWQ2MjE2OWIzYTQ2NmFkYjZiYzc1OGIyY2I4YmQ4MjI0NjgzYzA4NjgzNjQ4NGY4ICBub2RlLXYyNC4xOS4wLWFybTY0Lm1zaQo4Mjk0YjdhYTliMDM5OTc0ODFjMDZiYWJmMWU4YjI3MGM4NTkzNThmMjdkYTU3YTExNTA5YWZlNTM3YWMzODFkICBub2RlLXYyNC4xOS4wLWRhcndpbi1hcm02NC50YXIuZ3oKM2YxY2YxNTc0NzljMTQ4MDM1MjA4MzEwNWUxM2ZhZjlkMDA4ZWRlOThlN2UxNTc3NDZiNmRmOTQwZDE5N2I5NCAgbm9kZS12MjQuMTkuMC1kYXJ3aW4tYXJtNjQudGFyLnh6CmQxYjVlOTk5ZGIxNThjNjJmZThmNzI2N2E0NDc2YjAzNWQ4YmQ5M2IxYTYwNWJhYzI0YTNmMGRkMTY2ZTMzMTYgIG5vZGUtdjI0LjE5LjAtZGFyd2luLXg2NC50YXIuZ3oKZDM1ZTk1MjMwZjQ2ZjZmMDc1MWRmNDk3YzU2NjIyYzY3MzVlMDVkNWUxZmIxNjMwOTk2YTAwNWI5ZDMyOGZlNCAgbm9kZS12MjQuMTkuMC1kYXJ3aW4teDY0LnRhci54ego1NGYxNGEyOTdkNDdlYTA3OTRmZTI3MjM2MzcwM2Q5ZGM0MTljOTZhYzY4ZjIwZDg5MGY5OGI2Mzc1NGEzZTRjICBub2RlLXYyNC4xOS4wLWhlYWRlcnMudGFyLmd6CjQzNWRhODZlZjljNTRjNzY2MjQyNDA2MjhjYTRiYzk4YzhkODA3Mzk4YTg2YzllZDRhZWExNjZhM2RjMGJjZjIgIG5vZGUtdjI0LjE5LjAtaGVhZGVycy50YXIueHoKZDI4YzhhNWJmMGE4MDhmMGVkNDM0YTFkY2U4YzU0YWU5OGYwMzcxYzBiZDg2YWM1OGFiYzYxM2Y3M2U2NjQzZiAgbm9kZS12MjQuMTkuMC1saW51eC1hcm02NC50YXIuZ3oKMDE0NDNjMWUxYTI5ZTUzMWNjYWQ1YTQ2ZmVmYTZkZjQ5MGQyMTg5YzQ5Zjc5NTU5MDRhZWNkYmIwZmU4NmZkYyAgbm9kZS12MjQuMTkuMC1saW51eC1hcm02NC50YXIueHoKYjMxYThjNGNiYzU4OWY5MTUyZjMyZmRiNDMxODQ2OTc1OWUxNjU1OGE1YjVkOTNiN2ZmMmZiMjg3YjAxM2RiMiAgbm9kZS12MjQuMTkuMC1saW51eC1wcGM2NGxlLnRhci5negpjNTEwYzZjZTEyZjA3MDEwZjc3MWU2ZWRiMjJhM2ZlMjNmNGYyZTZmNDBiMWZmZDQ5NDFhZWQwNjQ2YTBkOGIzICBub2RlLXYyNC4xOS4wLWxpbnV4LXBwYzY0bGUudGFyLnh6CmIzYWI2YTQzOTI4MjhkYjlmZDRlZDk2ODM5OGQ1NTE4ODI5MmJjYzE1ODE5NTQyZDUzYmUwYjExMDBlMzMyZmMgIG5vZGUtdjI0LjE5LjAtbGludXgtczM5MHgudGFyLmd6CmE0NzkyZTY1OTYyZmZhMGFmNDI2MjdhYWNmMTEyMmE2MGMzYzg4ZGJmNGU0MTg0ZjA2ODIwZDY2ZjlkYThiYTQgIG5vZGUtdjI0LjE5LjAtbGludXgtczM5MHgudGFyLnh6CmY2MjVkOTdjZDcwN2RmNGZmOTYyNTQ5MTZmYmM1ZmYwMTRmMDljMDllZmZlNWExZTBjYThmNmQ0MWE4Nzg5ZDQgIG5vZGUtdjI0LjE5LjAtbGludXgteDY0LnRhci5negoxNGIzNDJlNzEyMDRmODExYmRlNjE1M2JlOGUwNGI2MmFlZjYzYzIzNmZlZjkyYjU1ZjljODMxNTRiNDA5NjQ3ICBub2RlLXYyNC4xOS4wLWxpbnV4LXg2NC50YXIueHoKNzU1YjAyM2U3MjlkYWM2M2M0ZDc0Njg4M2NhOTc5MDNhYmZjMjc4ODEzZDdiOWIxMDZlZDNhZWE4N2IzMjc4YyAgbm9kZS12MjQuMTkuMC13aW4tYXJtNjQuN3oKODUwMmY0YTUwYjQ1OGQ0Y2MzOGVkOGYyMDAxNTU2YzJjZDIzOWQ0NjQ5MjBmNzQwMTc5MjZjY2IxZTFjMTU3ZiAgbm9kZS12MjQuMTkuMC13aW4tYXJtNjQuemlwCjY0YWI4NDgwNTNkN2QwNTViNjZjMGNhOWVlOTRlY2NhYTQ5N2UxZjg2N2RlMzIxNmVhZWY2MTUwY2E4MmUwNzUgIG5vZGUtdjI0LjE5LjAtd2luLXg2NC43ego1N2Y3MWFiMzY1MmU3OTdkODRhY2RkYzc5YzgxY2M5ZmYxYzZkZGIyYTE5NzRjZGI4M2YwMGZlZTliZmY0YzczICBub2RlLXYyNC4xOS4wLXdpbi14NjQuemlwCmYwZjY2YzJhODBjMDhhMzBhNWFiNTE3OWVlOWVhOWU0NWY5YjQ2Mjg5NDM2YThjYzg3ZmY4MzNiODUyZGIzNTEgIG5vZGUtdjI0LjE5LjAteDY0Lm1zaQoxM2VjZWJmZWZhMDIzNGUzZDYxOGI0YTBhZjhjNTgwM2JkZWVkYWIzMGI4NGVlMzdjY2NhZmI3Mjc2ZDkwYTBlICBub2RlLXYyNC4xOS4wLnBrZwoxNmZlMjU4MDA2YTZlODY4NDRmYmUwNWIzYjVlMWU1NjIzY2E4ZDNkYTU0ZTMyZDk4ZDllODMyMzRiZjI1YjAxICBub2RlLXYyNC4xOS4wLnRhci5negpmNmQ5NWUxMGEwNDMxZWUxMDY3ZmM2YWFiZTlmNzYyOTA4YjQ3MTZkZDM1MzI0ZTFkZGI0YjE0NjZiNzY2NTlmICBub2RlLXYyNC4xOS4wLnRhci54egozOTU4ZTRiYjNmMmQ0ZWYzN2M5MzgyMTVkZmM2NWE5ZDNjOWQ4MzliNTA2MGZlYzEwM2JkMjM0NWZhNzhlOTUxICB3aW4tYXJtNjQvbm9kZS5leGUKODhkZWE2ZmY2NjU2NDQ1ZWUyMmVkODg0MzgxNmQ4ZDkwZDcwN2MzZDA1MjI4YmQwYTNhZDU2ZWEzMGU4YTM5NSAgd2luLWFybTY0L25vZGUubGliCjM0NTNmMTM5YTc2MWMwNTZlYmNkMzllMGQ3MmE1NDIyZDljYTcyMGJjODU3ODhiNjY0YWY1MDQxZTRjYzk2OTggIHdpbi1hcm02NC9ub2RlX3BkYi43egpmMzAzMzUyYTY1NjkxNzhkNDVmNWI0Nzc0ZmRlYjJhNjViMWE5Njg2ODcyYTRmZWYwYTgxMmZiY2NiYzUwMjg0ICB3aW4tYXJtNjQvbm9kZV9wZGIuemlwCjM2MDJmMmJiMWExMGYyY2JhYjRjMzY4ODYyMThhMzNjMWFiM2RiODcyOTBlNzNiMDMzYzQ2Yzc3MTQ3ZDAyMzcgIHdpbi14NjQvbm9kZS5leGUKNjNlYzgzMWJiZjE2NGQxYjIzMTk3ZDZmYWMxOTQ0ZGZiMTQ2NTM0ZTMzMjg4OWNhMDc1NWQyNTBlOGRlZGZmOSAgd2luLXg2NC9ub2RlLmxpYgpjNGMyMDQ0ZDEwODZmZWVkMTJkM2JhOTRjMWFiNDIwZjIwNTQ4MjUwNGZjNzczYzI1YTIwM2Q2NTkwODE3N2UyICB3aW4teDY0L25vZGVfcGRiLjd6CjBiYjNlNDBhYmE2YzhhMWFkNjg1YjAwNGExZjUzMWE5MmFmZWYzNTEyZjhjYjc2ZjY3NDI5MGY4NjFjZjI3N2YgIHdpbi14NjQvbm9kZV9wZGIuemlwCgotLS0tLUJFR0lOIFBHUCBTSUdOQVRVUkUtLS0tLQoKaUhVRUFSWUlBQjBXSVFSYjZLUDJ5S1hBSFJCc0N0Z2dzYU9Rc1dqVFZnVUNhbkNiUWdBS0NSQWdzYU9Rc1dqVApWc0dZQVFDWUcxSjhmTzVCUzQ4M1FYUDhMVE1NNHgzMmt0ZXl3aUFJaGprbFVVOW5mQUVBcU1PS2psVjVpcFpnCnRMRk1QVThKNGoxMncreG5ZOFVOWkVQeTlEMG5lUWM9Cj1NYmNlCi0tLS0tRU5EIFBHUCBTSUdOQVRVUkUtLS0tLQo=";

#[derive(Clone)]
struct Reply {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

#[derive(Default)]
struct MockDownloader {
    replies: Mutex<HashMap<String, Reply>>,
    counts: Mutex<HashMap<DownloadKind, usize>>,
}

impl MockDownloader {
    fn insert(&self, url: &str, reply: Reply) {
        self.replies.lock().unwrap().insert(url.to_owned(), reply);
    }

    fn count(&self, kind: DownloadKind) -> usize {
        *self.counts.lock().unwrap().get(&kind).unwrap_or(&0)
    }

    fn reply(&self, request: &TypedDownloadRequest) -> Result<Reply> {
        *self
            .counts
            .lock()
            .unwrap()
            .entry(request.kind())
            .or_default() += 1;
        self.replies
            .lock()
            .unwrap()
            .get(request.url().as_str())
            .cloned()
            .ok_or_else(|| SupplyError::Network(format!("unexpected test URL: {}", request.url())))
    }
}

#[async_trait]
impl DownloadClient for MockDownloader {
    async fn fetch_bytes_once(
        &self,
        request: &TypedDownloadRequest,
        max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<TypedDownloadResponse> {
        cancellation.check()?;
        let reply = self.reply(request)?;
        if reply.body.len() as u64 > max_bytes {
            return Err(SupplyError::Network("test body exceeds limit".to_owned()));
        }
        Ok(TypedDownloadResponse {
            status: reply.status,
            redirect_location: reply.location,
            body: reply.body,
        })
    }

    async fn fetch_file_once(
        &self,
        request: &TypedDownloadRequest,
        destination: &Path,
        max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<DownloadReceipt> {
        cancellation.check()?;
        let reply = self.reply(request)?;
        if reply.body.len() as u64 > max_bytes {
            return Err(SupplyError::Network("test body exceeds limit".to_owned()));
        }
        if (200..300).contains(&reply.status) {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?
                .write_all(&reply.body)?;
        }
        Ok(DownloadReceipt {
            status: reply.status,
            redirect_location: reply.location,
            bytes_written: reply.body.len() as u64,
        })
    }
}

#[derive(Default)]
struct MockRunner {
    calls: Mutex<Vec<TypedProcessRequest>>,
}

#[async_trait]
impl ProcessRunner for MockRunner {
    async fn run(
        &self,
        request: &TypedProcessRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ProcessOutcome> {
        self.calls.lock().unwrap().push(request.clone());
        let stdout = match request {
            TypedProcessRequest::Probe {
                expected_version, ..
            } => expected_version.as_bytes().to_vec(),
            TypedProcessRequest::SystemInstall(_) => Vec::new(),
        };
        Ok(ProcessOutcome {
            exit_code: Some(0),
            stdout,
            stderr: Vec::new(),
            timed_out: false,
            cancelled: false,
            os_error: None,
        })
    }
}

fn host() -> HostPlatform {
    HostPlatform {
        os: HostOs::Windows,
        arch: HostArch::X64,
    }
}

fn runtime_plan(root: &Path, reusable_pnpm: bool) -> RuntimePlanResponse {
    let mut tools = vec![RuntimePlanTool {
        name: "node".to_owned(),
        requirements: vec!["^22.19.0 || >=24.0.0".to_owned()],
        state: RuntimePlanToolState::Reusable,
        version: Some("24.19.0".to_owned()),
        path: Some(root.join("system-node.exe").to_string_lossy().into_owned()),
        ownership: Some(RuntimeOwnership::System),
        reason: None,
    }];
    tools.push(RuntimePlanTool {
        name: "pnpm".to_owned(),
        requirements: vec!["pnpm@11.7.0".to_owned()],
        state: if reusable_pnpm {
            RuntimePlanToolState::Reusable
        } else {
            RuntimePlanToolState::Missing
        },
        version: reusable_pnpm.then(|| "11.7.0".to_owned()),
        path: reusable_pnpm.then(|| root.join("system-pnpm.cmd").to_string_lossy().into_owned()),
        ownership: reusable_pnpm.then_some(RuntimeOwnership::System),
        reason: (!reusable_pnpm).then(|| "not_found".to_owned()),
    });
    RuntimePlanResponse {
        api_version: API_VERSION.to_owned(),
        plan_id: "foundation-plan-a".to_owned(),
        release_id: "dsh-v0.1.2-alpha.3".to_owned(),
        source: RuntimeSource::Official,
        mode: RuntimeInstallMode::Portable,
        requirements: RuntimeRequirements {
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
        },
        tools,
        suggested_actions: Vec::new(),
    }
}

#[tokio::test]
async fn confirmation_and_fresh_foundation_plan_are_mandatory() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let downloader = MockDownloader::default();
    let runner = MockRunner::default();
    let foundation = runtime_plan(temp.path(), true);
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    let supplier = RuntimeSupplier::new(&downloader, &runner, host(), cache.clone(), None).unwrap();

    let missing = supplier
        .execute_confirmed(&foundation, &plan, "", &CancellationToken::default())
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("confirmation"));
    assert!(!cache.exists());

    let mut changed = foundation.clone();
    changed.plan_id = "foundation-plan-b".to_owned();
    let stale = supplier
        .execute_confirmed(
            &changed,
            &plan,
            &plan.supply_plan_id,
            &CancellationToken::default(),
        )
        .await
        .unwrap_err();
    assert!(stale.to_string().contains("changed"));
}

#[tokio::test]
async fn caller_cannot_reseal_a_changed_destination_or_existing_path() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let downloader = MockDownloader::default();
    let runner = MockRunner::default();
    let foundation = runtime_plan(temp.path(), true);
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    let supplier = RuntimeSupplier::new(&downloader, &runner, host(), cache, None).unwrap();

    let mut changed = plan.clone();
    changed.node.path = temp.path().join("attacker-node.exe");
    changed = changed.seal().unwrap();
    let token = changed.supply_plan_id.clone();
    let error = supplier
        .execute_confirmed(&foundation, &changed, &token, &CancellationToken::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("observation changed"));
}

#[tokio::test]
async fn strict_redirect_policy_allows_only_the_compiled_mirror_transition() {
    let downloader = MockDownloader::default();
    downloader.insert(
        "https://npmmirror.com/mirrors/node/index.json",
        Reply {
            status: 302,
            location: Some("https://cdn.npmmirror.com/binaries/node/index.json".to_owned()),
            body: Vec::new(),
        },
    );
    downloader.insert(
        "https://cdn.npmmirror.com/binaries/node/index.json",
        Reply {
            status: 200,
            location: None,
            body: b"[]".to_vec(),
        },
    );
    let policy = SourcePolicy::new(RuntimeSource::Npmmirror);
    let body = fetch_bytes(
        &downloader,
        &policy,
        policy.node_index().unwrap(),
        32,
        &CancellationToken::default(),
    )
    .await
    .unwrap();
    assert_eq!(body, b"[]");

    let bad = MockDownloader::default();
    bad.insert(
        "https://npmmirror.com/mirrors/node/index.json",
        Reply {
            status: 302,
            location: Some("http://cdn.npmmirror.com/binaries/node/index.json".to_owned()),
            body: Vec::new(),
        },
    );
    let error = fetch_bytes(
        &bad,
        &policy,
        policy.node_index().unwrap(),
        32,
        &CancellationToken::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("HTTPS"));

    let changed = MockDownloader::default();
    changed.insert(
        "https://npmmirror.com/mirrors/node/index.json",
        Reply {
            status: 302,
            location: Some(
                "https://cdn.npmmirror.com/binaries/node/v24.19.0/SHASUMS256.txt.asc".to_owned(),
            ),
            body: Vec::new(),
        },
    );
    let error = fetch_bytes(
        &changed,
        &policy,
        policy.node_index().unwrap(),
        32,
        &CancellationToken::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("artifact identity"));
}

#[test]
fn npm_signature_is_checked_before_its_sri_is_trusted() {
    let (integrity, key) = verify_pnpm_metadata(PNPM_METADATA.as_bytes(), "11.7.0").unwrap();
    assert_eq!(integrity, PNPM_INTEGRITY);
    assert_eq!(key, NPM_KEY_ID);
    let corrupt = PNPM_METADATA.replace("11.7.0", "11.7.1");
    assert!(verify_pnpm_metadata(corrupt.as_bytes(), "11.7.1").is_err());
}

#[tokio::test]
async fn exact_corepack_cache_is_read_only_and_content_bound() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let corepack = temp.path().join("corepack");
    let package = corepack.join("v1/pnpm/11.7.0");
    fs::create_dir_all(package.join("bin")).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"pnpm","version":"11.7.0","bin":{"pnpm":"bin/pnpm.mjs"}}"#,
    )
    .unwrap();
    fs::write(package.join("bin/pnpm.mjs"), "console.log('11.7.0')").unwrap();
    let downloader = MockDownloader::default();
    let runner = MockRunner::default();
    let foundation = runtime_plan(temp.path(), false);
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        Some(corepack.clone()),
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    assert_eq!(plan.pnpm.disposition, SupplyDisposition::ReuseCorepackCache);
    assert_eq!(downloader.count(DownloadKind::PnpmMetadata), 0);

    fs::write(package.join("bin/pnpm.mjs"), "changed").unwrap();
    let supplier =
        RuntimeSupplier::new(&downloader, &runner, host(), cache, Some(corepack)).unwrap();
    let error = supplier
        .execute_confirmed(
            &foundation,
            &plan,
            &plan.supply_plan_id,
            &CancellationToken::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Corepack candidate changed"));
}

#[test]
fn zip_traversal_and_case_collisions_are_rejected_without_escape() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("bad.zip");
    let output = fs::File::create(&archive).unwrap();
    let mut writer = ZipWriter::new(output);
    writer
        .start_file(
            "node-v24.19.0-win-x64/../escape.txt",
            SimpleFileOptions::default(),
        )
        .unwrap();
    writer.write_all(b"escape").unwrap();
    writer.finish().unwrap();
    let destination = temp.path().join("unpacked");
    let error = extract_node_zip(
        &archive,
        &destination,
        "node-v24.19.0-win-x64",
        ArchiveLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap_err();
    assert!(matches!(error, SupplyError::UnsafeArchive(_)));
    assert!(!temp.path().join("escape.txt").exists());

    let collision = temp.path().join("collision.zip");
    let output = fs::File::create(&collision).unwrap();
    let mut writer = ZipWriter::new(output);
    writer
        .start_file("node-v24.19.0-win-x64/A.txt", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"first").unwrap();
    writer
        .start_file("node-v24.19.0-win-x64/a.TXT", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"second").unwrap();
    writer.finish().unwrap();
    let error = extract_node_zip(
        &collision,
        &temp.path().join("collision-out"),
        "node-v24.19.0-win-x64",
        ArchiveLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("case-colliding"));
}

#[test]
fn pnpm_tarball_extracts_regular_files_and_rejects_links() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("pnpm.tgz");
    let gzip = GzEncoder::new(fs::File::create(&archive).unwrap(), Compression::default());
    let mut builder = TarBuilder::new(gzip);
    let mut header = Header::new_gnu();
    header.set_size(21);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            "package/bin/pnpm.mjs",
            &b"console.log('11.7.0')"[..],
        )
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    let root = extract_pnpm_tarball(
        &archive,
        &temp.path().join("pnpm-out"),
        ArchiveLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(root.join("bin/pnpm.mjs")).unwrap(),
        b"console.log('11.7.0')"
    );

    let linked = temp.path().join("linked.tgz");
    let gzip = GzEncoder::new(fs::File::create(&linked).unwrap(), Compression::default());
    let mut builder = TarBuilder::new(gzip);
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_link_name("../../escape").unwrap();
    header.set_cksum();
    builder
        .append_data(&mut header, "package/bin/pnpm", &[][..])
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    let error = extract_pnpm_tarball(
        &linked,
        &temp.path().join("linked-out"),
        ArchiveLimits::default(),
        &CancellationToken::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("links and device entries"));
    assert!(!temp.path().join("escape").exists());
}

#[test]
fn corruption_and_system_exit_states_fail_closed() {
    let identity = ArtifactIdentity {
        kind: ArtifactKind::NodeZip,
        locator: "node/v24.19.0/node-v24.19.0-win-x64.zip".to_owned(),
        filename: "node-v24.19.0-win-x64.zip".to_owned(),
        digest_algorithm: "sha256".to_owned(),
        digest: sha256_hex(b"expected"),
        signing_key_id: Some("test".to_owned()),
    };
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("artifact.zip");
    fs::write(&file, b"corrupt").unwrap();
    assert!(crate::source::verify_file_digest(&file, &identity).is_err());

    let outcome = |code, timed_out, cancelled, os_error| ProcessOutcome {
        exit_code: code,
        stdout: Vec::new(),
        stderr: Vec::new(),
        timed_out,
        cancelled,
        os_error,
    };
    assert_eq!(
        classify_system_install(&outcome(Some(0), false, false, None)),
        SystemInstallResult::Success
    );
    assert_eq!(
        classify_system_install(&outcome(Some(1602), false, false, None)),
        SystemInstallResult::UserCancelled
    );
    assert_eq!(
        classify_system_install(&outcome(Some(1603), false, false, None)),
        SystemInstallResult::Failed(1603)
    );
    assert_eq!(
        classify_system_install(&outcome(Some(3010), false, false, None)),
        SystemInstallResult::RebootRequired
    );
    assert_eq!(
        classify_system_install(&outcome(None, false, false, Some(1223))),
        SystemInstallResult::UacDenied
    );
    assert_eq!(
        classify_system_install(&outcome(None, false, false, Some(2))),
        SystemInstallResult::SpawnFailed(2)
    );
    assert!(matches!(
        classify_system_install(&outcome(None, true, false, None)),
        SystemInstallResult::NeedsVerification(_)
    ));
    assert!(matches!(
        classify_system_install(&outcome(None, false, true, None)),
        SystemInstallResult::NeedsVerification(_)
    ));
}

#[tokio::test]
async fn confirmed_cancellation_has_no_filesystem_or_network_side_effect() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let downloader = MockDownloader::default();
    let runner = MockRunner::default();
    let foundation = runtime_plan(temp.path(), true);
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let supplier = RuntimeSupplier::new(&downloader, &runner, host(), cache.clone(), None).unwrap();
    assert!(matches!(
        supplier
            .execute_confirmed(&foundation, &plan, &plan.supply_plan_id, &cancellation)
            .await,
        Err(SupplyError::Cancelled)
    ));
    assert!(!cache.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn stale_marker_from_a_crashed_supplier_does_not_block_windows_lock_recovery() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let downloader = MockDownloader::default();
    let runner = MockRunner::default();
    let foundation = runtime_plan(temp.path(), true);
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join(".runtime-supply.lock"), "stale").unwrap();

    let supplier = RuntimeSupplier::new(&downloader, &runner, host(), cache.clone(), None).unwrap();
    supplier
        .execute_confirmed(
            &foundation,
            &plan,
            &plan.supply_plan_id,
            &CancellationToken::default(),
        )
        .await
        .unwrap();
    assert!(!cache.join(".runtime-supply.lock").exists());
}

#[cfg(windows)]
#[test]
fn fresh_portable_tree_writes_manifest_and_publishes_with_parent_creation() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    fs::create_dir(&cache).unwrap();
    let staging = create_staging(&cache).unwrap();
    let extracted = staging.join("unpacked/package");
    fs::create_dir_all(extracted.join("bin")).unwrap();
    let entry = b"console.log('fresh portable fixture')";
    fs::write(extracted.join("bin/pnpm.mjs"), entry).unwrap();
    let artifact = ArtifactIdentity {
        kind: ArtifactKind::PnpmTarball,
        locator: "fixture/pnpm-11.7.0.tgz".to_owned(),
        filename: "pnpm-11.7.0.tgz".to_owned(),
        digest_algorithm: "sha512-sri".to_owned(),
        digest: "sha512:fixture".to_owned(),
        signing_key_id: Some("fixture-key".to_owned()),
    };

    write_cache_manifest(
        &extracted,
        "pnpm",
        "11.7.0",
        "bin/pnpm.mjs",
        std::slice::from_ref(&artifact),
    )
    .unwrap();
    let target = cache.join("pnpm/11.7.0");
    assert!(!target.parent().unwrap().exists());
    publish_directory(&extracted, &target).unwrap();

    assert_eq!(fs::read(target.join("bin/pnpm.mjs")).unwrap(), entry);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(target.join(".nexus-runtime.json")).unwrap()).unwrap();
    assert_eq!(manifest["tool"], "pnpm");
    assert_eq!(manifest["version"], "11.7.0");
    assert_eq!(manifest["artifacts"][0]["locator"], artifact.locator);
    assert!(!extracted.exists());
    fs::remove_dir_all(staging).unwrap();
}

#[tokio::test]
async fn concurrent_confirmations_reuse_one_complete_owned_cache() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let package = cache.join("pnpm/11.7.0");
    fs::create_dir_all(package.join("bin")).unwrap();
    let entry = b"console.log('pnpm cache fixture')";
    fs::write(package.join("bin/pnpm.mjs"), entry).unwrap();
    let artifact = ArtifactIdentity {
        kind: ArtifactKind::PnpmTarball,
        locator: "npm/pnpm/11.7.0/pnpm-11.7.0.tgz".to_owned(),
        filename: "pnpm-11.7.0.tgz".to_owned(),
        digest_algorithm: "sha512-sri".to_owned(),
        digest: PNPM_INTEGRITY.to_owned(),
        signing_key_id: Some(NPM_KEY_ID.to_owned()),
    };
    fs::write(
        package.join(".nexus-runtime.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "tool": "pnpm",
            "version": "11.7.0",
            "entry": "bin/pnpm.mjs",
            "entry_sha256": sha256_hex(entry),
            "artifacts": [artifact],
        }))
        .unwrap(),
    )
    .unwrap();
    let downloader = Arc::new(MockDownloader::default());
    downloader.insert(
        "https://registry.npmjs.org/pnpm/11.7.0",
        Reply {
            status: 200,
            location: None,
            body: PNPM_METADATA.as_bytes().to_vec(),
        },
    );
    let runner = Arc::new(MockRunner::default());
    let foundation = runtime_plan(temp.path(), false);
    let planner = RuntimeSupplyPlanner::new(
        downloader.as_ref(),
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    assert_eq!(plan.pnpm.disposition, SupplyDisposition::ReuseOwnedCache);
    let supplier =
        RuntimeSupplier::new(downloader.as_ref(), runner.as_ref(), host(), cache, None).unwrap();
    let first_cancel = CancellationToken::default();
    let second_cancel = CancellationToken::default();
    let (first, second) = tokio::join!(
        supplier.execute_confirmed(&foundation, &plan, &plan.supply_plan_id, &first_cancel),
        supplier.execute_confirmed(&foundation, &plan, &plan.supply_plan_id, &second_cancel),
    );
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");
    assert_eq!(downloader.count(DownloadKind::PnpmTarball), 0);
    assert_eq!(runner.calls.lock().unwrap().len(), 2);
}

#[test]
fn compiled_node_keyring_verifies_a_real_v24_release_manifest() {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(NODE_V24_19_CHECKSUMS_ASC_B64)
        .unwrap();
    let (checksums, signer) = crate::source::verify_node_checksums(&bytes).unwrap();
    assert!(checksums.contains("node-v24.19.0-win-x64.zip"));
    assert_eq!(signer, "5BE8A3F6C8A5C01D106C0AD820B1A390B168D356");
}

#[tokio::test]
async fn planner_selects_highest_satisfying_lts_with_a_host_artifact() {
    use base64::Engine;
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let mut foundation = runtime_plan(temp.path(), true);
    foundation.tools[0].state = RuntimePlanToolState::Missing;
    foundation.tools[0].version = None;
    foundation.tools[0].path = None;
    foundation.tools[0].ownership = None;
    let downloader = MockDownloader::default();
    downloader.insert(
        "https://nodejs.org/dist/index.json",
        Reply {
            status: 200,
            location: None,
            body: br#"[
                {"version":"v25.9.0","lts":false,"files":["win-x64-zip"]},
                {"version":"v24.19.0","lts":"Krypton","files":["win-x64-zip"]},
                {"version":"v22.20.0","lts":"Jod","files":["win-x64-zip"]},
                {"version":"v24.20.0","lts":"Krypton","files":["win-arm64-zip"]}
            ]"#
            .to_vec(),
        },
    );
    downloader.insert(
        "https://nodejs.org/dist/v24.19.0/SHASUMS256.txt.asc",
        Reply {
            status: 200,
            location: None,
            body: base64::engine::general_purpose::STANDARD
                .decode(NODE_V24_19_CHECKSUMS_ASC_B64)
                .unwrap(),
        },
    );
    let planner =
        RuntimeSupplyPlanner::new(&downloader, RuntimeSource::Official, host(), cache, None)
            .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    assert_eq!(plan.node.version, "24.19.0");
    assert_eq!(plan.node.disposition, SupplyDisposition::PublishPortable);
    assert_eq!(
        plan.node.artifacts[0].signing_key_id.as_deref(),
        Some("5BE8A3F6C8A5C01D106C0AD820B1A390B168D356")
    );
}

struct CancelOnPnpmArtifact {
    metadata: Vec<u8>,
}

#[async_trait]
impl DownloadClient for CancelOnPnpmArtifact {
    async fn fetch_bytes_once(
        &self,
        request: &TypedDownloadRequest,
        _max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<TypedDownloadResponse> {
        cancellation.check()?;
        if request.kind() != DownloadKind::PnpmMetadata {
            return Err(SupplyError::Network(
                "unexpected metadata request".to_owned(),
            ));
        }
        Ok(TypedDownloadResponse {
            status: 200,
            redirect_location: None,
            body: self.metadata.clone(),
        })
    }

    async fn fetch_file_once(
        &self,
        request: &TypedDownloadRequest,
        destination: &Path,
        _max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<DownloadReceipt> {
        if request.kind() != DownloadKind::PnpmTarball {
            return Err(SupplyError::Network(
                "unexpected artifact request".to_owned(),
            ));
        }
        fs::write(destination, b"partial")?;
        cancellation.cancel();
        Err(SupplyError::Cancelled)
    }
}

#[tokio::test]
async fn cancellation_during_download_removes_staging_and_never_publishes() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let downloader = CancelOnPnpmArtifact {
        metadata: PNPM_METADATA.as_bytes().to_vec(),
    };
    let runner = MockRunner::default();
    let foundation = runtime_plan(temp.path(), false);
    let planner = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap();
    let plan = planner
        .plan(&foundation, &CancellationToken::default())
        .await
        .unwrap();
    assert_eq!(plan.pnpm.disposition, SupplyDisposition::PublishPortable);
    let cancellation = CancellationToken::default();
    let supplier = RuntimeSupplier::new(&downloader, &runner, host(), cache.clone(), None).unwrap();
    let error = supplier
        .execute_confirmed(&foundation, &plan, &plan.supply_plan_id, &cancellation)
        .await
        .unwrap_err();
    assert!(matches!(error, SupplyError::Cancelled));
    assert!(!plan.pnpm.path.exists());
    assert!(fs::read_dir(&cache).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".staging-")));
}

#[tokio::test]
async fn system_plans_are_typed_and_bind_msi_and_pinned_pnpm_script() {
    use base64::Engine;
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("runtimes");
    let mut node_foundation = runtime_plan(temp.path(), true);
    node_foundation.mode = RuntimeInstallMode::System;
    node_foundation.tools[0].state = RuntimePlanToolState::Missing;
    node_foundation.tools[0].version = None;
    node_foundation.tools[0].path = None;
    node_foundation.tools[0].ownership = None;
    let downloader = MockDownloader::default();
    downloader.insert(
        "https://nodejs.org/dist/index.json",
        Reply {
            status: 200,
            location: None,
            body: br#"[{"version":"v24.19.0","lts":"Krypton","files":["win-x64-msi"]}]"#.to_vec(),
        },
    );
    downloader.insert(
        "https://nodejs.org/dist/v24.19.0/SHASUMS256.txt.asc",
        Reply {
            status: 200,
            location: None,
            body: base64::engine::general_purpose::STANDARD
                .decode(NODE_V24_19_CHECKSUMS_ASC_B64)
                .unwrap(),
        },
    );
    let node_plan = RuntimeSupplyPlanner::new(
        &downloader,
        RuntimeSource::Official,
        host(),
        cache.clone(),
        None,
    )
    .unwrap()
    .plan(&node_foundation, &CancellationToken::default())
    .await
    .unwrap();
    assert_eq!(node_plan.node.disposition, SupplyDisposition::InstallSystem);
    assert_eq!(node_plan.node.artifacts[0].kind, ArtifactKind::NodeMsi);
    assert_eq!(
        node_plan.node.artifacts[0].filename,
        "node-v24.19.0-x64.msi"
    );

    let mut pnpm_foundation = runtime_plan(temp.path(), false);
    pnpm_foundation.mode = RuntimeInstallMode::System;
    downloader.insert(
        "https://registry.npmjs.org/pnpm/11.7.0",
        Reply {
            status: 200,
            location: None,
            body: PNPM_METADATA.as_bytes().to_vec(),
        },
    );
    downloader.insert(
        "https://raw.githubusercontent.com/pnpm/get.pnpm.io/11faaa4bb062a5cdac4c22d1a36645d6a3692b82/install.ps1",
        Reply { status: 200, location: None, body: b"#!/usr/bin/env pwsh\n$env:PNPM_VERSION\n".to_vec() },
    );
    let pnpm_plan =
        RuntimeSupplyPlanner::new(&downloader, RuntimeSource::Official, host(), cache, None)
            .unwrap()
            .plan(&pnpm_foundation, &CancellationToken::default())
            .await
            .unwrap();
    assert_eq!(pnpm_plan.pnpm.disposition, SupplyDisposition::InstallSystem);
    assert_eq!(pnpm_plan.pnpm.artifacts.len(), 2);
    assert_eq!(
        pnpm_plan.pnpm.artifacts[1].kind,
        ArtifactKind::PnpmInstallScript
    );
    let spec = crate::system::SystemInstallSpec::pnpm_user_script(
        temp.path().join("install.ps1"),
        "11.7.0".to_owned(),
        temp.path().join("pnpm.exe"),
        HostArch::X64,
        RuntimeSource::Official,
    );
    assert_eq!(spec.kind(), crate::SystemInstallKind::PnpmUserScript);
    assert_eq!(spec.expected_version(), "11.7.0");
}
