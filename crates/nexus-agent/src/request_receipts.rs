//! Durable admission for operations whose accidental retry changes data.
//! Receipts never replay a request after an uncertain process interruption.
use std::{collections::HashSet, io, path::PathBuf, sync::Arc};
use axum::{body::{to_bytes, Body}, extract::{Request, State}, http::StatusCode, middleware::Next, response::{IntoResponse, Response}, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const LIFETIME: u64 = 24 * 60 * 60;
const LIMIT: usize = 256;
const BODY_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Receipts { root: PathBuf, active: Arc<Mutex<HashSet<String>>> }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    request_id: String, kind: String, state: String, http_status: u16,
    #[serde(skip_serializing_if = "Option::is_none")] error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] target_id: Option<String>,
    created_at_unix: u64, updated_at_unix: u64,
    fingerprint: String,
    #[serde(default)] context: Option<nexus_core::OperationContext>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document { format_version: u32, requests: Vec<Receipt> }

impl Receipts {
    pub(crate) fn new(root: PathBuf) -> Self { Self { root, active: Arc::new(Mutex::new(HashSet::new())) } }
    fn load(&self, active: &HashSet<String>) -> io::Result<Document> {
        let mut doc = self.read_document()?;
        let now = nexus_core::unix_time_seconds();
        doc.requests.retain(|item| active.contains(&item.request_id) || timestamp(&item.request_id)
            .is_some_and(|issued| issued <= now.saturating_add(60) && now.saturating_sub(issued) <= LIFETIME));
        if doc.requests.len() > LIMIT { return Err(io::Error::other("Request receipt capacity reached")); }
        for item in &mut doc.requests {
            if item.state == "running" && !active.contains(&item.request_id) {
                item.state = "interrupted".into(); item.http_status = 409;
                item.error_code = Some("request_interrupted".into());
            }
        }
        Ok(doc)
    }
    fn read_document(&self) -> io::Result<Document> {
        let doc = match nexus_core::read_regular_file_bounded(&self.root.join("request-receipts.json"), 1024 * 1024)? {
            Some(bytes) => serde_json::from_slice::<Document>(&bytes).map_err(io::Error::other)?,
            None => Document { format_version: 1, requests: Vec::new() },
        };
        if doc.format_version != 1 { return Err(io::Error::other("Unsupported request receipt format")); }
        Ok(doc)
    }
    fn save(&self, doc: &Document) -> io::Result<()> {
        nexus_core::write_private_json_atomic(&self.root, &self.root.join("request-receipts.json"), doc)
    }
}

pub(crate) fn observed_context(paths: &nexus_core::NexusPaths) -> nexus_core::OperationContext {
    let profile = nexus_core::read_regular_file_bounded(&paths.profiles_file, 1024 * 1024).ok().flatten()
        .and_then(|b|serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v|safe_id(v.get("active_profile")));
    let run_id = launch_context(paths).and_then(|v|safe_id(v.get("run_id")));
    nexus_core::OperationContext {
        build_id: option_env!("NEXUS_BUILD_ID").map(str::to_owned),
        version: Some(env!("CARGO_PKG_VERSION").into()), profile,
        config_revision: nexus_core::ConfigStore::new(paths.clone()).snapshot().ok().map(|s|s.revision), run_id,
    }
}
pub(crate) fn launch_context(paths: &nexus_core::NexusPaths) -> Option<Value> {
    let bytes = nexus_core::read_regular_file_bounded(&paths.run_dir.join("harness-log-session.json"), 64 * 1024).ok()??;
    let session: nexus_core::HarnessLogSession = serde_json::from_slice(&bytes).ok()?;
    Some(json!({"run_id":session.run_id,"generation":session.generation,"created_at_unix":session.created_at_unix,"context":session.context}))
}
pub(crate) fn diagnostic_summary(paths: &nexus_core::NexusPaths) -> Value {
    match Receipts::new(paths.root.clone()).read_document() {
        Ok(doc) => json!({"state_source":"last_persisted; use /v1/requests for current owner status", "requests":doc.requests.iter().map(public).collect::<Vec<_>>()}),
        Err(_) => json!({"requests":null,"error_code":"request_receipts_unavailable"}),
    }
}

fn public(item: &Receipt) -> Value {
    let mut value = serde_json::to_value(item).expect("receipt serializes");
    value.as_object_mut().unwrap().remove("fingerprint"); value
}
fn reply(item: &Receipt) -> Response {
    let status = match item.state.as_str() { "running" => 202, "completed" => 200, _ => item.http_status };
    let mut body = json!({"request":public(item)});
    if !matches!(item.state.as_str(), "running" | "completed") {
        body["api_version"] = json!(nexus_protocol::API_VERSION);
        body["code"] = json!(if item.state == "interrupted" { "request_interrupted" } else { "request_previous_failed" });
        body["message"] = json!(format!("Original request {} is {}; original error code: {}. Inspect the operation and diagnostics before starting a new request.", item.request_id, item.state, item.error_code.as_deref().unwrap_or("unavailable")));
    }
    (StatusCode::from_u16(status).unwrap_or(StatusCode::CONFLICT), Json(body)).into_response()
}
fn error(status: StatusCode, code: &str, message: &str) -> Response { super::api_error_response(status, code, message) }
fn timestamp(id: &str) -> Option<u64> {
    let (time, random) = id.split_once('-')?;
    if !matches!(random.len(), 32 | 64) || !random.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) { return None; }
    let value = time.parse::<u64>().ok()?;
    (value.to_string() == time).then_some(value)
}
fn kind(path: &str, value: &Value) -> Option<&'static str> {
    nexus_protocol::request_receipt_kind(path, value.get("action")?.as_str()?)
}
fn safe_id(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).filter(|s| s.len() <= 160 && !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))).map(str::to_owned)
}
pub(crate) async fn list(State(state): State<Receipts>) -> Response {
    let active = state.active.lock().await;
    match state.load(&active) {
        Ok(doc) => Json(json!({"requests":doc.requests.iter().map(public).collect::<Vec<_>>()})).into_response(),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, "request_receipts_unavailable", &e.to_string()),
    }
}
pub(crate) async fn enforce(State(state): State<Receipts>, request: Request, next: Next) -> Response {
    if request.method() != axum::http::Method::POST || !matches!(request.uri().path(), "/v1/releases" | "/v1/updates" | "/v1/checkpoints" | "/v1/harness" | "/v1/profiles") { return next.run(request).await; }
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, BODY_LIMIT).await { Ok(b) => b, Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "Request exceeds the operation budget") };
    let mut value: Value = match serde_json::from_slice(&bytes) { Ok(v) => v, Err(_) => return next.run(Request::from_parts(parts, Body::from(bytes))).await };
    let Some(kind) = kind(parts.uri.path(), &value) else { return next.run(Request::from_parts(parts, Body::from(bytes))).await; };
    let id = value.get("request_id").and_then(Value::as_str).unwrap_or("").to_owned();
    let Some(issued) = timestamp(&id) else { return error(StatusCode::BAD_REQUEST, "request_id_required", "Supply a stable timestamp-random request_id and reuse it after a timeout"); };
    value.as_object_mut().unwrap().remove("request_id");
    let fingerprint = format!("{:x}", Sha256::digest(format!("{}\n{}", parts.uri.path(), value)));
    let now = nexus_core::unix_time_seconds();
    {
        let mut active = state.active.lock().await;
        let mut doc = match state.load(&active) { Ok(d) => d, Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, "request_receipts_unavailable", &e.to_string()) };
        if let Some(item) = doc.requests.iter().find(|r| r.request_id == id) {
            if item.fingerprint != fingerprint { return error(StatusCode::CONFLICT, "request_id_conflict", "This request_id was used with different parameters"); }
            return reply(item);
        }
        if issued > now.saturating_add(60) || now.saturating_sub(issued) > LIFETIME { return error(StatusCode::CONFLICT, "request_id_expired", "The request ID is outside its retry window; inspect the original operation before starting a new request"); }
        if doc.requests.len() >= LIMIT { return error(StatusCode::TOO_MANY_REQUESTS, "request_history_full", "The safe retry history is full; wait for older requests to expire"); }
        let context = Some(observed_context(&nexus_core::NexusPaths::from_root(state.root.clone())));
        doc.requests.push(Receipt { request_id:id.clone(), kind:kind.into(), state:"running".into(), http_status:202, error_code:None, operation_id:None, target_id:safe_id(value.get("id").or_else(||value.get("tag"))), created_at_unix:now, updated_at_unix:now, fingerprint, context });
        if let Err(e) = state.save(&doc) { return error(StatusCode::SERVICE_UNAVAILABLE, "request_receipts_unavailable", &e.to_string()); }
        active.insert(id.clone());
    }
    // The owner continues even if the HTTP client stops waiting. No request is
    // ever replayed merely because its receipt could not be finalized.
    let owner = tokio::spawn(async move {
        let response = tokio::spawn(async move { next.run(Request::from_parts(parts, Body::from(bytes))).await }).await;
        let response = response.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "request_interrupted", "Operation owner stopped unexpectedly; inspect recovery status"));
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, BODY_LIMIT).await;
        let mut active = state.active.lock().await;
        let result = (|| -> io::Result<()> {
            let mut doc = state.load(&active)?;
            let item = doc.requests.iter_mut().find(|r| r.request_id == id).ok_or_else(||io::Error::other("Request receipt disappeared"))?;
            item.state = if parts.status.is_success() && bytes.is_ok() { "completed" } else { "failed" }.into();
            item.http_status = parts.status.as_u16(); item.updated_at_unix = nexus_core::unix_time_seconds();
            if let Ok(bytes) = &bytes {
                if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
                    item.operation_id = safe_id(value.pointer("/operation/operation_id"));
                    item.target_id = safe_id(value.get("current_release").or_else(||value.pointer("/checkpoint/id")).or_else(||value.pointer("/operation/release_id"))).or_else(||item.target_id.clone());
                    item.error_code = safe_id(value.get("code"));
                    if item.error_code.as_deref() == Some("request_interrupted") { item.state = "interrupted".into(); item.http_status = 409; }
                }
            } else { item.state = "interrupted".into(); item.http_status = 409; item.error_code = Some("request_interrupted".into()); }
            state.save(&doc)
        })();
        active.remove(&id);
        if let Err(e) = result { return error(StatusCode::SERVICE_UNAVAILABLE, "request_receipt_finalize_failed", &format!("Inspect recovery before retrying with the same request ID: {e}")); }
        match bytes { Ok(bytes) => Response::from_parts(parts, Body::from(bytes)), Err(_) => error(StatusCode::BAD_GATEWAY, "request_interrupted", "Operation response exceeded its budget; inspect recovery status") }
    });
    owner.await.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "request_interrupted", "Inspect the original request and recovery status"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_and_expired_receipts_are_pruned_before_capacity_but_active_ids_survive() {
        let root = std::env::temp_dir().join(format!("nexus-receipt-prune-{}", nexus_core::agent_auth::random_hex().unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        let state = Receipts::new(root.clone());
        let now = nexus_core::unix_time_seconds();
        let make = |id: String| Receipt { request_id: id, kind: "rollback".into(), state: "running".into(), http_status: 202,
            error_code: None, operation_id: None, target_id: None, created_at_unix: now, updated_at_unix: now, fingerprint: String::new(), context: None };
        let fresh = format!("{now}-{:032x}", 999);
        let active_id = "broken-active".to_string();
        let mut requests = (0..LIMIT + 1).map(|i| make(format!("broken-{i}"))).collect::<Vec<_>>();
        requests.push(make(format!("{}-{:032x}", now - LIFETIME - 1, 1)));
        requests.push(make(fresh.clone())); requests.push(make(active_id.clone()));
        state.save(&Document { format_version: 1, requests }).unwrap();
        let doc = state.load(&HashSet::from([active_id.clone()])).unwrap();
        assert_eq!(doc.requests.len(), 2);
        assert_eq!(doc.requests[0].request_id, fresh); assert_eq!(doc.requests[0].state, "interrupted");
        assert_eq!(doc.requests[1].request_id, active_id); assert_eq!(doc.requests[1].state, "running");
        state.save(&doc).unwrap();
        assert_eq!(state.load(&HashSet::new()).unwrap().requests.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    use axum::{middleware, routing::post, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn send(address: std::net::SocketAddr, path: &str, body: Value) -> (u16, Value) {
        let body = body.to_string();
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("POST {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        let mut bytes = Vec::new(); socket.read_to_end(&mut bytes).await.unwrap();
        let boundary = bytes.windows(4).position(|b|b==b"\r\n\r\n").unwrap()+4;
        let status = String::from_utf8_lossy(&bytes[..boundary]).split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_slice(&bytes[boundary..]).unwrap())
    }
    #[tokio::test]
    async fn receipt_retries_do_not_repeat_effects_and_restart_is_honest() {
        let root = std::env::temp_dir().join(format!("nexus-receipt-{}", nexus_core::agent_auth::random_hex().unwrap()));
        std::fs::create_dir_all(&root).unwrap();
        let state = Receipts::new(root.clone());
        let count = Arc::new(AtomicUsize::new(0)); let counter = count.clone();
        let handler = move || { let counter = counter.clone(); async move {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            (StatusCode::ACCEPTED, Json(json!({"operation":{"operation_id":"cold-original"}})))
        }};
        let app = Router::new().route("/v1/releases", post(handler.clone()))
            .route("/v1/harness", post(handler.clone())).route("/v1/updates", post(handler.clone())).route("/v1/checkpoints", post(handler.clone())).route("/v1/profiles", post(handler))
            .layer(middleware::from_fn_with_state(state.clone(), enforce));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let now = nexus_core::unix_time_seconds();
        for (index, path, action) in [(0,"/v1/releases","rollback"), (1,"/v1/updates","switch"), (2,"/v1/updates","offline_import"), (3,"/v1/checkpoints","restore"),(4,"/v1/harness","restart"),(5,"/v1/profiles","delete"),(6,"/v1/profiles","restore_deleted")] {
            let id = format!("{now}-{index:032x}");
            let body = json!({"request_id":id,"action":action,"id":"safe-target"});
            let (first, second) = tokio::join!(send(address,path,body.clone()),send(address,path,body.clone()));
            assert_eq!(first.0,202); assert_eq!(second.0,202);
            let repeat = send(address,path,body.clone()).await;
            assert_eq!(repeat.0,200); assert_eq!(repeat.1["request"]["state"],"completed");
            assert_eq!(repeat.1["request"]["operation_id"],"cold-original");
            assert_eq!(count.load(Ordering::SeqCst),index+1);
            let mut changed = body; changed["id"] = json!("different");
            assert_eq!(send(address,path,changed).await.0,409);
        }
        let active = state.active.lock().await;
        let mut doc = state.load(&active).unwrap();
        doc.requests[0].state = "running".into(); state.save(&doc).unwrap(); drop(active);
        let restarted = Receipts::new(root.clone());
        let doc = restarted.load(&HashSet::new()).unwrap();
        assert_eq!(doc.requests[0].state,"interrupted"); assert_eq!(reply(&doc.requests[0]).status(),StatusCode::CONFLICT);
        let listing = list(State(restarted)).await;
        let listing: Value = serde_json::from_slice(&to_bytes(listing.into_body(),BODY_LIMIT).await.unwrap()).unwrap();
        assert_eq!(listing["requests"][1]["state"],"completed");
        assert!(listing["requests"][0].get("fingerprint").is_none());
        assert_eq!(send(address,"/v1/releases",json!({"action":"rollback","request_id":format!("{}-{:032x}",now-LIFETIME-1,99)})).await.0,409);
        assert_eq!(count.load(Ordering::SeqCst),7);
        server.abort(); let _=server.await; std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn receipt_failure_preserves_original_error_and_timeout_status() {
        let root = std::env::temp_dir().join(format!("nexus-receipt-{}", nexus_core::agent_auth::random_hex().unwrap())); std::fs::create_dir_all(&root).unwrap();
        let state = Receipts::new(root.clone());
        let app = Router::new().route("/v1/releases",post(||async { super::super::data_error_response(io::Error::new(io::ErrorKind::TimedOut,"original upstream failure"),"upstream_timeout") }))
            .layer(middleware::from_fn_with_state(state,enforce));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let address=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        let body=json!({"action":"rollback","request_id":format!("{}-{:032x}",nexus_core::unix_time_seconds(),1)});
        let original=send(address,"/v1/releases",body.clone()).await;
        assert_eq!(original.0,504); assert_eq!(original.1["message"],"original upstream failure");
        let repeated=send(address,"/v1/releases",body).await;
        assert_eq!(repeated.0,504); assert_eq!(repeated.1["code"],"request_previous_failed"); assert_eq!(repeated.1["request"]["error_code"],"upstream_timeout");
        server.abort(); let _=server.await; std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn disconnected_client_owner_finishes_and_receipt_retains_original_context() {
        let root = std::env::temp_dir().join(format!("nexus-receipt-{}", nexus_core::agent_auth::random_hex().unwrap()));
        let paths = nexus_core::NexusPaths::from_root(root.clone()); std::fs::create_dir_all(&paths.run_dir).unwrap();
        std::fs::write(&paths.profiles_file, br#"{"active_profile":"original-profile"}"#).unwrap();
        let state = Receipts::new(root.clone());
        let started = Arc::new(tokio::sync::Semaphore::new(0)); let release = Arc::new(tokio::sync::Semaphore::new(0));
        let count = Arc::new(AtomicUsize::new(0));
        let (ready, gate, effects) = (started.clone(), release.clone(), count.clone());
        let app = Router::new().route("/v1/releases", post(move || {
            let (ready, gate, effects) = (ready.clone(), gate.clone(), effects.clone());
            async move { ready.add_permits(1); gate.acquire().await.unwrap().forget(); effects.fetch_add(1,Ordering::SeqCst); Json(json!({"current_release":"original-slot"})) }
        })).layer(middleware::from_fn_with_state(state.clone(),enforce));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(); let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {axum::serve(listener,app).await.unwrap();});
        let id = format!("{}-{:032x}",nexus_core::unix_time_seconds(),7);
        let body = json!({"request_id":id,"action":"rollback","note":"SECRET-SENTINEL"}); let raw = body.to_string();
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("POST /v1/releases HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{raw}",raw.len()).as_bytes()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2),started.acquire()).await.unwrap().unwrap().forget();
        drop(socket); // The actual request connection is gone before its effect.
        std::fs::write(&paths.profiles_file, br#"{"active_profile":"later-profile"}"#).unwrap();
        release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(2),async {
            loop {
                if state.read_document().unwrap().requests[0].state == "completed" {break;}
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        let repeated = send(address,"/v1/releases",body).await;
        assert_eq!(repeated.0,200); assert_eq!(count.load(Ordering::SeqCst),1);
        assert_eq!(repeated.1["request"]["target_id"],"original-slot");
        assert_eq!(repeated.1["request"]["context"]["profile"],"original-profile");
        assert_eq!(repeated.1["request"]["context"]["version"],env!("CARGO_PKG_VERSION"));
        let summary = diagnostic_summary(&paths).to_string();
        assert!(!summary.contains("SECRET-SENTINEL")); assert!(!summary.contains("fingerprint"));
        assert!(!std::fs::read_to_string(root.join("request-receipts.json")).unwrap().contains("SECRET-SENTINEL"));
        // Records from before correlation metadata remain readable.
        let mut legacy:Value = serde_json::from_slice(&std::fs::read(root.join("request-receipts.json")).unwrap()).unwrap();
        legacy["requests"][0].as_object_mut().unwrap().remove("context");
        std::fs::write(root.join("request-receipts.json"),legacy.to_string()).unwrap();
        assert!(state.read_document().unwrap().requests[0].context.is_none());
        server.abort(); let _=server.await; std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn launch_context_is_safe_and_old_session_has_unknown_metadata() {
        let root = std::env::temp_dir().join(format!("nexus-context-{}",nexus_core::agent_auth::random_hex().unwrap()));
        let paths = nexus_core::NexusPaths::from_root(root.clone()); std::fs::create_dir_all(&paths.run_dir).unwrap();
        let mut session = nexus_core::HarnessLogSession::new("run-fixture".into(),1,0,0,"stdout-fixture".into(),"stderr-fixture".into(),"out.log".into(),"err.log".into(),true,123);
        session.context = Some(nexus_core::OperationContext { build_id:Some("build-fixture".into()),version:Some("0.1.2".into()),profile:Some("effective-profile".into()),config_revision:Some("sha256:fixture".into()),run_id:Some(session.run_id.clone()) });
        nexus_core::HarnessLogSessionStore::new(paths.clone()).write(&session).unwrap();
        let value=launch_context(&paths).unwrap();
        assert_eq!(value["context"]["profile"],"effective-profile");
        assert_eq!(value["context"]["config_revision"],"sha256:fixture");
        assert!(value.get("stdout_file_identity").is_none());
        let mut legacy=serde_json::to_value(session).unwrap(); legacy.as_object_mut().unwrap().remove("context");
        std::fs::write(paths.run_dir.join("harness-log-session.json"),legacy.to_string()).unwrap();
        assert!(launch_context(&paths).unwrap()["context"].is_null());
        assert!(nexus_core::HarnessLogSessionStore::new(paths).read().unwrap().unwrap().context.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
