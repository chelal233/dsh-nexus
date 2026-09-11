//! Bounded live-log allocation maintenance. Never truncates inherited handles.
use std::sync::{Mutex, OnceLock};
use nexus_core::{NexusPaths, HarnessLogSessionStore};
use nexus_launcher_core::{HarnessLogObserver, harness_ui::retain_harness_logs};
use serde_json::{json, Value};
use tokio::{sync::watch, task::JoinHandle};

static STATUS: OnceLock<Mutex<Value>> = OnceLock::new();
const CATCH_UP_MS: u64 = 100;
fn next_delay_ms(report: &Value) -> u64 {
    if report["state"] == "catching_up" { CATCH_UP_MS } else { nexus_core::log_retention::INTERVAL_SECS * 1000 }
}
pub(crate) fn status() -> Value {
    STATUS.get_or_init(|| Mutex::new(json!({"state":"pending"}))).lock()
        .map(|value| value.clone()).unwrap_or_else(|_| json!({"state":"unavailable"}))
}
fn limitation(error: std::io::Error) -> Value {
    // Never include raw paths, log contents, credential bytes, or parser excerpts.
    json!({"state":"limited", "kind":format!("{:?}", error.kind()), "os_code":error.raw_os_error(),
        "message":"Live log allocation retention could not complete; original logs were retained. No truncation fallback is used."})
}
fn cycle(paths: &NexusPaths, observer: &mut HarnessLogObserver) -> Value {
    let mut files = serde_json::Map::new();
    match HarnessLogSessionStore::new(paths.clone()).read() {
        Ok(Some(session)) => match retain_harness_logs(paths, observer, &session) {
            Ok(results) => for (name, status, backlog) in results {
                let over_target = status.allocated_bytes > nexus_core::log_retention::TAIL_BYTES + 256 * 1024;
                files.insert(name, json!({"state":if backlog != 0 {"scan_backlog"} else if over_target {"allocation_above_target"} else {"active"},
                    "allocation":status, "scan_backlog_bytes":backlog, "allocation_above_target":over_target}));
            },
            Err(error) => { files.insert("harness".into(), limitation(error)); },
        },
        Ok(None) => {},
        Err(error) => { files.insert("harness".into(), limitation(error)); },
    }
    for name in ["agent.stdout.log", "agent.stderr.log"] {
        match nexus_core::log_retention::maintain(&paths.logs_dir.join(name), None, &[], None) {
            Ok(status) => { files.insert(name.into(), json!({"state":"active", "allocation":status})); },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => { files.insert(name.into(), limitation(error)); },
        }
    }
    let catching_up = files.values().any(|file| file["state"] == "scan_backlog");
    let limited = files.values().any(|file| file["state"] == "limited" || file["state"] == "allocation_above_target");
    json!({"state":if catching_up {"catching_up"} else if limited {"limited"} else {"available"},
        "mode":"physical_allocation", "logical_size_is_unbounded":true,
        "next_check_ms":if catching_up {CATCH_UP_MS} else {nexus_core::log_retention::INTERVAL_SECS * 1000},
        "interval_seconds":nexus_core::log_retention::INTERVAL_SECS,
        "tail_bytes_per_stream":nexus_core::log_retention::TAIL_BYTES,
        "scan_bytes_per_harness_stream_per_cycle":512 * 1024,
        "updated_at_unix":nexus_core::unix_time_seconds(),"files":files})
}
pub(crate) fn start(paths: NexusPaths, mut shutdown: watch::Receiver<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut observer = HarnessLogObserver::default();
        loop {
            if *shutdown.borrow() { return; }
            let task_paths = paths.clone();
            let result = tokio::task::spawn_blocking(move || {
                let report = cycle(&task_paths, &mut observer);
                (observer, report)
            }).await;
            let delay_ms = match result {
                Ok((next, report)) => {
                    observer = next;
                    let delay = next_delay_ms(&report);
                    if let Ok(mut status) = STATUS.get_or_init(|| Mutex::new(Value::Null)).lock() { *status = report; }
                    delay
                },
                Err(_) => {
                    if let Ok(mut status) = STATUS.get_or_init(|| Mutex::new(Value::Null)).lock() { *status = json!({"state":"limited", "message":"Log retention worker failed; original logs retained"}); }
                    return;
                },
            };
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {},
                _ = shutdown.changed() => { if *shutdown.borrow() { return; } },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backlog_uses_short_bounded_slices_and_failures_stay_redacted() {
        assert_eq!(next_delay_ms(&json!({"state":"catching_up"})), 100);
        assert_eq!(next_delay_ms(&json!({"state":"available"})), 5000);
        let safe = limitation(std::io::Error::other("token=PRIVATE_SHOULD_NOT_LEAK C:/private/log"));
        assert!(!safe.to_string().contains("PRIVATE_SHOULD_NOT_LEAK"));
        assert_eq!(next_delay_ms(&safe), 5000);
    }
}
