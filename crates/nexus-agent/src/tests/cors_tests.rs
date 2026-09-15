#[tokio::test]
async fn api_authorization_rejects_anonymous_origin_tampering_and_replay() {
    use super::*;
    use nexus_core::agent_auth as auth;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!("nexus-auth-{}", auth::random_hex().unwrap()));
    let paths = nexus_core::NexusPaths::from_root(root.clone());
    std::fs::create_dir_all(&paths.run_dir).unwrap();
    let credential = auth::AgentCredential::publish(&paths, "test-generation").unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    let app = Router::new()
        .route(
            "/v1/config",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    "ok"
                }
            }),
        )
        .route("/v1/health", get(|| async { "health" }))
        .layer(middleware::from_fn_with_state(
            ApiAuthorization::new(credential.clone()),
            enforce_api_authorization,
        ))
        .layer(middleware::from_fn(local_console_cors));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    async fn send(
        address: std::net::SocketAddr,
        path: &str,
        headers: &str,
        body: &[u8],
    ) -> (String, Vec<u8>) {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n", body.len()).as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).await.unwrap();
        let boundary = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
        (
            String::from_utf8(bytes[..boundary].to_vec()).unwrap(),
            bytes[boundary..].to_vec(),
        )
    }
    assert!(send(address, "/v1/health", "", b"")
        .await
        .0
        .starts_with("HTTP/1.1 200"));
    assert!(send(address, "/v1/health?x=1", "", b"")
        .await
        .0
        .starts_with("HTTP/1.1 409"));
    assert!(send(address, "/v1/config", "", b"")
        .await
        .0
        .starts_with("HTTP/1.1 409"));
    let nonce = auth::random_hex().unwrap();
    let time = auth::unix_seconds().to_string();
    let ciphertext = credential
        .seal_request("GET", "/v1/config", &nonce, &time, b"")
        .unwrap();
    let signature = credential.request_signature("GET", "/v1/config", &nonce, &time, &ciphertext);
    let headers = format!("x-nexus-auth-version: 2\r\nx-nexus-data-root-id: {}\r\nx-nexus-instance-id: {}\r\n{}: {nonce}\r\n{}: {time}\r\n{}: {signature}\r\n", credential.data_root_id, credential.instance_id, auth::NONCE_HEADER, auth::TIME_HEADER, auth::SIGNATURE_HEADER);
    assert!(send(
        address,
        "/v1/config",
        &(headers.clone() + "Origin: https://evil.invalid\r\n"),
        &ciphertext
    )
    .await
    .0
    .starts_with("HTTP/1.1 403"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let response = send(address, "/v1/config", &headers, &ciphertext).await;
    assert!(response.0.starts_with("HTTP/1.1 200"));
    let proof = response
        .0
        .lines()
        .find_map(|line| line.strip_prefix("x-nexus-auth-response: "))
        .unwrap();
    assert!(credential.verify_response(&nonce, 200, &response.1, proof));
    assert!(send(address, "/v1/config", &headers, &ciphertext)
        .await
        .0
        .starts_with("HTTP/1.1 401"));
    assert_eq!(
        credential.open_response(&nonce, 200, &response.1).unwrap(),
        b"ok"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
    std::fs::remove_dir_all(root).unwrap();
}
use super::{
    acquire_runtime_lock, are_allowed_cors_headers, harness_ui_process_is_presentable,
    is_allowed_console_origin_for_port, is_allowed_cors_method, proxy_identity_values_match,
    recovery_log_payload, recovery_log_tail, redact_config_args, redact_config_url,
    redact_recovery_error, RecoveryStatusResponse, MAX_RECOVERY_LOG_BYTES, PROXY_DATA_ROOT_HEADER,
    PROXY_INSTANCE_HEADER,
};
use nexus_core::HarnessLogSession;
use nexus_core::NexusPaths;
use nexus_protocol::{HarnessResponse, HarnessRuntimeInfo, HarnessState};

#[test]
fn allows_only_the_configured_local_console_port() {
    assert!(is_allowed_console_origin_for_port(
        "http://127.0.0.1:3191",
        3191
    ));
    assert!(is_allowed_console_origin_for_port(
        "http://localhost:3191",
        3191
    ));
    assert!(is_allowed_console_origin_for_port(
        "http://[::1]:3191",
        3191
    ));
    assert!(!is_allowed_console_origin_for_port(
        "http://127.0.0.1:3091",
        3191
    ));
    assert!(!is_allowed_console_origin_for_port(
        "https://127.0.0.1:3191",
        3191
    ));
    assert!(!is_allowed_console_origin_for_port(
        "http://192.168.1.10:3191",
        3191
    ));
    assert!(!is_allowed_console_origin_for_port(
        "http://127.0.0.1:3191/",
        3191
    ));
    assert!(!is_allowed_console_origin_for_port(
        "http://127.0.0.1:3191@localhost",
        3191
    ));
}

#[test]
fn restricts_preflight_methods_and_headers() {
    assert!(is_allowed_cors_method("GET"));
    assert!(is_allowed_cors_method("POST"));
    assert!(!is_allowed_cors_method("DELETE"));
    assert!(are_allowed_cors_headers("content-type"));
    assert!(are_allowed_cors_headers("Content-Type, accept"));
    assert!(!are_allowed_cors_headers("authorization"));
    assert!(!are_allowed_cors_headers("content-type, x-client-secret"));
}

#[test]
fn redacts_token_shaped_config_values_and_readiness_credentials() {
    assert_eq!(
        redact_config_args(vec![
            "--token".to_owned(),
            "secret".to_owned(),
            "token=inline-secret".to_owned(),
            "--api-key=api-secret".to_owned(),
            "--accessToken".to_owned(),
            "camel-secret".to_owned(),
            "--header=Authorization: Bearer header-secret".to_owned(),
            "--tokenize".to_owned(),
            "safe".to_owned(),
        ]),
        vec![
            "--token".to_owned(),
            "[REDACTED]".to_owned(),
            "token=[REDACTED]".to_owned(),
            "--api-key=[REDACTED]".to_owned(),
            "--accessToken".to_owned(),
            "[REDACTED]".to_owned(),
            "[REDACTED]".to_owned(),
            "--tokenize".to_owned(),
            "safe".to_owned(),
        ]
    );
    assert_eq!(
        redact_config_url(Some(
            "http://user:password@127.0.0.1:3080/?token=secret#auth=secret".to_owned()
        )),
        Some("http://127.0.0.1:3080/".to_owned())
    );
    assert_eq!(
        redact_config_url(Some("tcp://127.0.0.1:3080?token=secret".to_owned())),
        Some("tcp://127.0.0.1:3080".to_owned())
    );
}

#[test]
fn runtime_lock_is_owned_for_the_agent_lifetime() {
    let root =
        std::env::temp_dir().join(format!("nexus-agent-runtime-lock-{}", std::process::id()));
    let paths = NexusPaths::from_root(root.clone());
    paths.ensure_directories().expect("directories create");
    let first = acquire_runtime_lock(&paths, "first").expect("first Agent owns root");
    let error = match acquire_runtime_lock(&paths, "second") {
        Ok(_) => panic!("second Agent must not own the same root"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    drop(first);
    let recovered = acquire_runtime_lock(&paths, "replacement")
        .expect("replacement owns root after first exits");
    drop(recovered);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn proxy_identity_headers_must_match_as_a_pair() {
    let mut headers = axum::http::HeaderMap::new();
    assert!(!proxy_identity_values_match(
        &headers,
        "root-a",
        "instance-a"
    ));
    headers.insert(PROXY_DATA_ROOT_HEADER, "root-a".parse().expect("header"));
    assert!(!proxy_identity_values_match(
        &headers,
        "root-a",
        "instance-a"
    ));
    headers.insert(PROXY_INSTANCE_HEADER, "instance-b".parse().expect("header"));
    assert!(!proxy_identity_values_match(
        &headers,
        "root-a",
        "instance-a"
    ));
    headers.insert(PROXY_INSTANCE_HEADER, "instance-a".parse().expect("header"));
    assert!(proxy_identity_values_match(
        &headers,
        "root-a",
        "instance-a"
    ));
}

#[test]
fn recovered_pidless_ui_requires_a_fresh_log_boundary() {
    let runtime = HarnessRuntimeInfo {
        state: HarnessState::Running,
        pid: None,
        exit_code: None,
        error: None,
        started_at_unix: Some(10),
        updated_at_unix: Some(11),
    };
    let response = HarnessResponse::from_observation(
        runtime,
        2,
        "run-2".to_owned(),
        2,
        10,
        20,
        "stdout-id".to_owned(),
        "stderr-id".to_owned(),
        "stdout.log".to_owned(),
        "stderr.log".to_owned(),
        true,
    );
    let session = HarnessLogSession::new(
        "run-2".to_owned(),
        2,
        10,
        20,
        "stdout-id".to_owned(),
        "stderr-id".to_owned(),
        "stdout.log".to_owned(),
        "stderr.log".to_owned(),
        true,
        11,
    );
    assert!(harness_ui_process_is_presentable(&response, &session));

    let mut unreserved = session.clone();
    unreserved.launch_pending = false;
    assert!(!harness_ui_process_is_presentable(&response, &unreserved));

    let attached = HarnessResponse::from_observation(
        HarnessRuntimeInfo {
            pid: Some(42),
            ..response.harness.clone()
        },
        2,
        "run-2".to_owned(),
        2,
        10,
        20,
        "stdout-id".to_owned(),
        "stderr-id".to_owned(),
        "stdout.log".to_owned(),
        "stderr.log".to_owned(),
        false,
    );
    assert!(harness_ui_process_is_presentable(&attached, &unreserved));
}

#[test]
fn recovery_tail_is_bounded_redacted_and_uses_fatal_prefix_as_metadata() {
    let root = std::env::temp_dir().join(format!("nexus-recovery-tail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = NexusPaths::from_root(root.clone());
    paths
        .ensure_directories()
        .expect("Nexus directories create");
    std::fs::write(
        paths.logs_dir.join("current.stderr.log"),
        "[fatal] boot failed\nAuthorization: Bearer DUMMY-RECOVERY-SECRET\n",
    )
    .expect("synthetic log writes");
    let (content, truncated, fatal) =
        recovery_log_tail(&paths, "current.stderr.log").expect("bounded recovery tail reads");
    assert!(fatal);
    assert!(!truncated);
    assert!(!content.contains("DUMMY-RECOVERY-SECRET"));
    assert!(content.contains("[REDACTED]"));
    std::fs::write(
        paths.logs_dir.join("current.stderr.log"),
        vec![b'x'; MAX_RECOVERY_LOG_BYTES as usize + 1024],
    )
    .expect("oversized log fixture writes");
    let (content, truncated, _) =
        recovery_log_tail(&paths, "current.stderr.log").expect("oversized tail reads");
    assert!(truncated);
    assert!(content.len() <= MAX_RECOVERY_LOG_BYTES as usize);
    assert!(recovery_log_tail(&paths, "../outside.log").is_err());
    std::fs::remove_dir_all(root).expect("fixture removes");
}

#[test]
fn recovery_payload_drops_sentinel_and_preserves_utf8_bound_after_redaction() {
    let limit = MAX_RECOVERY_LOG_BYTES as usize;
    let (ascii, _) = recovery_log_payload(vec![b'x'; limit + 1], limit, true);
    assert_eq!(ascii.len(), limit);

    let (multibyte, _) = recovery_log_payload("€".repeat(limit).into_bytes(), limit, true);
    assert!(multibyte.len() <= limit);
    assert!(multibyte.is_char_boundary(multibyte.len()));

    let (invalid, _) = recovery_log_payload(vec![0xff; limit + 1], limit, true);
    assert!(invalid.len() <= limit);
    assert!(invalid.contains("binary diagnostics payload omitted"));
}

#[test]
fn mixed_windows_log_keeps_stack_and_redacts_credentials() {
    let mut bytes = vec![0xce, 0xc4, 0xbc, 0xfe, b'\n'];
    bytes.extend_from_slice(b"Error: task-board ledger is already owned by process 62756\n    at HostTaskLedger.acquireLock (plugin/index.js:1985)\ntoken=do-not-expose-this-secret\n");
    let (text, _) = recovery_log_payload(bytes, 4096, false);
    assert!(text.contains("task-board ledger is already owned"));
    assert!(!text.contains("do-not-expose-this-secret"));
    assert!(!text.contains("binary diagnostics"));
}

#[test]
fn recovery_response_redacts_harness_error_and_startup_error_consistently() {
    let secret = "Authorization: Bearer DUMMY-RECOVERY-SECRET";
    let startup_error = redact_recovery_error(Some(secret));
    let response = RecoveryStatusResponse {
        pause_error: None,
        paused: false,
        api_version: nexus_protocol::API_VERSION.to_owned(),
        manual_entry_available: true,
        harness_stop_required: false,
        harness: HarnessRuntimeInfo {
            state: HarnessState::Stopped,
            pid: None,
            exit_code: None,
            error: startup_error.clone(),
            started_at_unix: None,
            updated_at_unix: None,
        },
        startup_error,
        fatal_prefix_observed: false,
        log_tail: Vec::new(),
        diagnostic_errors: Vec::new(),
        pending_restore: None,
    };
    let encoded = serde_json::to_string(&response).expect("recovery response serializes");
    assert!(!encoded.contains(secret));
    assert!(!encoded.contains("DUMMY-RECOVERY-SECRET"));
    assert_eq!(response.harness.error, response.startup_error);
}
