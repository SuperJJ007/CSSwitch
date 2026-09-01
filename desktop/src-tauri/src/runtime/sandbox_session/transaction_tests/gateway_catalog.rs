use super::*;

fn profile_with_policy(policy: ModelPolicy) -> config::Profile {
    config::Profile {
        model_policy: policy,
        ..Default::default()
    }
}

fn serve_models_after(
    delay: Duration,
    body: impl Into<String>,
) -> (u16, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let body = body.into();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    assert_ne!(port, 8765);
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = requests.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let read = stream.read(&mut request).unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.starts_with("GET /test-secret/v1/models HTTP/1.0\r\n"));
        server_requests.fetch_add(1, Ordering::SeqCst);
        thread::sleep(delay);
        write!(
                stream,
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
    });
    (port, requests, server)
}

#[test]
fn gateway_catalog_timeout_matches_model_policy_contract() {
    assert_eq!(
        gateway_model_catalog_timeout_ms(&profile_with_policy(ModelPolicy::DynamicCatalog)),
        crate::runtime::operation::CODEX_MODELS_PROBE_TIMEOUT_MS
    );
    assert_eq!(
        gateway_model_catalog_timeout_ms(&profile_with_policy(ModelPolicy::SavedCatalog)),
        crate::runtime::operation::LOCAL_HEALTH_TIMEOUT_MS
    );
}

#[test]
fn dynamic_catalog_cold_response_uses_one_long_local_request() {
    let body = r#"{"data":[
            {"id":"claude-csswitch-codex-gpt-5"},
            {"id":"claude-opus-5"},
            {"id":"claude-sonnet-5"},
            {"id":"claude-haiku-4-5-20251001"}
        ]}"#;
    let (port, requests, server) = serve_models_after(Duration::from_millis(600), body);
    let profile = profile_with_policy(ModelPolicy::DynamicCatalog);

    verify_gateway_model_catalog(port, "test-secret", &profile).unwrap();
    server.join().unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[test]
fn dynamic_catalog_accepts_reviewer_only_snapshot_for_three_standard_models() {
    let body = r#"{"data":[
            {"id":"claude-opus-5"},
            {"id":"claude-sonnet-5"},
            {"id":"claude-haiku-4-5-20251001"}
        ]}"#;
    let (port, _requests, server) = serve_models_after(Duration::ZERO, body);
    let profile = profile_with_policy(ModelPolicy::DynamicCatalog);

    verify_gateway_model_catalog(port, "test-secret", &profile).unwrap();
    server.join().unwrap();
}

#[test]
fn dynamic_catalog_still_rejects_empty_or_non_codex_aliases() {
    for body in [
        r#"{"data":[]}"#,
        r#"{"data":[{"id":"gpt-5"}]}"#,
        r#"{"data":[{"id":"claude-sonnet-5"}]}"#,
        r#"{"data":[{"id":"claude-csswitch-codex-gpt-5"},{"id":"unknown-alias"}]}"#,
        r#"{"data":[{"id":"claude-csswitch-codex-"}]}"#,
        r#"{"data":[
                {"id":"claude-csswitch-codex-gpt-5"},
                {"id":"claude-opus-5"},
                {"id":"claude-haiku-4-5-20251001"}
            ]}"#,
    ] {
        let (port, _requests, server) = serve_models_after(Duration::ZERO, body);
        let profile = profile_with_policy(ModelPolicy::DynamicCatalog);
        let error = verify_gateway_model_catalog(port, "test-secret", &profile).unwrap_err();
        assert!(error.contains("Codex published model snapshot"));
        server.join().unwrap();
    }
}

#[test]
fn gateway_catalog_rejects_malformed_or_duplicate_rows_without_filtering_them() {
    for body in [
        r#"{"data":[{"id":"claude-csswitch-codex-gpt-5"},{}]}"#,
        r#"{"data":[{"id":"claude-csswitch-codex-gpt-5"},{"id":7}]}"#,
        r#"{"data":[{"id":"claude-csswitch-codex-gpt-5"},"ignored-before-flow3"]}"#,
        r#"{"data":[{"id":"claude-csswitch-codex-gpt-5"},{"id":"claude-csswitch-codex-gpt-5"}]}"#,
    ] {
        let (port, _requests, server) = serve_models_after(Duration::ZERO, body);
        let profile = profile_with_policy(ModelPolicy::DynamicCatalog);
        let error = verify_gateway_model_catalog(port, "test-secret", &profile).unwrap_err();
        assert!(
            error.contains("malformed") || error.contains("duplicate"),
            "strict catalog parser must reject the exact malformed row: {error}"
        );
        server.join().unwrap();
    }
}

#[test]
fn saved_catalog_requires_exact_selectors_plus_science_canonical_roles() {
    let selector = "claude-csswitch-relay-mock-model-0123456789ab";
    let profile = config::Profile {
        model_policy: ModelPolicy::SavedCatalog,
        model_catalog: vec![crate::model_catalog::ModelRoute {
            selector_id: selector.into(),
            display_name: "Mock model".into(),
            upstream_model: "mock-model".into(),
            supports_tools: Some(true),
            ..Default::default()
        }],
        default_model_route_id: selector.into(),
        ..Default::default()
    };
    let body = r#"{"data":[
            {"id":"claude-csswitch-relay-mock-model-0123456789ab"},
            {"id":"claude-opus-5"},
            {"id":"claude-sonnet-5"},
            {"id":"claude-opus-4-8"},
            {"id":"claude-sonnet-4-6"},
            {"id":"claude-haiku-4-5-20251001"}
        ]}"#;
    let (port, _requests, server) = serve_models_after(Duration::ZERO, body);
    verify_gateway_model_catalog(port, "test-secret", &profile).unwrap();
    server.join().unwrap();

    let unknown = r#"{"data":[
            {"id":"claude-csswitch-relay-mock-model-0123456789ab"},
            {"id":"claude-opus-5"},
            {"id":"claude-sonnet-5"},
            {"id":"claude-opus-4-8"},
            {"id":"claude-sonnet-4-6"},
            {"id":"claude-haiku-4-5-20251001"},
            {"id":"claude-csswitch-stale-provider"}
        ]}"#;
    let (port, _requests, server) = serve_models_after(Duration::ZERO, unknown);
    let error = verify_gateway_model_catalog(port, "test-secret", &profile).unwrap_err();
    assert!(error.contains("白名单/default selector"));
    server.join().unwrap();
}
