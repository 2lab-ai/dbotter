use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

#[test]
fn config_contract_stdout_is_the_exact_three_field_object_and_is_pure() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args([
            "--config",
            directory.path().to_string_lossy().as_ref(),
            "config-contract",
            "--format",
            "json",
        ])
        .output()
        .expect("run config-contract");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"read_versions\":[1,2],\"write_version\":2,\"migration_backup_suffix\":\".v1.bak\"}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn version_stdout_is_exactly_the_separate_six_field_identity() {
    let output = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args(["version", "--format", "json"])
        .output()
        .expect("run version");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("identity JSON");
    let keys: BTreeSet<_> = value
        .as_object()
        .expect("identity object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "arch",
            "build_id",
            "channel",
            "package_version",
            "source_sha",
            "target",
        ])
    );
    assert!(value.get("read_versions").is_none());
}

#[test]
fn malformed_config_and_backend_details_never_cross_cli_stderr() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");
    let sentinel = "backend-top-secret-sentinel";
    fs::write(&path, format!("version = {sentinel}\n")).expect("malformed fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args([
            "--config",
            path.to_string_lossy().as_ref(),
            "check",
            "--profile",
            "missing",
        ])
        .output()
        .expect("run check");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(!stderr.contains(sentinel));
    assert_eq!(stderr, "error: The configuration was not changed.\n");
}
