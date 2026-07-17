use std::path::{Path, PathBuf};
use std::process::Command;

fn gateway_bin_name() -> &'static str {
    if cfg!(windows) {
        "csswitch-gateway.exe"
    } else {
        "csswitch-gateway"
    }
}

fn stage_gateway_sidecar() {
    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let Ok(target) = std::env::var("TARGET") else {
        return;
    };
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest_dir = PathBuf::from(manifest_dir);
    let gateway_dir = manifest_dir.join("../gateway");
    if !gateway_dir.join("Cargo.toml").is_file() {
        return;
    }

    println!(
        "cargo:rerun-if-changed={}",
        gateway_dir.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        gateway_dir.join("src").display()
    );
    println!("cargo:rerun-if-env-changed=CSSWITCH_SKIP_GATEWAY_STAGE");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_ACCEPTANCE_BUILD");
    let acceptance_build = std::env::var_os("CARGO_FEATURE_ACCEPTANCE_BUILD").is_some();
    if std::env::var_os("CSSWITCH_SKIP_GATEWAY_STAGE").is_some() {
        panic!(
            "CSSwitch builds cannot skip Gateway staging; Desktop and Gateway build variants must match"
        );
    }

    let mut command = Command::new(&cargo);
    command
        .current_dir(&gateway_dir)
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg(&target);
    if acceptance_build {
        command.arg("--features").arg("acceptance-build");
    }
    let status = command.status();
    if !matches!(status, Ok(s) if s.success()) {
        panic!("failed to build csswitch-gateway sidecar for target {target}");
    }

    let built = gateway_dir
        .join("target")
        .join(&target)
        .join("release")
        .join(gateway_bin_name());
    let out_dir = manifest_dir.join("binaries");
    std::fs::create_dir_all(&out_dir).expect("failed to create sidecar binaries dir");
    let mut staged_name = format!("csswitch-gateway-{target}");
    if cfg!(windows) {
        staged_name.push_str(".exe");
    }
    let staged = out_dir.join(staged_name);
    copy_executable(&built, &staged);
}

fn copy_executable(src: &Path, dst: &Path) {
    std::fs::copy(src, dst).unwrap_or_else(|e| {
        panic!(
            "failed to stage csswitch-gateway sidecar from {} to {}: {e}",
            src.display(),
            dst.display()
        )
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dst)
            .expect("failed to stat staged csswitch-gateway sidecar")
            .permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(dst, perms)
            .expect("failed to mark staged csswitch-gateway sidecar executable");
    }
}

fn main() {
    stage_gateway_sidecar();
    tauri_build::build()
}
