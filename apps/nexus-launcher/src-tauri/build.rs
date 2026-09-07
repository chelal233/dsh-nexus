fn main() {
    let identity = std::path::Path::new("resources/release-identity.json");
    println!("cargo:rerun-if-changed={}", identity.display());
    println!("cargo:rerun-if-changed=resources/release-manifest.json");
    println!("cargo:rerun-if-changed=scripts/prepare-release.mjs");
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        let node = if cfg!(windows) { "resources/runtime/node/node.exe" } else { "resources/runtime/node/node" };
        let status = std::process::Command::new(node).args(["scripts/prepare-release.mjs", "--verify"])
            .status().expect("Cannot verify staged release resources");
        assert!(status.success(), "Release resource verification failed");
    }
    let bytes = match std::fs::read(identity) {
        Ok(bytes) => bytes,
        Err(error) if std::env::var("PROFILE").as_deref() == Ok("debug")
            && error.kind() == std::io::ErrorKind::NotFound => b"{\"buildId\":\"development\"}".to_vec(),
        Err(error) => panic!("Release identity missing; run pnpm prepare:release before packaging: {error}"),
    };
    std::fs::write(std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("release-identity.json"), bytes).unwrap();
    tauri_build::build();
}
