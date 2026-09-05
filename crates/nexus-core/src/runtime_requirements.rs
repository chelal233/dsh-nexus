//! Pure parsing and matching of runtime requirements declared by one
//! registered Harness release. No executable is spawned from this module.

use std::{cmp::Ordering, fs, io, io::Read, path::Path};

use nexus_protocol::{
    RuntimeNodeRequirement, RuntimePackageManagerRequirement, RuntimeRequirements,
};
use serde::Deserialize;

const MAX_PACKAGE_MANIFEST_BYTES: u64 = 256 * 1024;
const PACKAGE_MANIFESTS: &[&str] = &["package.json", "apps/cli/package.json"];

#[derive(Debug, Deserialize)]
struct PackageManifest {
    #[serde(default)]
    engines: Engines,
    #[serde(default, rename = "packageManager")]
    package_manager: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Engines {
    #[serde(default)]
    node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PrereleaseIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrereleaseIdentifier {
    Numeric(u64),
    Text(String),
}

impl Ord for PrereleaseIdentifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(left), Self::Numeric(right)) => left.cmp(right),
            (Self::Numeric(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
        }
    }
}

impl PartialOrd for PrereleaseIdentifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.prerelease.cmp(&other.prerelease),
                },
            )
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
enum Comparator {
    Exact(Version),
    GreaterOrEqual(Version),
    LessThan(Version),
}

impl Comparator {
    fn matches(&self, version: &Version) -> bool {
        match self {
            Self::Exact(required) => version == required,
            Self::GreaterOrEqual(required) => version >= required,
            Self::LessThan(required) => version < required,
        }
    }

    fn version(&self) -> &Version {
        match self {
            Self::Exact(version) | Self::GreaterOrEqual(version) | Self::LessThan(version) => {
                version
            }
        }
    }
}

pub fn load_runtime_requirements(release_root: &Path) -> io::Result<RuntimeRequirements> {
    let canonical_root = fs::canonicalize(release_root)?;
    if !canonical_root.is_dir() {
        return Err(invalid_data("registered release root is not a directory"));
    }
    let mut node = Vec::new();
    let mut package_manager = None::<RuntimePackageManagerRequirement>;
    for relative in PACKAGE_MANIFESTS {
        let manifest = read_package_manifest(&canonical_root, relative)?;
        if let Some(range) = manifest.engines.node {
            let range = range.trim();
            if range.is_empty() {
                return Err(invalid_data(format!(
                    "{relative} engines.node cannot be empty"
                )));
            }
            parse_node_range(range)?;
            node.push(RuntimeNodeRequirement {
                manifest: (*relative).to_owned(),
                range: range.to_owned(),
            });
        }
        if let Some(spec) = manifest.package_manager {
            let candidate = parse_package_manager(relative, &spec)?;
            if let Some(existing) = &package_manager {
                if existing.name != candidate.name || existing.version != candidate.version {
                    return Err(invalid_data(
                        "release manifests declare conflicting packageManager versions",
                    ));
                }
            } else {
                package_manager = Some(candidate);
            }
        }
    }
    if node.is_empty() {
        return Err(invalid_data(
            "release package manifests do not declare engines.node",
        ));
    }
    let package_manager = package_manager
        .ok_or_else(|| invalid_data("release package manifests do not declare packageManager"))?;
    Ok(RuntimeRequirements {
        node,
        package_manager,
    })
}

pub fn node_version_satisfies(range: &str, version: &str) -> io::Result<bool> {
    let alternatives = parse_node_range(range)?;
    let version = parse_version(version)?;
    Ok(alternatives
        .iter()
        .any(|comparators| comparator_set_matches(comparators, &version)))
}

pub fn package_manager_version_matches(
    required_version: &str,
    observed_version: &str,
) -> io::Result<bool> {
    Ok(parse_version(required_version)? == parse_version(observed_version)?)
}

fn read_package_manifest(root: &Path, relative: &str) -> io::Result<PackageManifest> {
    let path = fs::canonicalize(root.join(relative)).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("required release manifest {relative} is unavailable: {error}"),
        )
    })?;
    if !path.starts_with(root) || !path.is_file() {
        return Err(invalid_data(format!(
            "required release manifest {relative} resolves outside the release root"
        )));
    }
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_PACKAGE_MANIFEST_BYTES {
        return Err(invalid_data(format!(
            "required release manifest {relative} exceeds {MAX_PACKAGE_MANIFEST_BYTES} bytes"
        )));
    }
    let mut bytes =
        Vec::with_capacity((metadata.len() as usize).min(MAX_PACKAGE_MANIFEST_BYTES as usize));
    fs::File::open(path)?
        .take(MAX_PACKAGE_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PACKAGE_MANIFEST_BYTES {
        return Err(invalid_data(format!(
            "required release manifest {relative} exceeds {MAX_PACKAGE_MANIFEST_BYTES} bytes"
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        invalid_data(format!(
            "required release manifest {relative} is invalid JSON: {error}"
        ))
    })
}

fn parse_package_manager(
    manifest: &str,
    value: &str,
) -> io::Result<RuntimePackageManagerRequirement> {
    let spec = value.trim();
    let Some((name, version)) = spec.split_once('@') else {
        return Err(invalid_data(format!(
            "{manifest} packageManager must pin pnpm with an exact version"
        )));
    };
    if name != "pnpm" || version.is_empty() || version.starts_with(['^', '>', '<', '~', '=']) {
        return Err(invalid_data(format!(
            "{manifest} packageManager must be an exact pnpm version"
        )));
    }
    parse_version(version)?;
    Ok(RuntimePackageManagerRequirement {
        manifest: manifest.to_owned(),
        spec: spec.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
    })
}

fn parse_node_range(range: &str) -> io::Result<Vec<Vec<Comparator>>> {
    let mut alternatives = Vec::new();
    for alternative in range.split("||") {
        let alternative = alternative.trim();
        if alternative.is_empty() {
            return Err(invalid_data("engines.node contains an empty || branch"));
        }
        let mut comparators = Vec::new();
        for token in alternative.split_whitespace() {
            if let Some(value) = token.strip_prefix('^') {
                let lower = parse_version(value)?;
                let upper = caret_upper_bound(&lower)?;
                comparators.push(Comparator::GreaterOrEqual(lower));
                comparators.push(Comparator::LessThan(upper));
            } else if let Some(value) = token.strip_prefix(">=") {
                comparators.push(Comparator::GreaterOrEqual(parse_version(value)?));
            } else if token.starts_with(['>', '<', '~', '='])
                || token.contains(['*', 'x', 'X'])
                || token == "-"
            {
                return Err(invalid_data(format!(
                    "unsupported engines.node comparator: {token}"
                )));
            } else {
                comparators.push(Comparator::Exact(parse_version(token)?));
            }
        }
        if comparators.is_empty() {
            return Err(invalid_data("engines.node comparator set is empty"));
        }
        alternatives.push(comparators);
    }
    Ok(alternatives)
}

fn comparator_set_matches(comparators: &[Comparator], version: &Version) -> bool {
    if !version.prerelease.is_empty()
        && !comparators.iter().any(|comparator| {
            let bound = comparator.version();
            !bound.prerelease.is_empty()
                && (bound.major, bound.minor, bound.patch)
                    == (version.major, version.minor, version.patch)
        })
    {
        return false;
    }
    comparators
        .iter()
        .all(|comparator| comparator.matches(version))
}

fn caret_upper_bound(version: &Version) -> io::Result<Version> {
    let (major, minor, patch) = if version.major > 0 {
        (
            version
                .major
                .checked_add(1)
                .ok_or_else(|| invalid_data("engines.node caret upper bound overflows"))?,
            0,
            0,
        )
    } else if version.minor > 0 {
        (
            0,
            version
                .minor
                .checked_add(1)
                .ok_or_else(|| invalid_data("engines.node caret upper bound overflows"))?,
            0,
        )
    } else {
        (
            0,
            0,
            version
                .patch
                .checked_add(1)
                .ok_or_else(|| invalid_data("engines.node caret upper bound overflows"))?,
        )
    };
    Ok(Version {
        major,
        minor,
        patch,
        prerelease: Vec::new(),
    })
}

fn parse_version(value: &str) -> io::Result<Version> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    if value.is_empty() || value.contains('+') {
        return Err(invalid_data(format!("invalid semantic version: {value}")));
    }
    let (core, prerelease) = value
        .split_once('-')
        .map_or((value, None), |(core, pre)| (core, Some(pre)));
    let mut components = core.split('.');
    let major = parse_numeric_component(components.next(), value)?;
    let minor = parse_numeric_component(components.next(), value)?;
    let patch = parse_numeric_component(components.next(), value)?;
    if components.next().is_some() {
        return Err(invalid_data(format!("invalid semantic version: {value}")));
    }
    let prerelease = match prerelease {
        None => Vec::new(),
        Some("") => return Err(invalid_data(format!("invalid semantic version: {value}"))),
        Some(prerelease) => prerelease
            .split('.')
            .map(|identifier| parse_prerelease_identifier(identifier, value))
            .collect::<io::Result<Vec<_>>>()?,
    };
    Ok(Version {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn parse_numeric_component(component: Option<&str>, original: &str) -> io::Result<u64> {
    let component = component
        .filter(|component| !component.is_empty())
        .ok_or_else(|| invalid_data(format!("invalid semantic version: {original}")))?;
    if (component.len() > 1 && component.starts_with('0'))
        || !component.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_data(format!(
            "invalid semantic version: {original}"
        )));
    }
    component.parse::<u64>().map_err(|_| {
        invalid_data(format!(
            "semantic version component is too large: {original}"
        ))
    })
}

fn parse_prerelease_identifier(
    identifier: &str,
    original: &str,
) -> io::Result<PrereleaseIdentifier> {
    if identifier.is_empty()
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(invalid_data(format!(
            "invalid semantic version: {original}"
        )));
    }
    if identifier.bytes().all(|byte| byte.is_ascii_digit()) {
        if identifier.len() > 1 && identifier.starts_with('0') {
            return Err(invalid_data(format!(
                "invalid semantic version: {original}"
            )));
        }
        return identifier
            .parse::<u64>()
            .map(PrereleaseIdentifier::Numeric)
            .map_err(|_| invalid_data(format!("invalid semantic version: {original}")));
    }
    Ok(PrereleaseIdentifier::Text(identifier.to_owned()))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{
        load_runtime_requirements, node_version_satisfies, package_manager_version_matches,
    };

    #[test]
    fn supported_node_ranges_cover_boundaries_and_disjunctions() {
        let range = "^22.19.0 || >=24.0.0";
        assert!(node_version_satisfies(range, "v22.19.0").expect("range parses"));
        assert!(node_version_satisfies(range, "22.99.4").expect("range parses"));
        assert!(!node_version_satisfies(range, "23.0.0").expect("range parses"));
        assert!(node_version_satisfies(range, "24.0.0").expect("range parses"));
        assert!(node_version_satisfies("22.19.0", "22.19.0").expect("exact parses"));
        assert!(!node_version_satisfies(">=22.19.0", "22.18.9").expect("gte parses"));
    }

    #[test]
    fn unknown_ranges_fail_closed_and_prereleases_are_explicit() {
        assert!(node_version_satisfies("~22.19.0", "22.19.1").is_err());
        assert!(node_version_satisfies("22.x", "22.19.0").is_err());
        assert!(node_version_satisfies("^18446744073709551615.0.0", "24.0.0").is_err());
        assert!(!node_version_satisfies(">=24.0.0", "24.0.0-rc.1").expect("gte parses"));
        assert!(
            node_version_satisfies("24.0.0-rc.1", "24.0.0-rc.1").expect("exact prerelease parses")
        );
        assert!(package_manager_version_matches("11.7.0", "v11.7.0")
            .expect("package manager versions parse"));
    }

    #[test]
    fn selected_release_manifests_supply_node_and_pnpm_requirements() {
        let root = unique_root("manifest-fixture");
        fs::create_dir_all(root.join("apps/cli")).expect("fixture directories create");
        fs::write(
            root.join("package.json"),
            br#"{"engines":{"node":"^22.19.0 || >=24.0.0"},"packageManager":"pnpm@11.7.0"}"#,
        )
        .expect("root package writes");
        fs::write(
            root.join("apps/cli/package.json"),
            br#"{"engines":{"node":">=22.19.0"}}"#,
        )
        .expect("CLI package writes");

        let requirements = load_runtime_requirements(&root).expect("requirements load");
        assert_eq!(requirements.node.len(), 2);
        assert_eq!(requirements.node[0].manifest, "package.json");
        assert_eq!(requirements.node[1].manifest, "apps/cli/package.json");
        assert_eq!(requirements.package_manager.name, "pnpm");
        assert_eq!(requirements.package_manager.version, "11.7.0");
        let _ = fs::remove_dir_all(root);
    }

    fn unique_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nexus-runtime-requirements-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is after epoch")
                .as_nanos()
        ))
    }
}
