//! Resolve the selected program source independently of managed version slots.
use std::{io, path::PathBuf};
use nexus_core::{ConfigStore, NexusPaths, ReleaseStore};
#[derive(Debug)]
pub(crate) struct SourceContext { pub root: Option<PathBuf>, pub release_id: Option<String>, pub external: bool }
pub(crate) fn resolve(paths: &NexusPaths, releases: &ReleaseStore) -> io::Result<SourceContext> {
    if let Some(source)=ConfigStore::new(paths.clone()).load()?.external_harness {
        source.verify(paths)?;
        let home=crate::dsh::resolve_dsh_home_for_paths(paths)?;
        if nexus_core::paths_overlap_by_identity(&source.root,&home)? { return Err(io::Error::other("Harness data must not overlap the external program directory")); }
        return Ok(SourceContext {root:Some(source.root),release_id:None,external:true});
    }
    let release_id=releases.load()?.current_release;
    let root=release_id.as_deref().map(|id|releases.release_root(id)).transpose()?;
    Ok(SourceContext{root,release_id,external:false})
}

pub(crate) async fn resolve_async(paths: &NexusPaths, releases: &ReleaseStore) -> io::Result<SourceContext> {
    let paths=paths.clone();let releases=releases.clone();
    tokio::task::spawn_blocking(move||resolve(&paths,&releases)).await.map_err(io::Error::other)?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn external_launch_context_ignores_damaged_old_managed_catalog() {
        let base=std::env::temp_dir().join(format!("external-context-{}",nexus_core::new_instance_id()));
        let paths=NexusPaths::from_root(base.join("nexus"));paths.ensure_directories().unwrap();
        let root=base.join("program");std::fs::create_dir_all(root.join("apps/cli/lib")).unwrap();std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("package.json"),br#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
        std::fs::write(root.join("apps/cli/lib/bin.js"),b"built").unwrap();
        let source=nexus_core::ExternalHarness::inspect(&paths,&root).unwrap();
        let config=nexus_core::NexusConfigFile{external_harness:Some(source.clone()),..Default::default()};
        ConfigStore::new(paths.clone()).write(&config).unwrap();
        std::fs::write(&paths.release_pointers_file,b"damaged old managed catalog").unwrap();
        let releases=ReleaseStore::new(paths.clone());assert!(releases.load().is_err());
        let mut spec=nexus_core::load_harness_launch_spec(&paths).unwrap().unwrap();
        crate::supervisor::normalize_selected_launch(&mut spec,&paths,&releases).unwrap();
        let context=resolve_async(&paths,&releases).await.unwrap();
        assert!(context.external);assert!(context.release_id.is_none());assert_eq!(context.root,Some(source.root.clone()));
        assert_eq!(spec,source.launch_spec(&paths));
        assert!(crate::launch_inputs::next(&paths).is_ok());
        std::fs::remove_dir_all(base).unwrap();
    }
}

pub(crate) fn compatibility_id(paths:&NexusPaths,releases:&ReleaseStore)->io::Result<Option<String>> {
    if let Some(source)=ConfigStore::new(paths.clone()).load()?.external_harness {return Ok(Some(format!("external-{}",source.fingerprint)));}
    Ok(releases.load()?.current_release)
}
