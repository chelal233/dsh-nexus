use std::{fs, io::{self}, path::{Path, PathBuf}, time::{Duration, Instant}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::{NexusPaths, HarnessLaunchSpec};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalHarness {
    pub root: PathBuf,
    pub identity: String,
    pub fingerprint: String,
    pub version: String,
}
fn invalid(s: &str) -> io::Error { io::Error::other(s) }
impl ExternalHarness {
    pub fn inspect(paths: &NexusPaths, root: &Path) -> io::Result<Self> {
        if !root.is_absolute() { return Err(invalid("External Harness requires an absolute directory")); }
        let root = fs::canonicalize(root)?;
        if crate::config_protection::paths_overlap_by_identity(&paths.root, &root)? {
            return Err(invalid("External Harness must not overlap Nexus data or cleanup directories"));
        }
        let manifest = crate::read_regular_file_bounded(&root.join("package.json"), 1024*1024)?.ok_or_else(|| invalid("Harness package.json is missing"))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest)?;
        if manifest["name"] != "@deepseek-ai/dsh-root" { return Err(invalid("Not a supported Harness source directory")); }
        let version = manifest["version"].as_str().filter(|s| !s.is_empty() && s.len()<128).unwrap_or("unknown").to_owned();
        if !root.join("apps/cli/lib/bin.js").is_file() || !root.join("node_modules").is_dir() {
            return Err(invalid("External Harness must already be built with installed dependencies; Nexus will not build or install it"));
        }
        let identity = crate::data_root_identity(&NexusPaths::from_root(root.clone()))?;
        let started = Instant::now(); let mut count=0usize; let mut bytes=0u64; let mut hash=Sha256::new();
        // A planted source directory can nest thousands of levels; the depth
        // bound keeps recursion far from the stack limit while staying above
        // any real dependency tree.
        const MAX_TREE_DEPTH: usize = 512;
        fn visit(root: &Path, path: &Path, depth: usize, hash: &mut Sha256, count: &mut usize, bytes: &mut u64, started: Instant) -> io::Result<()> {
            if depth>MAX_TREE_DEPTH {return Err(invalid("External Harness directory nesting exceeds the supported depth"));}
            let mut entries=Vec::new();
            for entry in fs::read_dir(path)? {
                if *count+entries.len()>=150_000 || started.elapsed()>Duration::from_secs(30) {return Err(invalid("External Harness inspection budget exceeded"));}
                entries.push(entry?);
            }
            entries.sort_by_key(|e|e.file_name());
            for entry in entries {
                if entry.file_name()==".git" { continue; }
                *count+=1; if *count>150_000 || started.elapsed()>Duration::from_secs(30) {return Err(invalid("External Harness inspection budget exceeded"));}
                let p=entry.path(); let metadata=fs::symlink_metadata(&p)?;
                let relative=p.strip_prefix(root).map_err(io::Error::other)?;
                hash.update(serde_json::to_vec(&(relative, metadata.len(), metadata.modified()?.duration_since(std::time::UNIX_EPOCH).map_err(io::Error::other)?.as_nanos()))?);
                if crate::path_is_reparse(&metadata) {
                    let target=fs::canonicalize(&p)?;
                    if !target.starts_with(root) { return Err(invalid("External Harness dependency link escapes its directory")); }
                    hash.update(b"link"); hash.update(target.to_string_lossy().as_bytes()); continue;
                }
                if metadata.is_dir() { hash.update(b"dir"); visit(root,&p,depth+1,hash,count,bytes,started)?; }
                else if metadata.is_file() {
                    hash.update(b"file");
                    if relative==Path::new("package.json") || relative==Path::new("apps/cli/package.json") || relative==Path::new("apps/cli/lib/bin.js") {
                        let data=crate::read_regular_file_bounded(&p,16*1024*1024)?.ok_or_else(||invalid("External entry disappeared"))?;
                        *bytes+=data.len() as u64; hash.update(Sha256::digest(&data));
                    }
                } else { return Err(invalid("External Harness contains an unsupported file")); }
            }
            Ok(())
        }
        visit(&root,&root,0,&mut hash,&mut count,&mut bytes,started)?;
        Ok(Self {root,identity,fingerprint:format!("{:x}",hash.finalize()),version})
    }
    pub fn verify(&self, paths: &NexusPaths) -> io::Result<()> {
        if Self::inspect(paths,&self.root)? != *self { return Err(invalid("External Harness contents or directory changed; confirm the source again")); } Ok(())
    }
    pub fn launch_spec(&self, paths: &NexusPaths) -> HarnessLaunchSpec {
        let mut spec=HarnessLaunchSpec::new("node".into()); spec.mode=nexus_protocol::HarnessLaunchMode::Node;
        spec.args=vec![self.root.join("apps/cli/lib/bin.js").to_string_lossy().into_owned(),"--profile".into(),"{profile}".into()];
        let _=paths; spec.working_dir=Some(self.root.clone()); spec
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf,NexusPaths,PathBuf) {
        let base=std::env::temp_dir().join(format!("external-source-{}",crate::new_instance_id()));
        let paths=NexusPaths::from_root(base.join("nexus"));paths.ensure_directories().unwrap();
        let root=base.join("harness");fs::create_dir_all(root.join("apps/cli/lib")).unwrap();fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("package.json"),br#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
        fs::write(root.join("apps/cli/lib/bin.js"),b"ready").unwrap();
        (base,paths,root)
    }
    #[test]
    fn external_source_changes_require_confirmation_and_do_not_rewrite_program() {
        let (base,paths,root)=fixture();let source=ExternalHarness::inspect(&paths,&root).unwrap();source.verify(&paths).unwrap();
        fs::write(root.join("apps/cli/lib/bin.js"),b"other").unwrap();assert!(source.verify(&paths).is_err());
        let source=ExternalHarness::inspect(&paths,&root).unwrap();source.verify(&paths).unwrap();
        fs::write(root.join("node_modules/new.js"),b"dependency").unwrap();assert!(source.verify(&paths).is_err());
        assert_eq!(fs::read(root.join("apps/cli/lib/bin.js")).unwrap(),b"other");fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn external_source_can_be_cleared_after_directory_disappears_and_remains_protected() {
        let (base,paths,root)=fixture();let source=ExternalHarness::inspect(&paths,&root).unwrap();
        let store=crate::ConfigStore::new(paths.clone());let mut config=crate::NexusConfigFile::default();config.external_harness=Some(source);store.write(&config).unwrap();
        assert!(crate::ensure_harness_homes_preserved(&paths.root,&root).is_err());
        fs::remove_dir_all(&root).unwrap();assert!(store.load().unwrap().external_harness.is_some());
        config.external_harness=None;store.write(&config).unwrap();assert!(store.load().unwrap().external_harness.is_none());
        fs::create_dir(&root).unwrap();
        assert!(crate::ensure_harness_homes_preserved(&paths.root,&root).is_err());fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn external_source_is_independent_of_slots_and_unknown_version_is_allowed() {
        let (base,paths,root)=fixture();fs::write(root.join("package.json"),br#"{"name":"@deepseek-ai/dsh-root"}"#).unwrap();
        let source=ExternalHarness::inspect(&paths,&root).unwrap();assert_eq!(source.version,"unknown");
        let config=crate::NexusConfigFile{external_harness:Some(source.clone()),..Default::default()};crate::ConfigStore::new(paths.clone()).write(&config).unwrap();
        assert_eq!(crate::load_harness_launch_spec(&paths).unwrap().unwrap(),source.launch_spec(&paths));
        assert!(crate::ReleaseStore::new(paths.clone()).load().unwrap().current_release.is_none());
        assert!(ExternalHarness::inspect(&paths,&paths.root).is_err());fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn external_source_history_exceeds_256_without_consuming_data_home_capacity() {
        let (base,paths,root)=fixture();
        let source=ExternalHarness::inspect(&paths,&root).unwrap();
        let store=crate::ConfigStore::new(paths.clone());
        let mut config=crate::NexusConfigFile::default();
        for index in 0..258 {
            let next=base.join(format!("source-{index}"));
            fs::rename(config.external_harness.as_ref().map(|s| &s.root).unwrap_or(&root), &next).unwrap();
            let mut next_source=source.clone(); next_source.root=next;
            config.external_harness=Some(next_source);
            store.write(&config).unwrap();
        }
        config.external_harness=None; store.write(&config).unwrap();
        assert_eq!(protected_locations(&paths).unwrap().len(),258);
        for index in [0,256,257] {
            let path=base.join(format!("source-{index}"));fs::create_dir_all(&path).unwrap();
            assert!(crate::ensure_harness_homes_preserved(&paths.root,&path).is_err());
        }
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn full_source_history_rejects_only_new_roots_without_losing_protection() {
        let (base,paths,root)=fixture(); let source=ExternalHarness::inspect(&paths,&root).unwrap();
        let file=paths.root.join("external-harness-locations.json");
        let mut roots=vec![source.root.clone()];
        // A bounded synthetic record at the byte capacity; no real giant directory is created.
        roots.push(base.join("x".repeat(LOCATION_BYTES as usize - 512)));
        let bytes=serde_json::to_vec(&Locations{schema_version:1,roots}).unwrap();
        assert!(bytes.len() < LOCATION_BYTES as usize);
        fs::write(&file,&bytes).unwrap();
        let mut extra=source.clone(); extra.root=base.join("y".repeat(1024));
        assert!(protect_locations(&paths,Some(&source),Some(&extra)).unwrap_err().to_string().contains("previously confirmed"));
        assert!(fs::read(&file).unwrap()==bytes,"rejected addition changed protection history");
        protect_locations(&paths,None,Some(&source)).unwrap();
        protect_locations(&paths,Some(&source),None).unwrap();
        assert!(fs::read(&file).unwrap()==bytes,"existing source unnecessarily rewrote protection history");
        fs::remove_dir_all(base).unwrap();
    }
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Locations { schema_version:u32, roots:Vec<PathBuf> }
const LOCATION_BYTES: u64 = 1024 * 1024;
pub(crate) fn protected_locations(paths:&NexusPaths)->io::Result<Vec<PathBuf>> {
    let Some(bytes)=crate::read_regular_file_bounded(&paths.root.join("external-harness-locations.json"),LOCATION_BYTES)? else{return Ok(vec![])};
    let saved:Locations=serde_json::from_slice(&bytes)?;
    if saved.schema_version!=1 || saved.roots.iter().any(|p|!p.is_absolute()){return Err(invalid("Invalid external source protection record"));}
    Ok(saved.roots)
}
pub(crate) fn protect_locations(paths:&NexusPaths,old:Option<&ExternalHarness>,new:Option<&ExternalHarness>)->io::Result<()> {
    // Same cross-process union write as the Harness homes record; take the
    // shared protection lock before reading so concurrent writers cannot drop
    // each other's confirmed roots.
    let _lock=crate::config_protection::ProtectionLock::acquire(&paths.root)?;
    let mut roots=protected_locations(paths)?;
    let previous = roots.clone();
    for source in old.into_iter().chain(new){if !roots.contains(&source.root){roots.push(source.root.clone());}}
    if roots == previous { return Ok(()); }
    // Keep every previously confirmed root protected. Bound encoded bytes,
    // rather than blocking all new sources at an arbitrary directory count.
    let bytes = serde_json::to_vec(&Locations{schema_version:1,roots})?;
    if bytes.len() as u64 > LOCATION_BYTES {
        return Err(invalid("External source protection history is full (1 MiB). Keep the current source or choose a previously confirmed directory, then save again. Existing directory protection is retained."));
    }
    crate::write_private_bytes_atomic(&paths.root,&paths.root.join("external-harness-locations.json"),&bytes)?;
    Ok(())
}

#[cfg(test)]
mod real_source_inspection {
    #[test]
    #[ignore = "Explicit read-only external source performance check"]
    fn inspect_existing_external_source_read_only() {
        let root=std::env::var_os("NEXUS_TEST_EXTERNAL_SOURCE").expect("set external source path");
        let paths=crate::NexusPaths::from_root(std::env::temp_dir().join("nexus-inspect-readonly-identity"));
        // This test does not create the source, launch it, or write a config.
        let began=std::time::Instant::now();
        let source=super::ExternalHarness::inspect(&paths,std::path::Path::new(&root)).unwrap();
        eprintln!("Rust external inspect: {:?}; version={}",began.elapsed(),source.version);
        assert!(began.elapsed()<std::time::Duration::from_secs(30));
    }
}
