// Appended to checkpoint_tests.rs in isolated A/B copies, never to the shipped Agent.
#[tokio::test]
async fn ab_checkpoint_observations() {
    use serde_json::{json, Value};
    use std::sync::atomic::Ordering;
    fn normalize(value: &mut Value, root: &str, checkpoint: &str) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    if ["created_at_unix", "updated_at_unix", "started_at_unix", "installed_at_unix"].contains(&key.as_str()) {
                        *value = json!(0);
                    } else { normalize(value, root, checkpoint); }
                }
            }
            Value::Array(values) => for value in values { normalize(value, root, checkpoint); },
            Value::String(text) => *text = text.replace(root, "<fixture>").replace(checkpoint, "<checkpoint>"),
            _ => {}
        }
    }
    let mut observations = Vec::new();
    for scenario in ["success", "missing", "missing-profile", "missing-release", "busy", "persist-failure", "commit-uncertain", "cancel"] {
        let (state, root) = content_test_state("ab");
        state.profiles.write(&ProfileCatalog::new("demo", vec!["demo".into(), "alternate".into()]).unwrap()).unwrap();
        state.releases.register("alpha", "1", None, None).unwrap();
        state.releases.register("beta", "2", None, None).unwrap();
        state.releases.promote("beta").unwrap();
        update_agent_state(&state, |runtime| runtime.set_release(Some("beta".into()))).await.unwrap();
        let profile = if scenario == "missing-profile" { "absent" } else { "alternate" };
        let release = if scenario == "missing-release" { "absent" } else { "alpha" };
        let checkpoint = state.checkpoints.create(profile, Some(release.into()), None,
            NexusStateSnapshot { profile: profile.into(), release: Some(release.into()) }).unwrap();
        let held = if scenario == "busy" { Some(state.updater.try_acquire_gate().unwrap()) } else { None };
        state.agent_persist_failure.store(scenario == "persist-failure", Ordering::SeqCst);
        state.checkpoint_commit_result_failure.store(scenario == "commit-uncertain", Ordering::SeqCst);
        let mut during = Value::Null;
        let response = if scenario == "cancel" {
            let (reached, reached_rx) = oneshot::channel();
            let (release_tx, release) = oneshot::channel();
            *state.checkpoint_transition_gate.lock().await = Some(CheckpointTransitionGate { reached, release });
            let owner = state.clone();
            let id = checkpoint.id.clone();
            let request = tokio::spawn(async move { checkpoint_restore(owner, id).await });
            timeout(Duration::from_secs(10), reached_rx).await.unwrap().unwrap();
            during = json!({ "journal": state.checkpoint_restores.load().unwrap(),
                "update_locked": state.updater.try_acquire_gate().is_err() });
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            release_tx.send(()).unwrap();
            drop(timeout(Duration::from_secs(10), state.supervisor.acquire_lifecycle()).await.unwrap());
            json!({ "cancelled": true })
        } else {
            let id = if scenario == "missing" { "absent".into() } else { checkpoint.id.clone() };
            let response = checkpoint_restore(state.clone(), id).await;
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            json!({ "status": status, "body": serde_json::from_slice::<Value>(&body).unwrap() })
        };
        drop(held);
        let mut result = json!({ "scenario": scenario, "response": response, "during": during,
            "profiles": state.profiles.load().unwrap(), "releases": state.releases.load().unwrap(),
            "journal": state.checkpoint_restores.load().unwrap(),
            "runtime": state.runtime.read().await.as_payload(),
            "durable_runtime": state.supervisor.metadata_store().read().unwrap(),
            "update_unlocked": state.updater.try_acquire_gate().is_ok() });
        normalize(&mut result, &root.to_string_lossy(), &checkpoint.id);
        observations.push(result);
        fs::remove_dir_all(root).unwrap();
    }
    fs::write(std::env::var_os("NEXUS_AB_REPORT").unwrap(), serde_json::to_vec_pretty(&json!({ "manifest": env!("CARGO_MANIFEST_DIR"), "implementation": include_str!("../checkpoint_api.rs"), "observations": observations })).unwrap()).unwrap();
}
