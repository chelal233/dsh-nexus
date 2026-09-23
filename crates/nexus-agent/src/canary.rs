//! Temporary-home diagnostics. No production compatibility or patch-policy writes.
use super::*;
use nexus_protocol::{CanaryAction, CanaryCommand};
use nexus_core::CancellationToken;
use std::{path::PathBuf, time::Duration, process::Stdio};
use serde_json::{json, Value};

pub(crate) type Owner = Arc<Mutex<Option<(String, CancellationToken)>>>;
fn diagnostic_capabilities(slot: &std::path::Path, home: &std::path::Path, profile: &str,
    preferences: &nexus_protocol::HarnessPreferencesPayload) -> io::Result<nexus_core::HarnessProfileCapabilities> {
    // Unlike ordinary launch with empty overrides, a diagnostic needs positive
    // Web evidence; unknown versions must not inherit a false capability value.
    let capabilities = crate::preference_capabilities::inspect(slot, home, profile)?.capabilities;
    capabilities.validate_preferences(preferences)?;
    if !capabilities.web { return Err(io::Error::other("Canary HTML probe requires a Web-capable profile")); }
    Ok(capabilities)
}
fn directory(paths: &nexus_core::NexusPaths) -> PathBuf { paths.root.join("canary") }
fn prepare_root(paths: &nexus_core::NexusPaths) -> io::Result<PathBuf> {
    let root = directory(paths);
    match fs::symlink_metadata(&root) {
        Ok(meta) if meta.is_dir() && !nexus_core::path_is_reparse(&meta) => {},
        Ok(_) => return Err(io::Error::other("Canary directory must not be a link")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&root)?,
        Err(error) => return Err(error),
    }
    Ok(root)
}
fn load(paths: &nexus_core::NexusPaths) -> io::Result<Value> {
    let Some(bytes) = nexus_core::read_regular_file_bounded(&directory(paths).join("latest.json"), 512 * 1024)? else { return Ok(json!({"phase":"idle"})); };
    let value: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if value["format_version"] != 1 || value["operation_id"].as_str().is_none_or(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit())) { return Err(io::Error::other("Unsupported Canary record; original retained")); }
    Ok(value)
}
fn busy(value: &Value) -> bool { matches!(value["phase"].as_str(), Some("running" | "cancelling")) || value["cleanup_pending"] == true }
pub(crate) fn ensure_idle(paths: &nexus_core::NexusPaths) -> io::Result<()> {
    if busy(&load(paths)?) { return Err(io::Error::other("Canary verification or cleanup is pending; cancel it before other changes")); }
    Ok(())
}
fn save(paths: &nexus_core::NexusPaths, value: &Value) -> io::Result<()> {
    let root = prepare_root(paths)?;
    if serde_json::to_vec(value).map_err(io::Error::other)?.len()>480*1024 {return Err(io::Error::other("Canary status exceeds size limit"));}
    nexus_core::write_private_json_atomic(&root, &root.join("latest.json"), value)
}
fn work(paths: &nexus_core::NexusPaths, id: &str) -> PathBuf { directory(paths).join(format!("work-{id}")) }
fn job(id: &str) -> String { format!("Global\\NexusCanary-{id}") }
fn recovered_owner(paths: &nexus_core::NexusPaths, id: &str) -> io::Result<bool> {
    crate::process_recovery::reconcile(&directory(paths).join("owned-processes"))?;
    #[cfg(windows)] { crate::dsh::named_operation_job_is_empty(&job(id)) }
    #[cfg(unix)] {
        let _ = id;
        if load(paths)?["process_owner_version"] != 1 {
            crate::process_recovery::require_legacy_reboot(&directory(paths).join("latest.json"))?;
        }
        Ok(true)
    }
}
pub(crate) fn recover_unattached(paths: &nexus_core::NexusPaths) -> io::Result<()> {
    let mut value = load(paths)?;
    if !busy(&value) { return Ok(()); }
    let id = value["operation_id"].as_str().ok_or_else(|| io::Error::other("Canary operation identity missing"))?.to_owned();
    if !recovered_owner(paths, &id)? { return Err(io::Error::new(io::ErrorKind::ResourceBusy, "Canary descendants are still stopping")); }
    cleanup(paths, &id)?;
    value["phase"] = json!("interrupted");
    value["cleanup_pending"] = json!(false);
    save(paths, &value)?;
    archive(paths, &value)
}
fn cleanup(paths: &nexus_core::NexusPaths, id: &str) -> io::Result<()> {
    let work = work(paths, id); if work.exists() { crate::cold::remove_owned_directory(&directory(paths), &work)?; } Ok(())
}
pub(crate) async fn status(State(state): State<AppState>) -> Response {
    match load(&state.paths) { Ok(mut value) => { match history(&state.paths) {
            Ok(records)=>value["history"]=json!(records.into_iter().take(10).collect::<Vec<_>>()),
            Err(error)=>{let (safe,_)=redact_diagnostics_payload(error.to_string().as_bytes());value["history_error"]=json!(String::from_utf8_lossy(&safe));}
        } Json(value).into_response() }, Err(error) => data_error_response(error, "canary_record_invalid") }
}
pub(crate) async fn control(State(state): State<AppState>, Json(command): Json<CanaryCommand>) -> Response {
    if command.action == CanaryAction::History {
        if command.mode.is_some() || command.profile.is_some() { return api_error_response(StatusCode::BAD_REQUEST,"canary_invalid","History accepts only an operation ID"); }
        return match command.operation_id.as_deref().ok_or_else(|| io::Error::other("History operation ID is required")).and_then(|id| read_history(&state.paths,id)) {
            Ok(value) => Json(value).into_response(), Err(error) => data_error_response(error,"canary_history_unavailable")
        };
    }
    let mut owner = state.canary.lock().await;
    if command.action == CanaryAction::Cancel {
        let Some(id) = command.operation_id.as_deref() else { return api_error_response(StatusCode::BAD_REQUEST, "canary_invalid", "Operation ID is required"); };
        if let Some((current, token)) = owner.as_ref() {
            if current != id { return api_error_response(StatusCode::CONFLICT, "canary_stale", "Canary operation changed"); }
            token.cancel();
            if let Ok(mut value) = load(&state.paths) { value["phase"] = json!("cancelling"); if let Err(error) = save(&state.paths, &value) { return data_error_response(error, "canary_record_invalid"); } }
            return (StatusCode::ACCEPTED, Json(json!({"operation_id":id,"phase":"cancelling"}))).into_response();
        }
        let paths = state.paths.clone(); let id = id.to_owned();
        let result = tokio::task::spawn_blocking(move || -> io::Result<Value> {
            let mut value = load(&paths)?;
            if value["operation_id"] != id { return Err(io::Error::other("Canary operation changed")); }
            if busy(&value) {
                if !recovered_owner(&paths, &id)? { return Err(io::Error::other("Canary process shutdown cannot yet be verified; files retained")); }
                cleanup(&paths, &id)?; value["phase"] = json!("interrupted"); value["cleanup_pending"] = json!(false); save(&paths, &value)?; if let Err(error)=archive(&paths,&value) {tracing::warn!(%error,"Interrupted Canary history was not saved");}
            }
            Ok(value)
        }).await.map_err(io::Error::other).and_then(|result| result);
        return match result { Ok(value) => Json(value).into_response(), Err(error) => data_error_response(error, "canary_cleanup_pending") };
    }
    if owner.is_some() { return api_error_response(StatusCode::CONFLICT, "canary_busy", "Canary is already running"); }
    let Some(mode) = command.mode else { return api_error_response(StatusCode::BAD_REQUEST, "canary_invalid", "Select diagnostic_only or bisect"); };
    let Some(lifecycle) = state.supervisor.try_acquire_lifecycle() else { return api_error_response(StatusCode::CONFLICT, "canary_busy", "Another operation owns Harness"); };
    if let Err(response) = ensure_checkpoint_mutation_ready(&state).await { return response; }
    let update = match state.updater.try_acquire_gate() { Ok(value) => value, Err(error) => return update_error_response(error) };
    let cold = match state.cold.try_acquire_maintenance() { Ok(value) => value, Err(error) => return data_error_response(error, "canary_busy") };
    let configuration = match state.snapshots.try_acquire_configuration() { Ok(value) => value, Err(error) => return data_error_response(error, "canary_busy") };
    if let Err(response) = ensure_harness_selection_quiescent(&state, &lifecycle, "canary_busy", "Stop Harness before Canary verification").await { return response; }
    let start = (|| -> io::Result<(String, String, Value)> {
        let home = state.snapshots.configured_dsh_home()?;
        let source = compatibility::source_profile(&home, &state.profiles.load()?.active_profile)?;
        if command.profile.as_deref().is_some_and(|name| name != source) { return Err(io::Error::other("Canary requires the currently selected source profile")); }
        let id = nexus_core::agent_auth::random_hex()?;
        let value = json!({"format_version":1,"process_owner_version":1,"operation_id":id,"phase":"running","mode":mode,"source_profile":source,
            "config_revision":state.config.snapshot()?.revision,"started_at_unix":unix_time_seconds(),"cleanup_pending":true});
        save(&state.paths, &value)?; Ok((id, source, value))
    })();
    let (id, source, value) = match start { Ok(value) => value, Err(error) => return data_error_response(error, "canary_invalid") };
    let token = CancellationToken::with_job_name(job(&id)).with_process_registry(directory(&state.paths).join("owned-processes"));
    *owner = Some((id.clone(), token.clone())); drop(owner);
    let response = value.clone();
    tokio::spawn(async move {
        let _guards = (lifecycle, update, cold, configuration);
        let mut shutdown = state.shutdown.subscribe(); let shutdown_token = token.clone();
        let watcher = tokio::spawn(async move { if *shutdown.borrow() { shutdown_token.cancel(); return; } while shutdown.changed().await.is_ok() { if *shutdown.borrow() { shutdown_token.cancel(); return; } } });
        let result = run(&state, &source, &id, &mode, &token).await;
        watcher.abort();
        let mut value = load(&state.paths).ok().filter(|current| current["operation_id"] == id).unwrap_or(value);
        let quiescent = result.as_ref().err().is_none_or(crate::cold::command_owner_quiescent);
        value["phase"] = json!(if token.is_cancelled() { "cancelled" } else if result.is_ok() { "completed" } else { "failed" });
        match result { Ok(report) => value["report"] = report, Err(error) => { let (safe, _) = redact_diagnostics_payload(error.to_string().as_bytes()); value["error"] = json!(String::from_utf8_lossy(&safe)); } }
        value["cleanup_pending"] = json!(true);
        if quiescent { let paths = state.paths.clone(); let cleanup_id = id.clone(); match tokio::task::spawn_blocking(move || cleanup(&paths, &cleanup_id)).await.map_err(io::Error::other).and_then(|result| result) { Ok(()) => value["cleanup_pending"] = json!(false), Err(error) => value["cleanup_error"] = json!(error.to_string()) } }
        value["finished_at_unix"] = json!(unix_time_seconds());
        let mut owner = state.canary.lock().await;
        if token.is_cancelled() { value["phase"]=json!("cancelled"); }
        if let Err(error) = save(&state.paths, &value) { tracing::error!(%error, "Canary result could not be saved; pending record remains authoritative"); }
        if let Err(error) = archive(&state.paths, &value) { let (safe,_)=redact_diagnostics_payload(error.to_string().as_bytes()); value["history_error"]=json!(String::from_utf8_lossy(&safe)); let _=save(&state.paths,&value); tracing::warn!(%error,"Canary history was not saved"); }
        *owner = None;
    });
    (StatusCode::ACCEPTED, Json(response)).into_response()
}
async fn run(state: &AppState, profile: &str, id: &str, mode: &nexus_protocol::CanaryMode, cancellation: &CancellationToken) -> io::Result<Value> {
    if cancellation.is_cancelled() { return Err(io::Error::other("Canary cancelled")); }
    let revision = state.config.snapshot()?.revision;
    let home = state.snapshots.configured_dsh_home()?;
    let source_paths = state.paths.clone(); let releases = state.releases.clone();
    let context = tokio::task::spawn_blocking(move || crate::source_context::resolve(&source_paths,&releases)).await.map_err(io::Error::other)??;
    let slot = context.root.ok_or_else(|| io::Error::other("Select an installed Harness source"))?;
    let release = match context.release_id {Some(id)=>id,None=>format!("external-{}",state.config.load()?.external_harness.ok_or_else(||io::Error::other("External source changed"))?.fingerprint)};
    let mut spec = nexus_core::load_harness_launch_spec(&state.paths)?.ok_or_else(|| io::Error::other("Configure a managed Node Harness"))?;
    if spec.mode != nexus_protocol::HarnessLaunchMode::Node { return Err(io::Error::other("Canary only supports managed Node Harness")); }
    if !context.external { crate::supervisor::normalize_managed_launch(&mut spec, &state.releases)?; }
    let runtime = crate::runtime::runtime_for_launch(&mut spec, state.config.load()?.runtime.unwrap_or_default(), nexus_core::bundled_runtime_dir().as_deref());
    let node = spec.render_path_for_context(&spec.program, profile, Some(&release), Some(&slot))?;
    let args = spec.render_args_for_context(profile, Some(&release), Some(&slot))?;
    if args.len() != 3 || args[1] != "--profile" || args[2] != profile || args.first().and_then(|s| fs::canonicalize(s).ok()) != Some(fs::canonicalize(slot.join("apps/cli/lib/bin.js"))?) { return Err(io::Error::other("Canary requires the managed entry and profile arguments; custom launch commands are unsupported")); }
    let preferences = nexus_core::load_harness_preferences(&state.paths)?;
    // Validate file contents without recording or acknowledging production patch failures.
    crate::runtime_patches::validate(&preferences)?;
    let capabilities = diagnostic_capabilities(&slot, &home, profile, &preferences)?;
    let work = work(&state.paths, id); nexus_core::create_new_private_directory(&work)?;
    let script = work.join("checker.mjs");
    fs::write(&script, include_bytes!("compatibility.mjs"))?;
    fs::write(script.parent().unwrap().join("startup-diagnosis.mjs"), include_bytes!("startup-diagnosis.mjs"))?;
    let vendor = script.parent().unwrap().join("vendor");
    fs::create_dir_all(&vendor)?;
    fs::write(vendor.join("semver.cjs"), include_bytes!("vendor/semver.cjs"))?;
    fs::write(vendor.join("semver.LICENSE"), include_bytes!("vendor/semver.LICENSE"))?;
    let mut options = json!({"home":home,"selected":profile,"release_id":release,"node":node,"slot":slot,"mode":"diagnostic_only","owned_round":true,"patches":preferences.patches.as_deref().unwrap_or(&[])});
    let mut environment = nexus_core::checked_runtime_child_env(&runtime, std::env::var_os("PATH").as_deref())?;
    options["builtin_patches"] = json!([crate::desktop_plugins::stage(&state.paths, &home, profile)?]);
    environment.push(("NEXUS_DESKTOP_CONTEXT".into(), crate::desktop_plugins::context(&state.paths, &home, profile, &node, &slot, runtime.pnpm.as_ref().map(|pin| pin.path.as_path()))?.into()));
    environment.push(("NEXUS_DESKTOP_PROBE".into(), "1".into()));
    environment.extend(nexus_core::harness_preferences_environment(&preferences, &capabilities));
    let started = std::time::Instant::now();
    let mut report = operation_round(state, id, &node, &script, &work, &options, &environment, cancellation, 0).await?;
    let identity = json!([report["fingerprint"], report["patches"]]);
    let mut rounds = vec![report["rounds"][0].clone()];
    let mut suspects = report["all_candidates"].as_array().cloned().unwrap_or_default();
    if matches!(mode, nexus_protocol::CanaryMode::Bisect) && report["outcome"] == "failed" {
        let mut trial = Vec::new();
        let mut baseline = true;
        let mut index = 0usize;
        let mut confirmed = false;
        loop {
            if rounds.len() >= 20 || started.elapsed() >= Duration::from_secs(540) { report["outcome"] = json!("inconclusive"); report["reason"] = json!("Canary round or time budget exceeded"); break; }
            options["subset"] = json!(trial);
            let next = operation_round(state, id, &node, &script, &work, &options, &environment, cancellation, rounds.len()).await?;
            if json!([next["fingerprint"], next["patches"]]) != identity { return Err(io::Error::other("Canary source or patches changed between rounds")); }
            rounds.push(next["rounds"][0].clone());
            let outcome = next["outcome"].as_str().unwrap_or("inconclusive");
            if baseline {
                if outcome != "passed" { report["outcome"] = json!("inconclusive"); report["reason"] = json!("Baseline without third-party plugins did not pass"); break; }
                baseline = false;
            } else if confirmed {
                if outcome == "failed" { report["suspect_combination"] = json!(suspects); report["reason"] = json!("Confirmed failing combination; no plugins were changed"); }
                else { report["outcome"] = json!("inconclusive"); report["reason"] = json!("Failure could not be reproduced"); }
                break;
            } else if outcome == "failed" { suspects = trial; index = 0; }
            else if outcome == "passed" { index += 1; }
            else { report["outcome"] = json!("inconclusive"); report["reason"] = next["reason"].clone(); break; }
            if index >= suspects.len() { trial = suspects.clone(); confirmed = true; }
            else { trial = suspects.clone(); trial.remove(index); }
        }
    }
    report["rounds"] = json!(rounds);
    if state.config.snapshot()?.revision != revision { return Err(io::Error::other("Configuration changed during Canary; no attribution is valid")); }
    report["config_revision"] = json!(revision);
    if report["source_profile"] != profile || report["release_id"] != release || report["checks"]["feature"] != "unsupported" { return Err(io::Error::other("Canary report identity mismatch")); }
    Ok(report)
}

fn history_path(paths: &nexus_core::NexusPaths, id: &str) -> io::Result<PathBuf> {
    if id.len()!=64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(io::Error::other("Invalid Canary history ID")); }
    Ok(directory(paths).join(format!("history-{id}.json")))
}
fn read_history(paths: &nexus_core::NexusPaths, id: &str) -> io::Result<Value> {
    let bytes = nexus_core::read_regular_file_bounded(&history_path(paths,id)?,256*1024)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound,"Canary history no longer available"))?;
    let value:Value=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if value["format_version"]!=1 || value["operation_id"]!=id || value["cleanup_pending"]!=false || !matches!(value["phase"].as_str(),Some("completed"|"cancelled"|"failed"|"interrupted")) {return Err(io::Error::other("Unsupported or incomplete Canary history; original retained"));}
    Ok(value)
}
fn history(paths: &nexus_core::NexusPaths) -> io::Result<Vec<Value>> {
    history_records(paths)
}
fn visit_history(paths: &nexus_core::NexusPaths, mut visit: impl FnMut(Value) -> io::Result<()>) -> io::Result<()> {
    let entries=match fs::read_dir(directory(paths)){Ok(v)=>v,Err(e) if e.kind()==io::ErrorKind::NotFound=>return Ok(()),Err(e)=>return Err(e)};
    // Stream entries instead of making a large backlog an unrecoverable state.
    // Each document is still bounded; callers retain only the newest summaries.
    for entry in entries {
        let entry=entry?;let name=entry.file_name();let Some(name)=name.to_str() else {continue};
        let Some(id)=name.strip_prefix("history-").and_then(|s|s.strip_suffix(".json")) else {continue};
        let value = match read_history(paths,id) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        visit(json!({"operation_id":id,"phase":value["phase"],"mode":value["mode"],"source_profile":value["source_profile"],"finished_at_unix":value["finished_at_unix"],"outcome":value["report"]["outcome"]}))?;
    }
    Ok(())
}
fn history_order(value: &Value) -> (u64, &str) {
    (value["finished_at_unix"].as_u64().unwrap_or(0), value["operation_id"].as_str().unwrap_or(""))
}
fn history_records(paths: &nexus_core::NexusPaths) -> io::Result<Vec<Value>> {
    Ok(history_inventory(paths)?.0)
}
fn history_inventory(paths: &nexus_core::NexusPaths) -> io::Result<(Vec<Value>, usize)> {
    let mut records=Vec::new();
    let mut count=0usize;
    visit_history(paths, |value| {
        count=count.saturating_add(1);
        records.push(value);
        records.sort_by(|left,right|history_order(right).cmp(&history_order(left)));
        records.truncate(10);
        Ok(())
    })?;
    Ok((records,count))
}
fn archive(paths: &nexus_core::NexusPaths, value: &Value) -> io::Result<()> {
    archive_with_cleanup(paths,value,&mut |file|fs::remove_file(file))
}
const MAX_HISTORY_FILES:usize=64;
fn prune_history(paths:&nexus_core::NexusPaths,id:&str,cutoff:&Value,remove:&mut impl FnMut(&std::path::Path)->io::Result<()>) -> io::Result<()> {
    visit_history(paths, |old| {
        if old["operation_id"]!=id && history_order(&old)<history_order(cutoff) {
            match remove(&history_path(paths,old["operation_id"].as_str().unwrap())?) {
                Err(error) if error.kind()!=io::ErrorKind::NotFound=>return Err(error), _=>{}
            }
        }
        Ok(())
    })
}
fn archive_with_cleanup(paths:&nexus_core::NexusPaths,value:&Value,remove:&mut impl FnMut(&std::path::Path)->io::Result<()>) -> io::Result<()> {
    let id=value["operation_id"].as_str().ok_or_else(||io::Error::other("Missing history ID"))?;
    if busy(value) {return Ok(());}
    let bytes=serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len()>256*1024 {return Err(io::Error::other("Canary history exceeds size limit"));}
    let (records,count)=history_inventory(paths)?;
    if count>=MAX_HISTORY_FILES {
        // Latest-result persistence is handled by the caller. At capacity,
        // preserve the newest ten history records and require cleanup before
        // creating another file. A persistent ACL error must not grow storage.
        if let Some(cutoff)=records.last() {
            prune_history(paths,id,cutoff,remove).map_err(|_|io::Error::other("Canary history is full and old reports cannot be removed; restore delete access before archiving more reports"))?;
        }
        if history_inventory(paths)?.1>=MAX_HISTORY_FILES {
            return Err(io::Error::other("Canary history is full; remove old reports before archiving more results"));
        }
    }
    let existing:Vec<_>=records.into_iter().filter(|old|old["operation_id"]!=id).take(9).collect();
    nexus_core::write_private_bytes_atomic(&directory(paths),&history_path(paths,id)?,&bytes)?;
    if let Some(cutoff)=existing.last() {
        prune_history(paths,id,cutoff,remove)?;
    }
    Ok(())
}
async fn progress(state:&AppState,id:&str,stage:&str,index:usize,subset:&Value,plan:Option<&Value>,completed:Option<Value>)->io::Result<()> {
    let _owner=state.canary.lock().await;
    let mut value=load(&state.paths)?;
    if value["operation_id"]!=id || value["phase"]!="running" {return Ok(());}
    let prior=value["progress"]["round_index"].as_u64();
    if prior!=Some(index as u64) {value["progress"]["round_started_at_unix"]=json!(unix_time_seconds());}
    value["progress"]["round_index"]=json!(index);value["progress"]["round_limit"]=json!(20);
    value["progress"]["stage"]=json!(stage);value["progress"]["enabled_bundles"]=subset.clone();
    if let Some(plan)=plan {value["progress"]["space"]=plan.clone();}
    if let Some(completed)=completed {
        let mut rounds=value["progress"]["completed_rounds"].as_array().cloned().unwrap_or_default();
        if rounds.len()<20 {rounds.push(completed);} value["progress"]["completed_rounds"]=json!(rounds);
    }
    save(&state.paths,&value)
}
async fn operation_round(state:&AppState,id:&str,node:&std::path::Path,script:&std::path::Path,work:&std::path::Path,options:&Value,
    environment:&[(std::ffi::OsString,std::ffi::OsString)],cancellation:&CancellationToken,index:usize)->io::Result<Value> {
    let subset=&options["subset"];
    progress(state,id,"planning",index,subset,None,None).await?;
    let plan_dir=work.join(format!("plan-{index}"));nexus_core::create_new_private_directory(&plan_dir)?;
    let input=plan_dir.join("request.json");let output=plan_dir.join("plan.json");let mut request=options.clone();
    request["canary_plan"]=json!(true);request["output"]=json!(output);
    nexus_core::write_private_json_atomic(&plan_dir,&input,&request)?;
    let mut command=std::process::Command::new(node);
    command.arg(script).arg(&input).current_dir(&plan_dir).stdin(Stdio::null()).stdout(Stdio::null()).envs(environment.iter().cloned());
    crate::cold::run_owned_command(command,"Canary copy plan",Duration::from_secs(15),&plan_dir,cancellation).await?;
    let bytes=nexus_core::read_regular_file_bounded(&output,4096)?.ok_or_else(||io::Error::other("Canary copy plan missing"))?;
    let plan:Value=serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let required=plan["required_bytes"].as_u64().ok_or_else(||io::Error::other("Invalid Canary space budget"))?;
    nexus_private_file::ensure_space_budget(&[(work,required)])?;
    crate::cold::remove_owned_directory(work,&plan_dir)?;
    if cancellation.is_cancelled(){return Err(io::Error::other("Canary cancelled"));}
    progress(state,id,"copying_and_probing",index,subset,Some(&plan),None).await?;
    let started=std::time::Instant::now();
    let mut value=round(node,script,work,options,environment,cancellation,index).await?;
    if value["fingerprint"]!=plan["fingerprint"] {return Err(io::Error::other("Canary source changed after copy planning"));}
    value["rounds"][0]["duration_ms"]=json!(started.elapsed().as_millis() as u64);
    progress(state,id,"round_finished",index,subset,Some(&plan),Some(value["rounds"][0].clone())).await?;
    Ok(value)
}

async fn round(node: &std::path::Path, script: &std::path::Path, work: &std::path::Path, options: &Value,
    environment: &[(std::ffi::OsString, std::ffi::OsString)], cancellation: &CancellationToken, index: usize) -> io::Result<Value> {
    let directory = work.join(format!("round-{index}")); nexus_core::create_new_private_directory(&directory)?;
    let input = directory.join("request.json"); let output = directory.join("report.json");
    let mut options = options.clone(); options["work"] = json!(directory); options["output"] = json!(output);
    nexus_core::write_private_json_atomic(&directory, &input, &options)?;
    let mut command = std::process::Command::new(node);
    command.arg(script).arg(&input).current_dir(&directory).stdin(Stdio::null()).stdout(Stdio::null()).envs(environment.iter().cloned());
    // Success/error returns only after the owned job is quiescent. Unknown ownership preserves the directory.
    crate::cold::run_owned_command(command, "Canary round", Duration::from_secs(60), &directory, cancellation).await?;
    let bytes = nexus_core::read_regular_file_bounded(&output, 256 * 1024)?.ok_or_else(|| io::Error::other("Canary round did not produce a report"))?;
    let (safe, _) = redact_diagnostics_payload(&bytes);
    let value = serde_json::from_slice(&safe).map_err(io::Error::other)?;
    let parent = work.to_owned();
    tokio::task::spawn_blocking(move || crate::cold::remove_owned_directory(&parent, &directory)).await.map_err(io::Error::other)??;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canary_pending_guard_covers_cleanup_and_cancelling() {
        assert!(busy(&json!({"phase":"cancelling"})));
        assert!(busy(&json!({"phase":"failed","cleanup_pending":true})));
        assert!(!busy(&json!({"phase":"completed","cleanup_pending":false})));
    }
    #[test]
    fn default_web_diagnostic_requires_positive_capability_and_profile_evidence() {
        let root = std::env::temp_dir().join(format!("canary-capabilities-{}", nexus_core::unix_time_nanos_for_update()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"0.1.2-rc.1"}"#).unwrap();
        crate::preference_capabilities::write_web_contract_fixture(&root);
        let p = nexus_protocol::HarnessPreferencesPayload::default();
        assert!(diagnostic_capabilities(&root, &root.join("home"), "web", &p).unwrap().web);
        assert!(diagnostic_capabilities(&root, &root.join("home"), "headless", &p).is_err());
        fs::write(root.join("package.json"), r#"{"name":"@deepseek-ai/dsh-root","version":"unknown"}"#).unwrap();
        assert!(diagnostic_capabilities(&root, &root.join("home"), "web", &p).unwrap().web);
        fs::remove_file(root.join("packages/bundle/web-app/src/startup.ts")).unwrap();
        assert!(diagnostic_capabilities(&root, &root.join("home"), "web", &p).is_err());
        assert!(!root.join("home").exists());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, windows))]
mod ownership_http_tests {
    use super::*;
    #[tokio::test]
    async fn canary_round_waits_for_descendants_before_deleting_scratch() {
        let root = std::env::temp_dir().join(format!("canary-round-tree-{}",nexus_core::agent_auth::random_hex().unwrap()));
        nexus_core::create_new_private_directory(&root).unwrap();
        let script = root.join("checker.cjs");
        fs::write(&script,"const fs=require('fs'),cp=require('child_process');if(process.argv[2]==='child'){setInterval(()=>{},1000)}else{const o=JSON.parse(fs.readFileSync(process.argv[2]));cp.spawn(process.execPath,[__filename,'child'],{stdio:'ignore'}).unref();fs.writeFileSync(o.output,JSON.stringify({outcome:'passed',rounds:[{outcome:'passed'}]}))}").unwrap();
        let name = job(&nexus_core::agent_auth::random_hex().unwrap());
        let token = CancellationToken::with_job_name(name.clone());
        let result = round(std::path::Path::new("node"),&script,&root,&json!({}),&[],&token,0).await.unwrap();
        assert_eq!(result["outcome"],"passed");
        assert!(crate::dsh::named_operation_job_is_empty(&name).unwrap());
        assert!(!root.join("round-0").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn canary_http_cancel_and_restart_reconciliation_preserve_live_owned_tree() {
        let state = crate::switch_ownership_tests::switch_test_state("canary-http-owner");
        let root = state.paths.root.clone();
        let id = nexus_core::agent_auth::random_hex().unwrap();
        save(&state.paths, &json!({"format_version":1,"process_owner_version":1,"operation_id":id,"phase":"running","cleanup_pending":true})).unwrap();
        let work = work(&state.paths,&id); nexus_core::create_new_private_directory(&work).unwrap();
        let marker = work.join("child-ready");
        let script = work.join("worker.cjs");
        fs::write(&script, "const fs=require('fs'),cp=require('child_process');if(process.argv[2]==='child'){fs.writeFileSync(process.argv[3],'ready');setInterval(()=>{},1000)}else{cp.spawn(process.execPath,[__filename,'child',process.argv[2]],{stdio:'ignore'});setInterval(()=>{},1000)}").unwrap();
        let token = CancellationToken::with_job_name(job(&id));
        let owned = token.clone(); let command_work = work.clone();
        let worker = tokio::spawn(async move {
            let mut command = std::process::Command::new("node"); command.arg(script).arg(&marker).stdin(Stdio::null());
            crate::cold::run_owned_command(command,"canary ownership fixture",Duration::from_secs(20),&command_work,&owned).await
        });
        let ready = work.join("child-ready");
        let became_ready = tokio::time::timeout(Duration::from_secs(10),async {while !ready.exists() && !worker.is_finished() {tokio::time::sleep(Duration::from_millis(20)).await;}}).await.is_ok() && ready.exists();
        if !became_ready { token.cancel(); panic!("Canary ownership fixture did not start: {:?}", worker.await); }
        // A recovered record has no in-memory owner: cleanup must refuse a live named job.
        let app = axum::Router::new().route("/v1/canary",axum::routing::get(status).post(control)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/canary",listener.local_addr().unwrap());
        let server = tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        let client = reqwest::Client::new();
        let body = json!({"action":"cancel","operation_id":id});
        assert!(!client.post(&url).json(&body).send().await.unwrap().status().is_success());
        assert!(ready.exists()); assert!(ensure_idle(&state.paths).is_err());
        *state.canary.lock().await = Some((id.clone(),token.clone()));
        let wrong = client.post(&url).json(&json!({"action":"cancel","operation_id":"stale"})).send().await.unwrap();
        assert_eq!(wrong.status(),reqwest::StatusCode::CONFLICT);
        assert!(!token.is_cancelled());
        let response = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(response.status(),reqwest::StatusCode::ACCEPTED);
        let current:Value=client.get(&url).send().await.unwrap().json().await.unwrap();
        assert_eq!(current["phase"],"cancelling");
        let error = worker.await.unwrap().unwrap_err(); assert!(crate::cold::command_owner_quiescent(&error));
        assert!(crate::dsh::named_operation_job_is_empty(&job(&id)).unwrap());
        *state.canary.lock().await=None;
        assert!(client.post(&url).json(&body).send().await.unwrap().status().is_success());
        assert!(!work.exists()); assert!(ensure_idle(&state.paths).is_ok());
        server.abort(); fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod progress_history_tests {
    use super::*;
    #[test]
    fn persistent_cleanup_failures_bound_history_and_preserve_the_latest_result() {
        let state=crate::switch_ownership_tests::switch_test_state("canary-cleanup-capacity");
        prepare_root(&state.paths).unwrap();
        let mut last=Value::Null;
        for time in 0..MAX_HISTORY_FILES+8 {
            last=json!({"format_version":1,"operation_id":nexus_core::agent_auth::random_hex().unwrap(),"phase":"completed","cleanup_pending":false,"finished_at_unix":time});
            save(&state.paths,&last).unwrap();
            let result=archive_with_cleanup(&state.paths,&last,&mut |_|Err(io::Error::new(io::ErrorKind::PermissionDenied,"fixture ACL denies deletion")));
            if time>=10 {assert!(result.is_err());}
            assert!(history_inventory(&state.paths).unwrap().1<=MAX_HISTORY_FILES);
            assert_eq!(load(&state.paths).unwrap(),last);
        }
        assert_eq!(history_inventory(&state.paths).unwrap().1,MAX_HISTORY_FILES);
        assert_eq!(history(&state.paths).unwrap().len(),10);
        archive(&state.paths,&last).unwrap();
        assert_eq!(history_inventory(&state.paths).unwrap().1,10);
        assert_eq!(history(&state.paths).unwrap()[0]["operation_id"],last["operation_id"]);
        fs::remove_dir_all(&state.paths.root).unwrap();
    }
    #[test]
    fn overfull_history_is_read_only_until_archive() {
        let state=crate::switch_ownership_tests::switch_test_state("canary-overfull");
        let count=128;
        for time in 0..count {
            let id=nexus_core::agent_auth::random_hex().unwrap();
            let file=history_path(&state.paths,&id).unwrap();
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file,serde_json::to_vec(&json!({"format_version":1,"operation_id":id,"phase":"completed","cleanup_pending":false,"finished_at_unix":time})).unwrap()).unwrap();
        }
        let history_count=||fs::read_dir(directory(&state.paths)).unwrap().filter(|entry|entry.as_ref().unwrap().file_name().to_string_lossy().starts_with("history-")).count();
        let records=history(&state.paths).unwrap();assert_eq!(records.len(),10);assert_eq!(records[0]["finished_at_unix"],count-1);
        assert_eq!(history_count(),count as usize,"status must not delete history");
        std::thread::scope(|scope| {
            for _ in 0..4 { scope.spawn(|| { for _ in 0..3 { assert_eq!(history(&state.paths).unwrap().len(),10); } }); }
        });
        let id=nexus_core::agent_auth::random_hex().unwrap();
        for _ in 0..2 {
            assert!(archive(&state.paths,&json!({"operation_id":id,"phase":"failed","cleanup_pending":false,"oversized":"x".repeat(256*1024)})).is_err());
            assert_eq!(history_count(),count as usize);
            assert_eq!(history(&state.paths).unwrap().len(),10);
        }
        archive(&state.paths,&json!({"format_version":1,"operation_id":id,"phase":"completed","cleanup_pending":false,"finished_at_unix":count})).unwrap();
        let records=history(&state.paths).unwrap();assert_eq!(records.len(),10);assert_eq!(records[0]["operation_id"],id);
        assert_eq!(history_count(),10);
        fs::remove_dir_all(&state.paths.root).unwrap();
    }
    #[tokio::test]
    async fn canary_progress_preserves_cancel_and_history_is_bounded_and_independently_readable() {
        let state=crate::switch_ownership_tests::switch_test_state("canary-history");
        let id=nexus_core::agent_auth::random_hex().unwrap();
        save(&state.paths,&json!({"format_version":1,"process_owner_version":1,"operation_id":id,"phase":"running","cleanup_pending":true})).unwrap();
        progress(&state,&id,"planning",0,&Value::Null,None,None).await.unwrap();
        let mut current=load(&state.paths).unwrap();current["phase"]=json!("cancelling");save(&state.paths,&current).unwrap();
        progress(&state,&id,"round_finished",0,&json!([]),None,Some(json!({"outcome":"passed"}))).await.unwrap();
        assert_eq!(load(&state.paths).unwrap(),current);
        let mut newest=String::new();
        for time in 0..12 {newest=nexus_core::agent_auth::random_hex().unwrap();archive(&state.paths,&json!({"format_version":1,"operation_id":newest,"phase":"completed","cleanup_pending":false,"finished_at_unix":time,"report":{"outcome":"passed"}})).unwrap();}
        assert_eq!(history(&state.paths).unwrap().len(),10);
        assert_eq!(history(&state.paths).unwrap()[0]["finished_at_unix"],11);
        let _owner=state.canary.lock().await;
        let response=tokio::time::timeout(Duration::from_millis(500),control(State(state.clone()),Json(CanaryCommand{action:CanaryAction::History,mode:None,profile:None,operation_id:Some(newest)}))).await.unwrap();
        assert_eq!(response.status(),StatusCode::OK);
        assert!(read_history(&state.paths,"../latest").is_err());
        let future=nexus_core::agent_auth::random_hex().unwrap();let file=history_path(&state.paths,&future).unwrap();
        let bytes=b"{\"format_version\":99}";fs::write(&file,bytes).unwrap();
        assert!(history(&state.paths).is_err());
        assert!(archive(&state.paths,&json!({"operation_id":id,"phase":"failed","cleanup_pending":false})).is_err());
        assert_eq!(fs::read(file).unwrap(),bytes);
        fs::remove_dir_all(&state.paths.root).unwrap();
    }
}
