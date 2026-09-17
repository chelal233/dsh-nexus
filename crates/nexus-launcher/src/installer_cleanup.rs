//! Explicit uninstall cleanup, compiled into the temporary installer payload.
//! Never follow junctions or recursively erase the user's entire data root.
use std::{ffi::OsString, fs, io, path::{Path, PathBuf}};

const OWNED: &[&str] = &[
    "config.json", nexus_core::PREVIOUS_CONFIG_FILE, "state.json", "profiles.json", "release-pointers.json",
    "update-state.json", "install-operation.json", "cold-operation.json", "cold-publication.json", "request-receipts.json",
    "logs", "checkpoints", "releases", "runtimes", "downloads", "diagnostics", "run",
    "compatibility", "snapshots", "snapshot-transactions", "canary",
];

#[cfg(test)]
const PROTECTED_HOMES: &str = nexus_core::PROTECTED_HARNESS_HOMES_FILE;

fn root_argument(args: &[OsString]) -> Result<PathBuf, String> {
    if !matches!(args.len(), 4 | 6) || args[0] != "--data-dir" || args[2] != "--install-dir" {
        return Err("expected installer-cleanup --data-dir PATH --install-dir PATH [--locale LANGID]".into());
    }
    if args.len() == 6 && (args[4] != "--locale" || !matches!(args[5].to_str(), Some("1033" | "2052" | "4100" | "1028" | "3076" | "5124"))) {
        return Err("unsupported installer locale; expected an English or Chinese LANGID".into());
    }
    let root = PathBuf::from(&args[1]);
    if !root.is_absolute() || root.file_name() != Some(std::ffi::OsStr::new("Nexus"))
        || root.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("cleanup requires an absolute default Nexus data directory".into());
    }
    Ok(root)
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    { metadata.file_type().is_symlink() }
}

fn protected(path: &Path, homes: &[&Path]) -> bool {
    path.file_name().is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(".dsh"))
        || homes.iter().any(|home| same_path(path, home))
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    fn same_component(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
        use std::os::windows::ffi::OsStrExt;
        let a: Vec<_> = a.encode_wide().collect();
        let b: Vec<_> = b.encode_wide().collect();
        unsafe { windows_sys::Win32::Globalization::CompareStringOrdinal(
            a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1) == 2 }
    }
    #[cfg(not(windows))]
    fn same_component(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool { a == b }
    let mut left = left.components();
    let mut right = right.components();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if same_component(a.as_os_str(), b.as_os_str()) => {},
            _ => return false,
        }
    }
}

fn remove_owned(path: &Path, homes: &[&Path]) -> io::Result<()> {
    if protected(path, homes) { return Ok(()); }
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Preserve links themselves as well as their targets. This also covers
    // mounted directories and unexpected reparse types, not just symlinks.
    if is_link(&metadata) { return Ok(()); }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_owned(&entry?.path(), homes)?;
        }
        if fs::read_dir(path)?.next().is_none() { fs::remove_dir(path)?; }
    } else {
        if metadata.permissions().readonly() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(path, permissions)?;
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

fn cleanup(root: &Path, home: Option<&Path>) -> io::Result<()> {
    // Inspect lexical ancestry before canonicalization can hide a junction.
    for ancestor in root.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_link(&metadata) => return Err(io::Error::other("Nexus data path contains a link; data was preserved")),
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(error),
        }
    }
    if !root.exists() { return Ok(()); }
    let resolved_root = fs::canonicalize(root)?;
    let root = resolved_root.as_path();
    let persisted_home = nexus_core::configured_harness_home(root)?;
    let candidates: Vec<_> = home.into_iter().chain(persisted_home.as_deref()).map(Path::to_owned).collect();
    let saved_homes = nexus_core::protect_harness_homes(root, &candidates)?;
    let homes: Vec<&Path> = saved_homes.iter().map(PathBuf::as_path).collect();
    // A custom Harness home that contains this data root must stay intact.
    if homes.iter().any(|home| root.ancestors().any(|ancestor| same_path(ancestor, home))) {
        return Err(io::Error::other("Nexus data root is inside DSH_HOME; data was preserved"));
    }
    for name in OWNED { remove_owned(&root.join(name), &homes)?; }
    if fs::read_dir(root)?.next().is_none() { fs::remove_dir(root)?; }
    Ok(())
}

#[cfg(windows)]
fn message(text: &str, flags: u32, chinese: bool) -> i32 {
    // The uninstall helper runs after the desktop UI has closed. Use a Nexus
    // dialog template here; never require Electron just to confirm data removal.
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe extern "system" fn procedure(window: windows_sys::Win32::Foundation::HWND, message: u32, wparam: usize, _: isize) -> isize {
        match message {
            WM_COMMAND => {
                let id = (wparam & 0xffff) as i32;
                if matches!(id, IDYES | IDNO | IDOK | IDCANCEL) {
                    unsafe { EndDialog(window, if id == IDCANCEL { IDNO } else { id } as isize); }
                    return 1;
                }
                0
            }
            WM_CLOSE => { unsafe { EndDialog(window, IDNO as isize); } 1 }
            WM_INITDIALOG => 1,
            _ => 0,
        }
    }
    fn dword(words: &mut Vec<u16>, value: u32) { words.extend([value as u16, (value >> 16) as u16]); }
    fn string(words: &mut Vec<u16>, value: &str) { words.extend(value.encode_utf16()); words.push(0); }
    fn control(words: &mut Vec<u16>, style: u32, rect: [u16; 4], id: u16, class: u16, text: &str) {
        if words.len() % 2 != 0 { words.push(0); }
        dword(words, style | WS_CHILD | WS_VISIBLE); dword(words, 0);
        words.extend(rect); words.extend([id, 0xffff, class]); string(words, text); words.push(0);
    }
    let question = flags & MB_YESNO == MB_YESNO;
    let mut words = Vec::new();
    dword(&mut words, WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME as u32 | DS_SETFONT as u32 | DS_CENTER as u32);
    dword(&mut words, 0);
    words.extend([if question { 3 } else { 2 }, 0, 0, 420, 220, 0, 0]);
    string(&mut words, if chinese { "卸载 Nexus Launcher" } else { "Nexus Launcher Uninstall" });
    words.push(10); string(&mut words, "Segoe UI");
    control(&mut words, WS_BORDER | WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32 | ES_AUTOVSCROLL as u32,
        [14, 14, 392, 164], 100, 0x81, text);
    if question {
        control(&mut words, WS_TABSTOP | BS_DEFPUSHBUTTON as u32, [206, 190, 96, 22], IDNO as u16, 0x80,
            if chinese { "保留数据" } else { "Keep data" });
        control(&mut words, WS_TABSTOP | BS_PUSHBUTTON as u32, [310, 190, 96, 22], IDYES as u16, 0x80,
            if chinese { "删除 Nexus 数据" } else { "Remove Nexus data" });
    } else {
        control(&mut words, WS_TABSTOP | BS_DEFPUSHBUTTON as u32, [310, 190, 96, 22], IDOK as u16, 0x80,
            if chinese { "关闭" } else { "Close" });
    }
    if words.len() % 2 != 0 { words.push(0); }
    // DLGTEMPLATE and each item require DWORD alignment.
    let template: Vec<u32> = words.chunks_exact(2).map(|v| v[0] as u32 | ((v[1] as u32) << 16)).collect();
    let result = unsafe { DialogBoxIndirectParamW(std::ptr::null_mut(), template.as_ptr().cast(), std::ptr::null_mut(), Some(procedure), 0) };
    if result == IDYES as isize { IDYES } else { IDNO }
}

pub fn run(args: Vec<OsString>) -> Result<(), String> {
    let root = root_argument(&args)?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_YESNO, MB_ICONQUESTION, MB_DEFBUTTON2, MB_OK, MB_ICONWARNING};
        if !root.exists() { return Ok(()); }
        let chinese = installer_chinese(args.get(5).and_then(|v| v.to_str()), unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() });
        let question = if chinese {
            format!("是否删除 {} 中的 Nexus 数据？\n\n删除 Nexus 数据：删除 Nexus 设置、安装历史、日志、快照、已下载版本和缓存的运行时。\n保留数据（默认）：保留这些数据，以便重新安装。\n\n保留 .dsh 中的 Harness 会话和插件、自定义 NEXUS_DATA_DIR 位置、链接及无法识别的文件。", root.display())
        } else {
            format!("Remove Nexus data from {}?\n\nRemove Nexus data: remove Nexus settings, installation history, logs, snapshots, downloaded versions and cached runtimes.\nKeep data (default): keep this data for reinstallation.\n\nHarness sessions and plugins in .dsh, custom NEXUS_DATA_DIR locations, links and unrecognized files are kept.", root.display())
        };
        if message(&question, MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2, chinese) != IDYES { return Ok(()); }
        let home = std::env::var_os("DSH_HOME").filter(|value| !value.is_empty()).map(PathBuf::from);
        let result = crate::installer_shutdown::run(args[2..4].to_vec())
            .and_then(|()| ensure_agent_stopped(&root))
            .and_then(|()| cleanup(&root, home.as_deref()).map_err(|error| error.to_string()));
        if let Err(error) = result {
            let detail = if chinese { format!("部分 Nexus 数据未能删除。\n\n原始错误：{error}\n\n剩余数据：{}", root.display()) }
                else { format!("Some Nexus data could not be removed: {error}\n\nRemaining data: {}", root.display()) };
            message(&detail, MB_OK | MB_ICONWARNING, chinese);
            return Err(error.to_string());
        }
        if root.exists() {
            let detail = if chinese { format!("Nexus 设置和可删除缓存已清理。受保护的 Harness 数据、链接或无法识别的文件仍保留在 {}。", root.display()) }
                else { format!("Nexus settings and removable caches were cleared. Protected Harness data, links or unrecognized files remain in {}.", root.display()) };
            message(&detail, MB_OK, chinese);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    { let _ = root; Err("installer-cleanup is supported only on Windows".into()) }
}

fn installer_chinese(explicit: Option<&str>, system_lang: u16) -> bool {
    match explicit {
        Some("2052" | "4100") => true,
        Some("1033" | "1028" | "3076" | "5124") => false,
        _ => matches!(system_lang, 0x0804 | 0x1004),
    }
}

#[cfg(windows)]
fn ensure_agent_stopped(root: &Path) -> Result<(), String> {
    use windows_sys::Win32::{Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT}, System::Threading::{OpenProcess, WaitForSingleObject}};
    let record = match fs::read(root.join("run/agent.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let record: serde_json::Value = serde_json::from_slice(&record).map_err(|error| error.to_string())?;
    let pid = record.get("pid").and_then(|v| v.as_u64()).and_then(|v| u32::try_from(v).ok()).filter(|v| *v != 0).ok_or("Cannot verify the remaining Agent record; data was preserved")?;
    unsafe {
        let handle = OpenProcess(0x00100000, 0, pid); // SYNCHRONIZE access only
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) { return Ok(()); }
            return Err(format!("Cannot verify whether Agent is stopped: {error}"));
        }
        let wait = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        if wait == WAIT_TIMEOUT { return Err("An Agent still owns this data folder. Close Nexus and retry; data was preserved".into()); }
        if wait != 0 { return Err("Cannot verify Agent shutdown; data was preserved".into()); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("nexus-cleanup-{}-{}-{sequence}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())).join("Nexus");
        fs::create_dir_all(&root).unwrap(); root
    }
    #[test]
    fn removes_history_and_caches_but_preserves_harness_and_unknown_data() {
        let root = fixture();
        for directory in ["downloads/failed", "releases/.dsh", "runtimes/custom-home", ".dsh", "my-files"] {
            fs::create_dir_all(root.join(directory)).unwrap();
            fs::write(root.join(directory).join("keep.txt"), "data").unwrap();
        }
        fs::write(root.join("cold-operation.json"), "old failure").unwrap();
        fs::write(root.join("request-receipts.json"), "old operation receipt").unwrap();
        fs::create_dir_all(root.join("canary")).unwrap();
        fs::write(root.join("canary/latest.json"), "old Canary failure").unwrap();
        fs::write(root.join("canary/history-previous.json"), "old Canary history").unwrap();
        cleanup(&root, Some(&root.join("runtimes/custom-home"))).unwrap();
        assert!(!root.join("cold-operation.json").exists());
        assert!(!root.join("request-receipts.json").exists());
        assert!(!root.join("canary").exists());
        assert!(!root.join("downloads").exists());
        for directory in ["releases/.dsh", "runtimes/custom-home", ".dsh", "my-files"] {
            assert!(root.join(directory).join("keep.txt").exists());
        }
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn configuration_redirect_and_clear_protect_previous_home_on_repeated_uninstall() {
        for clear in [false, true] {
            let root = fixture();
            let old = root.join("runtimes/my-harness-data");
            fs::create_dir_all(&old).unwrap();
            fs::write(old.join("settings.yaml"), "user data").unwrap();
            let store = nexus_core::ConfigStore::new(nexus_core::NexusPaths::from_root(root.clone()));
            let mut config = nexus_core::NexusConfigFile::default();
            config.harness_preferences = Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(old.to_string_lossy().into_owned()), ..Default::default()
            });
            store.write(&config).unwrap();
            config.harness_preferences = if clear { None } else { Some(nexus_protocol::HarnessPreferencesPayload {
                home: Some(root.parent().unwrap().join("external-not-created").to_string_lossy().into_owned()), ..Default::default()
            }) };
            store.transaction(|document| { *document = config; Ok(()) }).unwrap();
            assert!(root.join(nexus_core::PREVIOUS_CONFIG_FILE).is_file());
            cleanup(&root, None).unwrap();
            cleanup(&root, None).unwrap();
            assert_eq!(fs::read_to_string(old.join("settings.yaml")).unwrap(), "user data");
            assert!(!root.join(nexus_core::PREVIOUS_CONFIG_FILE).exists());
            fs::remove_dir_all(root.parent().unwrap()).unwrap();
        }
    }
    #[test]
    fn rejects_other_roots_and_parent_traversal() {
        let args = |path: PathBuf| vec!["--data-dir".into(), path.into_os_string(), "--install-dir".into(), std::env::temp_dir().join("install").into_os_string()];
        assert!(root_argument(&[]).is_err());
        assert!(root_argument(&args("Nexus".into())).is_err());
        assert!(root_argument(&args(std::env::temp_dir().join("other"))).is_err());
        assert!(root_argument(&args(std::env::temp_dir().join("../Nexus"))).is_err());
        let root = std::env::temp_dir().join("Nexus");
        assert_eq!(root_argument(&args(root.clone())).unwrap(), root);
    }
    #[test]
    fn installer_language_is_explicit_and_does_not_change_cleanup_paths() {
        for id in [1028,3076,5124] {
            assert!(!installer_chinese(None,id));
            assert!(!installer_chinese(Some(&id.to_string()),2052));
        }
        assert!(installer_chinese(None,4100));
        let root = std::env::temp_dir().join("Nexus");
        let mut args = vec!["--data-dir".into(), root.clone().into_os_string(), "--install-dir".into(), "C:/Nexus".into(), "--locale".into(), "2052".into()];
        assert_eq!(root_argument(&args).unwrap(), root);
        assert!(installer_chinese(Some("2052"), 1033));
        assert!(!installer_chinese(Some("1033"), 2052));
        assert!(installer_chinese(None, 2052));
        assert!(!installer_chinese(None, 1033));
        args[5] = "invalid".into();
        assert!(root_argument(&args).is_err());
        args[5] = "1033".into();
        args[4] = "--unexpected".into();
        assert!(root_argument(&args).is_err());
    }
    #[test]
    fn preserves_persisted_and_environment_homes_before_removing_config() {
        let root = fixture();
        let selected = root.join("runtimes/custom");
        let previous = root.join("downloads/previous");
        for home in [&selected, &previous] {
            fs::create_dir_all(home).unwrap();
            fs::write(home.join("sessions.json"), "keep").unwrap();
        }
        fs::write(root.join("config.json"), serde_json::to_vec(&serde_json::json!({
            "harness_preferences": { "home": selected }
        })).unwrap()).unwrap();
        cleanup(&root, Some(&previous)).unwrap();
        assert!(!root.join("config.json").exists());
        assert!(selected.join("sessions.json").exists());
        assert!(previous.join("sessions.json").exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[test]
    fn invalid_persisted_home_preserves_data() {
        let root = fixture();
        fs::write(root.join("cold-operation.json"), "keep").unwrap();
        for config in [r#"{"harness_preferences":{"home":123}}"#, "invalid json"] {
            fs::write(root.join("config.json"), config).unwrap();
            assert!(cleanup(&root, None).is_err());
            assert!(root.join("cold-operation.json").exists());
            assert!(root.join("config.json").exists());
        }
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[test]
    fn protection_survives_configuration_removal_and_repeated_cleanup() {
        let root = fixture();
        let selected = root.join("runtimes").join("custom");
        let ambient = root.join("downloads").join("custom");
        for home in [&selected, &ambient] {
            fs::create_dir_all(home).unwrap();
            fs::write(home.join("sessions.json"), "keep").unwrap();
        }
        fs::write(root.join("config.json"), serde_json::to_vec(&serde_json::json!({
            "harness_preferences": { "home": selected }
        })).unwrap()).unwrap();
        cleanup(&root, Some(&ambient)).unwrap();
        assert!(!root.join("config.json").exists());
        assert!(root.join(PROTECTED_HOMES).is_file());
        cleanup(&root, None).unwrap();
        assert_eq!(fs::read_to_string(selected.join("sessions.json")).unwrap(), "keep");
        assert_eq!(fs::read_to_string(ambient.join("sessions.json")).unwrap(), "keep");
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[test]
    fn invalid_protection_record_prevents_every_deletion() {
        let root = fixture();
        for bytes in ["invalid json".to_owned(), serde_json::json!({
            "schema_version": 1, "root": root.join("other"), "homes": []
        }).to_string(), serde_json::json!({
            "schema_version": 2, "root": fs::canonicalize(&root).unwrap(), "homes": []
        }).to_string()] {
            fs::write(root.join(PROTECTED_HOMES), bytes).unwrap();
            fs::write(root.join("config.json"), "{}").unwrap();
            fs::write(root.join("cold-operation.json"), "keep").unwrap();
            assert!(cleanup(&root, None).is_err());
            assert!(root.join("config.json").exists());
            assert!(root.join("cold-operation.json").exists());
        }
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn verbatim_and_case_aliases_preserve_the_actual_home() {
        let root = fixture();
        let selected = root.join("runtimes").join("custom");
        fs::create_dir_all(&selected).unwrap();
        fs::write(selected.join("sessions.json"), "keep").unwrap();
        let alias = PathBuf::from(fs::canonicalize(&selected).unwrap().to_string_lossy().to_uppercase());
        fs::write(root.join("config.json"), serde_json::to_vec(&serde_json::json!({
            "harness_preferences": { "home": alias }
        })).unwrap()).unwrap();
        cleanup(&root, None).unwrap();
        cleanup(&root, None).unwrap();
        assert_eq!(fs::read_to_string(selected.join("sessions.json")).unwrap(), "keep");
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[test]
    fn cleanup_removes_release_pointers_with_downloaded_versions() {
        let root = fixture();
        let slot = root.join("releases/old-harness");
        fs::create_dir_all(slot.join("node_modules")).unwrap();
        fs::write(slot.join("manifest.json"), r#"{"id":"old-harness"}"#).unwrap();
        fs::write(root.join("release-pointers.json"), r#"{"schema_version":1,"current_release":"old-harness"}"#).unwrap();
        fs::write(root.join("config.json"), "{}").unwrap();
        fs::create_dir(root.join(".dsh")).unwrap();
        fs::write(root.join(".dsh/sessions.json"), "keep").unwrap();
        cleanup(&root, None).unwrap();
        assert!(!root.join("release-pointers.json").exists());
        assert!(!slot.exists());
        assert!(root.join(".dsh/sessions.json").exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[test]
    fn ambiguous_harness_home_preserves_all_data() {
        let root = fixture();
        fs::write(root.join("config.json"), "settings").unwrap();
        assert!(cleanup(&root, Some(&root.join("runtimes/../releases/home"))).is_err());
        assert!(root.join("config.json").exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn live_agent_record_prevents_cleanup() {
        let root = fixture();
        fs::create_dir(root.join("run")).unwrap();
        fs::write(root.join("run/agent.json"), format!("{{\"pid\":{}}}", std::process::id())).unwrap();
        assert!(ensure_agent_stopped(&root).is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
}
