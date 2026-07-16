#[cfg(unix)]
#[test]
fn non_utf8_codex_argument_is_redacted_structured_error() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Command;

    let output = Command::new(env!("CARGO_BIN_EXE_csswitch-gateway"))
        .arg("codex-auth")
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "invalid_arguments");
}

#[test]
fn managed_keychain_namespace_mismatch_fails_before_auth_dispatch() {
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let root = std::env::temp_dir().join(format!(
        "csswitch-auth-handshake-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_csswitch-gateway"))
        .args(["codex-auth", "status"])
        .env_clear()
        .env("HOME", &root)
        .env("CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE", "wrong.service")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(8));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "internal_error");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);

    let matching = Command::new(env!("CARGO_BIN_EXE_csswitch-gateway"))
        .args(["codex-auth", "invalid"])
        .env_clear()
        .env("HOME", &root)
        .env(
            "CSSWITCH_EXPECTED_CODEX_KEYCHAIN_SERVICE",
            csswitch_gateway::codex_auth::OAUTH_KEYCHAIN_SERVICE,
        )
        .output()
        .unwrap();
    assert_eq!(matching.status.code(), Some(2));
    let matching_value: serde_json::Value = serde_json::from_slice(&matching.stdout).unwrap();
    assert_eq!(matching_value["error"]["code"], "invalid_arguments");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(root);
}
