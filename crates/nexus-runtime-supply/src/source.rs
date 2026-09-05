use std::{path::Path, time::Duration};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use nexus_core::runtime_requirements::node_version_satisfies;
use nexus_protocol::{RuntimeInstallMode, RuntimeRequirements, RuntimeSource};
use p256::{
    ecdsa::{signature::Verifier, Signature, VerifyingKey},
    pkcs8::DecodePublicKey,
};
use pgp::{
    composed::{CleartextSignedMessage, Deserializable, SignedPublicKey},
    types::KeyDetails,
};
use reqwest::{header::LOCATION, redirect::Policy};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::{ArtifactIdentity, ArtifactKind, CancellationToken, HostPlatform, Result, SupplyError};

pub const MAX_INDEX_BYTES: u64 = 1024 * 1024;
pub const MAX_METADATA_BYTES: u64 = 1024 * 1024;
pub const MAX_NODE_ARTIFACT_BYTES: u64 = 192 * 1024 * 1024;
pub const MAX_PNPM_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const PNPM_INSTALL_SCRIPT_COMMIT: &str = "11faaa4bb062a5cdac4c22d1a36645d6a3692b82";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownloadKind {
    NodeIndex,
    NodeChecksums,
    NodeArtifact,
    PnpmMetadata,
    PnpmTarball,
    PnpmInstallScript,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedDownloadRequest {
    kind: DownloadKind,
    url: Url,
}

impl TypedDownloadRequest {
    pub fn kind(&self) -> DownloadKind {
        self.kind
    }

    pub fn url(&self) -> &Url {
        &self.url
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedDownloadResponse {
    pub status: u16,
    pub redirect_location: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadReceipt {
    pub status: u16,
    pub redirect_location: Option<String>,
    pub bytes_written: u64,
}

#[async_trait]
pub trait DownloadClient: Send + Sync {
    async fn fetch_bytes_once(
        &self,
        request: &TypedDownloadRequest,
        max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<TypedDownloadResponse>;

    async fn fetch_file_once(
        &self,
        request: &TypedDownloadRequest,
        destination: &Path,
        max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<DownloadReceipt>;
}

pub struct HttpDownloadClient {
    client: reqwest::Client,
}

impl HttpDownloadClient {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(180))
            .user_agent("dsh-nexus-runtime-supply/0.1")
            .build()
            .map_err(|error| SupplyError::Network(error.to_string()))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl DownloadClient for HttpDownloadClient {
    async fn fetch_bytes_once(
        &self,
        request: &TypedDownloadRequest,
        max_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<TypedDownloadResponse> {
        cancellation.check()?;
        let mut response = self
            .client
            .get(request.url.clone())
            .send()
            .await
            .map_err(|error| SupplyError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        let redirect_location = header_location(&response)?;
        if (300..400).contains(&status) {
            return Ok(TypedDownloadResponse {
                status,
                redirect_location,
                body: Vec::new(),
            });
        }
        reject_content_length(response.content_length(), max_bytes)?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| SupplyError::Network(error.to_string()))?
        {
            cancellation.check()?;
            if body.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(SupplyError::Network(format!(
                    "response exceeded {max_bytes} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(TypedDownloadResponse {
            status,
            redirect_location,
            body,
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
        let mut response = self
            .client
            .get(request.url.clone())
            .send()
            .await
            .map_err(|error| SupplyError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        let redirect_location = header_location(&response)?;
        if (300..400).contains(&status) {
            return Ok(DownloadReceipt {
                status,
                redirect_location,
                bytes_written: 0,
            });
        }
        reject_content_length(response.content_length(), max_bytes)?;
        let mut output = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .await?;
        let mut bytes_written = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| SupplyError::Network(error.to_string()))?
        {
            cancellation.check()?;
            bytes_written = bytes_written
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| SupplyError::Network("response length overflow".to_owned()))?;
            if bytes_written > max_bytes {
                return Err(SupplyError::Network(format!(
                    "response exceeded {max_bytes} bytes"
                )));
            }
            output.write_all(&chunk).await?;
        }
        output.flush().await?;
        output.sync_all().await?;
        Ok(DownloadReceipt {
            status,
            redirect_location,
            bytes_written,
        })
    }
}

fn header_location(response: &reqwest::Response) -> Result<Option<String>> {
    response
        .headers()
        .get(LOCATION)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| SupplyError::Network("redirect Location is not UTF-8".to_owned()))
        })
        .transpose()
}

fn reject_content_length(length: Option<u64>, maximum: u64) -> Result<()> {
    if length.is_some_and(|length| length > maximum) {
        return Err(SupplyError::Network(format!(
            "declared response length exceeds {maximum} bytes"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SourcePolicy {
    source: RuntimeSource,
}

impl SourcePolicy {
    pub fn new(source: RuntimeSource) -> Self {
        Self { source }
    }

    pub fn source(&self) -> RuntimeSource {
        self.source
    }

    pub(crate) fn node_index(&self) -> Result<TypedDownloadRequest> {
        self.request(DownloadKind::NodeIndex, self.node_url("index.json")?)
    }

    pub(crate) fn node_checksums(&self, version: &str) -> Result<TypedDownloadRequest> {
        validate_exact_version(version)?;
        self.request(
            DownloadKind::NodeChecksums,
            self.node_url(&format!("v{version}/SHASUMS256.txt.asc"))?,
        )
    }

    pub(crate) fn node_artifact(
        &self,
        version: &str,
        filename: &str,
    ) -> Result<TypedDownloadRequest> {
        validate_exact_version(version)?;
        validate_filename(filename)?;
        self.request(
            DownloadKind::NodeArtifact,
            self.node_url(&format!("v{version}/{filename}"))?,
        )
    }

    pub(crate) fn pnpm_metadata(&self, version: &str) -> Result<TypedDownloadRequest> {
        validate_exact_version(version)?;
        self.request(
            DownloadKind::PnpmMetadata,
            Url::parse(&format!("{}/pnpm/{version}", self.registry_base()))
                .map_err(|error| SupplyError::InvalidPlan(error.to_string()))?,
        )
    }

    pub(crate) fn pnpm_tarball(&self, version: &str) -> Result<TypedDownloadRequest> {
        validate_exact_version(version)?;
        self.request(
            DownloadKind::PnpmTarball,
            Url::parse(&format!(
                "{}/pnpm/-/pnpm-{version}.tgz",
                self.registry_base()
            ))
            .map_err(|error| SupplyError::InvalidPlan(error.to_string()))?,
        )
    }

    pub(crate) fn pnpm_install_script(&self) -> Result<TypedDownloadRequest> {
        self.request(
            DownloadKind::PnpmInstallScript,
            Url::parse(&format!(
                "https://raw.githubusercontent.com/pnpm/get.pnpm.io/{PNPM_INSTALL_SCRIPT_COMMIT}/install.ps1"
            ))
            .map_err(|error| SupplyError::InvalidPlan(error.to_string()))?,
        )
    }

    pub(crate) fn artifact_request(
        &self,
        artifact: &ArtifactIdentity,
        version: &str,
    ) -> Result<TypedDownloadRequest> {
        let expected_locator = match artifact.kind {
            ArtifactKind::NodeZip | ArtifactKind::NodeMsi => {
                format!("node/v{version}/{}", artifact.filename)
            }
            ArtifactKind::PnpmTarball => {
                format!("npm/pnpm/{version}/pnpm-{version}.tgz")
            }
            ArtifactKind::PnpmInstallScript => {
                format!("pnpm-installer/{PNPM_INSTALL_SCRIPT_COMMIT}/install.ps1")
            }
        };
        if artifact.locator != expected_locator {
            return Err(SupplyError::InvalidPlan(format!(
                "artifact locator is not derived from source policy: {}",
                artifact.locator
            )));
        }
        match artifact.kind {
            ArtifactKind::NodeZip | ArtifactKind::NodeMsi => {
                self.node_artifact(version, &artifact.filename)
            }
            ArtifactKind::PnpmTarball => self.pnpm_tarball(version),
            ArtifactKind::PnpmInstallScript => self.pnpm_install_script(),
        }
    }

    fn request(&self, kind: DownloadKind, url: Url) -> Result<TypedDownloadRequest> {
        self.validate_url(kind, &url)?;
        Ok(TypedDownloadRequest { kind, url })
    }

    fn node_url(&self, relative: &str) -> Result<Url> {
        let base = match self.source {
            RuntimeSource::Official => "https://nodejs.org/dist/",
            RuntimeSource::Npmmirror => "https://npmmirror.com/mirrors/node/",
        };
        Url::parse(base)
            .and_then(|base| base.join(relative))
            .map_err(|error| SupplyError::InvalidPlan(error.to_string()))
    }

    fn registry_base(&self) -> &'static str {
        match self.source {
            RuntimeSource::Official => "https://registry.npmjs.org",
            RuntimeSource::Npmmirror => "https://registry.npmmirror.com",
        }
    }

    fn validate_url(&self, kind: DownloadKind, url: &Url) -> Result<()> {
        if url.scheme() != "https"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(SupplyError::Network(
                "download URL violates strict HTTPS policy".to_owned(),
            ));
        }
        let host = url.host_str().unwrap_or_default();
        let path = url.path();
        let allowed = match kind {
            DownloadKind::NodeIndex | DownloadKind::NodeChecksums | DownloadKind::NodeArtifact => {
                match self.source {
                    RuntimeSource::Official => host == "nodejs.org" && path.starts_with("/dist/"),
                    RuntimeSource::Npmmirror => {
                        (host == "npmmirror.com" && path.starts_with("/mirrors/node/"))
                            || (host == "cdn.npmmirror.com" && path.starts_with("/binaries/node/"))
                    }
                }
            }
            DownloadKind::PnpmMetadata | DownloadKind::PnpmTarball => {
                (match self.source {
                    RuntimeSource::Official => host == "registry.npmjs.org",
                    RuntimeSource::Npmmirror => host == "registry.npmmirror.com",
                }) && path.starts_with("/pnpm/")
            }
            DownloadKind::PnpmInstallScript => {
                host == "raw.githubusercontent.com"
                    && path == format!("/pnpm/get.pnpm.io/{PNPM_INSTALL_SCRIPT_COMMIT}/install.ps1")
            }
        };
        if !allowed {
            return Err(SupplyError::Network(format!(
                "download URL is outside the compiled source policy: {url}"
            )));
        }
        Ok(())
    }

    fn follow_redirect(
        &self,
        current: &TypedDownloadRequest,
        location: &str,
    ) -> Result<TypedDownloadRequest> {
        let next = current
            .url
            .join(location)
            .map_err(|error| SupplyError::Network(format!("invalid redirect: {error}")))?;
        if next == current.url || !same_resource_after_redirect(current.kind, &current.url, &next) {
            return Err(SupplyError::Network(
                "redirect changed the requested artifact identity".to_owned(),
            ));
        }
        self.request(current.kind, next)
    }
}

fn same_resource_after_redirect(kind: DownloadKind, current: &Url, next: &Url) -> bool {
    match kind {
        DownloadKind::NodeIndex | DownloadKind::NodeChecksums | DownloadKind::NodeArtifact => {
            node_resource_suffix(current).is_some()
                && node_resource_suffix(current) == node_resource_suffix(next)
        }
        DownloadKind::PnpmMetadata
        | DownloadKind::PnpmTarball
        | DownloadKind::PnpmInstallScript => current.path() == next.path(),
    }
}

fn node_resource_suffix(url: &Url) -> Option<&str> {
    url.path()
        .strip_prefix("/mirrors/node/")
        .or_else(|| url.path().strip_prefix("/binaries/node/"))
        .or_else(|| url.path().strip_prefix("/dist/"))
}

pub(crate) async fn fetch_bytes<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    mut request: TypedDownloadRequest,
    maximum: u64,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>> {
    for redirect_count in 0..=MAX_REDIRECTS {
        let response = downloader
            .fetch_bytes_once(&request, maximum, cancellation)
            .await?;
        if (200..300).contains(&response.status) {
            return Ok(response.body);
        }
        if (300..400).contains(&response.status) {
            if redirect_count == MAX_REDIRECTS {
                return Err(SupplyError::Network("too many redirects".to_owned()));
            }
            let location = response.redirect_location.ok_or_else(|| {
                SupplyError::Network("redirect response omitted Location".to_owned())
            })?;
            request = policy.follow_redirect(&request, &location)?;
            continue;
        }
        return Err(SupplyError::Network(format!(
            "unexpected HTTP status {}",
            response.status
        )));
    }
    Err(SupplyError::Network("too many redirects".to_owned()))
}

pub(crate) async fn fetch_file<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    mut request: TypedDownloadRequest,
    destination: &Path,
    maximum: u64,
    cancellation: &CancellationToken,
) -> Result<u64> {
    for redirect_count in 0..=MAX_REDIRECTS {
        let response = downloader
            .fetch_file_once(&request, destination, maximum, cancellation)
            .await?;
        if (200..300).contains(&response.status) {
            return Ok(response.bytes_written);
        }
        if (300..400).contains(&response.status) {
            if redirect_count == MAX_REDIRECTS {
                return Err(SupplyError::Network("too many redirects".to_owned()));
            }
            let location = response.redirect_location.ok_or_else(|| {
                SupplyError::Network("redirect response omitted Location".to_owned())
            })?;
            request = policy.follow_redirect(&request, &location)?;
            continue;
        }
        return Err(SupplyError::Network(format!(
            "unexpected HTTP status {}",
            response.status
        )));
    }
    Err(SupplyError::Network("too many redirects".to_owned()))
}

#[derive(Debug, Deserialize)]
struct NodeIndexEntry {
    version: String,
    #[serde(default)]
    lts: serde_json::Value,
    #[serde(default)]
    files: Vec<String>,
}

pub(crate) async fn select_node_version<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    requirements: &RuntimeRequirements,
    host: HostPlatform,
    mode: RuntimeInstallMode,
    cancellation: &CancellationToken,
) -> Result<String> {
    let bytes = fetch_bytes(
        downloader,
        policy,
        policy.node_index()?,
        MAX_INDEX_BYTES,
        cancellation,
    )
    .await?;
    let entries: Vec<NodeIndexEntry> = serde_json::from_slice(&bytes)
        .map_err(|error| SupplyError::Network(format!("invalid Node index: {error}")))?;
    if entries.len() > 4096 {
        return Err(SupplyError::Network(
            "Node index contains too many releases".to_owned(),
        ));
    }
    let file_marker = match mode {
        RuntimeInstallMode::Portable => format!("win-{}-zip", host.node_arch()),
        RuntimeInstallMode::System => format!("win-{}-msi", host.node_arch()),
    };
    let mut candidates = entries
        .into_iter()
        .filter_map(|entry| {
            let version_text = entry.version.strip_prefix('v')?.to_owned();
            let version = Version::parse(&version_text).ok()?;
            if !version.pre.is_empty()
                || !entry.lts.as_bool().unwrap_or(false)
                    && !entry.lts.as_str().is_some_and(|value| !value.is_empty())
                || !entry.files.iter().any(|file| file == &file_marker)
            {
                return None;
            }
            let satisfies = requirements.node.iter().all(|requirement| {
                node_version_satisfies(&requirement.range, &version_text).unwrap_or(false)
            });
            satisfies.then_some((version, version_text))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, text)| text)
        .ok_or_else(|| {
            SupplyError::InvalidPlan(format!(
                "no published LTS Node {file_marker} artifact satisfies every engines.node range"
            ))
        })
}

pub(crate) fn node_filename(
    version: &str,
    host: HostPlatform,
    mode: RuntimeInstallMode,
) -> Result<(ArtifactKind, String)> {
    validate_exact_version(version)?;
    let kind = match mode {
        RuntimeInstallMode::Portable => ArtifactKind::NodeZip,
        RuntimeInstallMode::System => ArtifactKind::NodeMsi,
    };
    let filename = match mode {
        RuntimeInstallMode::Portable => {
            format!("node-v{version}-win-{}.zip", host.node_arch())
        }
        RuntimeInstallMode::System => format!("node-v{version}-{}.msi", host.node_arch()),
    };
    Ok((kind, filename))
}

pub(crate) async fn resolve_node_artifact<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    version: &str,
    host: HostPlatform,
    mode: RuntimeInstallMode,
    cancellation: &CancellationToken,
) -> Result<ArtifactIdentity> {
    let armored = fetch_bytes(
        downloader,
        policy,
        policy.node_checksums(version)?,
        MAX_METADATA_BYTES,
        cancellation,
    )
    .await?;
    let (checksums, signer) = verify_node_checksums(&armored)?;
    let (kind, filename) = node_filename(version, host, mode)?;
    let digest = checksum_for(&checksums, &filename)?;
    Ok(ArtifactIdentity {
        kind,
        locator: format!("node/v{version}/{filename}"),
        filename,
        digest_algorithm: "sha256".to_owned(),
        digest,
        signing_key_id: Some(signer),
    })
}

pub(crate) fn verify_node_checksums(armored: &[u8]) -> Result<(String, String)> {
    let armored = std::str::from_utf8(armored)
        .map_err(|_| SupplyError::Integrity("Node checksum signature is not UTF-8".to_owned()))?;
    let (message, _) = CleartextSignedMessage::from_string(armored)
        .map_err(|error| SupplyError::Integrity(format!("invalid Node signature: {error}")))?;
    let keyring = std::str::from_utf8(include_bytes!("../trust/node-release-keys.asc"))
        .map_err(|_| SupplyError::Integrity("compiled Node keys are not UTF-8".to_owned()))?;
    let mut verify_errors = Vec::new();
    let mut key_count = 0_usize;
    for tail in keyring
        .split("-----BEGIN PGP PUBLIC KEY BLOCK-----")
        .skip(1)
    {
        let Some((body, _)) = tail.split_once("-----END PGP PUBLIC KEY BLOCK-----") else {
            return Err(SupplyError::Integrity(
                "compiled Node key block is unterminated".to_owned(),
            ));
        };
        let armored = format!(
            "-----BEGIN PGP PUBLIC KEY BLOCK-----{body}-----END PGP PUBLIC KEY BLOCK-----\n"
        );
        let (key, _) = SignedPublicKey::from_string(&armored).map_err(|error| {
            SupplyError::Integrity(format!("invalid compiled Node key: {error}"))
        })?;
        key_count += 1;
        let key_fingerprint = format!("{:X}", key.fingerprint());
        match message.verify(&key) {
            Ok(_) => {
                return Ok((
                    message.text().to_owned(),
                    format!("{:X}", key.fingerprint()),
                ))
            }
            Err(error)
                if message.signatures().iter().any(|signature| {
                    signature
                        .issuer_fingerprint()
                        .iter()
                        .any(|issuer| format!("{issuer:X}") == key_fingerprint)
                }) =>
            {
                verify_errors.push(error.to_string())
            }
            Err(_) => {}
        }
    }
    if key_count == 0 {
        return Err(SupplyError::Integrity(
            "compiled Node keyring is empty".to_owned(),
        ));
    }
    let issuers = message
        .signatures()
        .iter()
        .flat_map(|signature| signature.issuer_fingerprint())
        .map(|fingerprint| format!("{fingerprint:X}"))
        .collect::<Vec<_>>()
        .join(",");
    Err(SupplyError::Integrity(format!(
        "Node checksum manifest has no signature from a compiled release key (issuer={issuers}; detail={})",
        verify_errors.join(" | ")
    )))
}

pub(crate) fn checksum_for(checksums: &str, filename: &str) -> Result<String> {
    let mut found = None;
    for line in checksums.lines() {
        let mut fields = line.split_whitespace();
        let Some(digest) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            return Err(SupplyError::Integrity(format!(
                "malformed Node checksum line: {line:?}"
            )));
        };
        if fields.next().is_some()
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !valid_checksum_name(name)
        {
            return Err(SupplyError::Integrity(format!(
                "malformed Node checksum line: {line:?}"
            )));
        }
        if name == filename && found.replace(digest.to_ascii_lowercase()).is_some() {
            return Err(SupplyError::Integrity(format!(
                "duplicate checksum for {filename}"
            )));
        }
    }
    found.ok_or_else(|| SupplyError::Integrity(format!("Node checksum manifest omits {filename}")))
}

fn valid_checksum_name(name: &str) -> bool {
    let path = Path::new(name);
    !name.is_empty()
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

#[derive(Debug, Deserialize)]
struct NpmMetadata {
    name: String,
    version: String,
    dist: NpmDist,
    #[serde(default)]
    bin: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct NpmDist {
    integrity: String,
    #[serde(default)]
    signatures: Vec<NpmSignature>,
}

#[derive(Debug, Deserialize)]
struct NpmSignature {
    keyid: String,
    sig: String,
}

#[derive(Debug, Deserialize)]
struct NpmKeys {
    keys: Vec<NpmKey>,
}

#[derive(Debug, Deserialize)]
struct NpmKey {
    keyid: String,
    keytype: String,
    scheme: String,
    key: String,
}

pub(crate) async fn resolve_pnpm_artifact<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    version: &str,
    cancellation: &CancellationToken,
) -> Result<ArtifactIdentity> {
    let bytes = fetch_bytes(
        downloader,
        policy,
        policy.pnpm_metadata(version)?,
        MAX_METADATA_BYTES,
        cancellation,
    )
    .await?;
    let (integrity, keyid) = verify_pnpm_metadata(&bytes, version)?;
    Ok(ArtifactIdentity {
        kind: ArtifactKind::PnpmTarball,
        locator: format!("npm/pnpm/{version}/pnpm-{version}.tgz"),
        filename: format!("pnpm-{version}.tgz"),
        digest_algorithm: "sha512-sri".to_owned(),
        digest: integrity,
        signing_key_id: Some(keyid),
    })
}

pub(crate) fn verify_pnpm_metadata(
    bytes: &[u8],
    expected_version: &str,
) -> Result<(String, String)> {
    let metadata: NpmMetadata = serde_json::from_slice(bytes)
        .map_err(|error| SupplyError::Integrity(format!("invalid pnpm metadata: {error}")))?;
    if metadata.name != "pnpm" || metadata.version != expected_version {
        return Err(SupplyError::Integrity(
            "npm metadata package identity does not match pnpm requirement".to_owned(),
        ));
    }
    let bin = metadata
        .bin
        .get("pnpm")
        .and_then(serde_json::Value::as_str)
        .or_else(|| metadata.bin.as_str());
    if bin != Some("bin/pnpm.mjs") {
        return Err(SupplyError::Integrity(
            "pnpm metadata does not declare bin/pnpm.mjs".to_owned(),
        ));
    }
    let encoded = metadata
        .dist
        .integrity
        .strip_prefix("sha512-")
        .ok_or_else(|| SupplyError::Integrity("pnpm requires SHA-512 SRI".to_owned()))?;
    let digest = STANDARD
        .decode(encoded)
        .map_err(|_| SupplyError::Integrity("invalid pnpm SRI".to_owned()))?;
    if digest.len() != 64 {
        return Err(SupplyError::Integrity(
            "pnpm SHA-512 SRI has the wrong length".to_owned(),
        ));
    }
    let keys: NpmKeys = serde_json::from_slice(include_bytes!("../trust/npm-keys.json"))
        .map_err(|error| SupplyError::Integrity(format!("invalid compiled npm keys: {error}")))?;
    let payload = format!("pnpm@{expected_version}:{}", metadata.dist.integrity);
    for signature in metadata.dist.signatures {
        let Some(key) = keys.keys.iter().find(|key| key.keyid == signature.keyid) else {
            continue;
        };
        if key.keytype != "ecdsa-sha2-nistp256" || key.scheme != "ecdsa-sha2-nistp256" {
            continue;
        }
        let key_der = STANDARD
            .decode(&key.key)
            .map_err(|_| SupplyError::Integrity("invalid compiled npm key".to_owned()))?;
        let verifying_key = VerifyingKey::from_public_key_der(&key_der)
            .map_err(|_| SupplyError::Integrity("invalid compiled npm P-256 key".to_owned()))?;
        let signature_der = STANDARD
            .decode(&signature.sig)
            .map_err(|_| SupplyError::Integrity("invalid npm signature encoding".to_owned()))?;
        let signature_value = Signature::from_der(&signature_der)
            .map_err(|_| SupplyError::Integrity("invalid npm P-256 signature".to_owned()))?;
        if verifying_key
            .verify(payload.as_bytes(), &signature_value)
            .is_ok()
        {
            return Ok((metadata.dist.integrity, signature.keyid));
        }
    }
    Err(SupplyError::Integrity(
        "pnpm metadata has no valid signature from a compiled npm key".to_owned(),
    ))
}

pub(crate) async fn resolve_pnpm_script<D: DownloadClient + ?Sized>(
    downloader: &D,
    policy: &SourcePolicy,
    cancellation: &CancellationToken,
) -> Result<ArtifactIdentity> {
    let bytes = fetch_bytes(
        downloader,
        policy,
        policy.pnpm_install_script()?,
        MAX_METADATA_BYTES,
        cancellation,
    )
    .await?;
    if !bytes.starts_with(b"#!/usr/bin/env pwsh")
        || !bytes.windows(12).any(|window| window == b"PNPM_VERSION")
    {
        return Err(SupplyError::Integrity(
            "pinned pnpm installer script shape is unexpected".to_owned(),
        ));
    }
    Ok(ArtifactIdentity {
        kind: ArtifactKind::PnpmInstallScript,
        locator: format!("pnpm-installer/{PNPM_INSTALL_SCRIPT_COMMIT}/install.ps1"),
        filename: "install.ps1".to_owned(),
        digest_algorithm: "sha256".to_owned(),
        digest: format!("{:x}", Sha256::digest(&bytes)),
        signing_key_id: None,
    })
}

pub(crate) fn verify_file_digest(path: &Path, identity: &ArtifactIdentity) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SupplyError::Integrity(
            "artifact is not a regular file".to_owned(),
        ));
    }
    let actual = match identity.digest_algorithm.as_str() {
        "sha256" => {
            use std::io::Read;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            format!("{:x}", digest.finalize())
        }
        "sha512-sri" => {
            use std::io::Read;
            let mut digest = Sha512::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            format!("sha512-{}", STANDARD.encode(digest.finalize()))
        }
        other => {
            return Err(SupplyError::InvalidPlan(format!(
                "unsupported artifact digest: {other}"
            )))
        }
    };
    if actual != identity.digest {
        return Err(SupplyError::Integrity(format!(
            "{} does not match its confirmed digest",
            identity.filename
        )));
    }
    Ok(())
}

pub(crate) fn validate_exact_version(version: &str) -> Result<()> {
    let parsed = Version::parse(version)
        .map_err(|_| SupplyError::InvalidPlan(format!("invalid exact version: {version}")))?;
    if !parsed.pre.is_empty() || !parsed.build.is_empty() || version.starts_with('v') {
        return Err(SupplyError::InvalidPlan(format!(
            "version is not a stable exact version: {version}"
        )));
    }
    Ok(())
}

fn validate_filename(filename: &str) -> Result<()> {
    if filename.is_empty()
        || filename.contains(['/', '\\', ':'])
        || filename.chars().any(char::is_control)
    {
        return Err(SupplyError::InvalidPlan(
            "artifact filename is not a single safe component".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
