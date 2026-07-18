use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn repository_path(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn tracked_source(path: &str) -> String {
    fs::read_to_string(repository_path(path))
        .unwrap_or_else(|_| panic!("missing canonical J2 installed dependency: {path}"))
}

#[test]
fn installed_j2_verifier_owns_all_six_exact_acceptance_steps() {
    let verifier = tracked_source("scripts/verify-installed-j2.sh");
    for required in [
        "set -euo pipefail",
        "--app-path",
        "--config",
        "--manifest",
        "--mysql-container",
        "--output",
        "scripts/build-native-j2-ax-driver.sh",
        "scripts/native-j2-ax-driver.swift",
        "--phase seed",
        "--phase restart",
        "--phase second-instance",
        "--phase corrupt-reopen",
        "kill -KILL \"$seed_pid\"",
        "workspace_manifest_generation",
        "zero_dispatch_before",
        "zero_dispatch_after_open",
        "command_type = 'Execute'",
        "fresh_result_after_explicit_run",
        "second_instance_read_only",
        "corrupt_profile_quarantined",
        "healthy_profile_remains_usable",
        "current_selection_all_exercised",
        "syntax_autocomplete_exercised",
        "result_inspection_completed",
        "history_filters_and_metrics_visible",
        "history_source_exact_retained",
        "history_source_plus_one_omitted",
        "tab_bound_enforced",
        "shard_bound_enforced",
        "persistence_opt_out_and_clear",
        "private_store_payload_scan_clean",
        "MAX_PROFILE_SHARD_BYTES=33554432",
        "scripts/scan-private-workspace.py",
    ] {
        assert!(
            verifier.contains(required),
            "installed J2 verifier is missing exact acceptance token `{required}`"
        );
    }
    assert!(
        !verifier.contains("command_type = 'Query' AND argument = 'SELECT 42 AS j2_second'"),
        "prepared MySQL execution must be counted from Execute rows, not text Query rows"
    );
}

#[test]
fn native_j2_driver_emits_only_sanitized_checkpoint_truth() {
    let driver = tracked_source("scripts/native-j2-ax-driver.swift");
    for phase in ["seed", "restart", "second-instance", "corrupt-reopen"] {
        assert!(
            driver.contains(&format!("case \"{phase}\"")),
            "native J2 driver is missing phase `{phase}`"
        );
    }
    for identifier in [
        "connection.profile.",
        "editor.tab.new",
        "editor.tab.title",
        "editor.tab.move_left",
        "editor.tab.move_right",
        "editor.save",
        "workspace.persistence.status",
        "result.tab.history",
        "history.search",
        "history.entry.",
        "editorTabIdentifiers",
        "tabIdentifiersBefore",
        "tabIdentifiersBefore.count + 1",
        "editor.autocomplete.candidate.",
        "result.mode.grid",
        "result.mode.record",
        "result.filter",
        "result.sort.0",
        "result.copy.cell",
        "workspace.persistence.toggle",
        "workspace.persistence.clear",
        "workspace.persistence.clear.confirm",
        "navigator.catalog.refresh-schemas",
        "configuration.environment = [credentialEnvName: credential]",
        "configuration.allowsRunningApplicationSubstitution = false",
    ] {
        assert!(
            driver.contains(identifier),
            "native J2 driver is missing AX boundary `{identifier}`"
        );
    }
    for checkpoint in [
        "tabs_created_renamed_reordered",
        "saved_visible_before_kill",
        "tabs_restored",
        "results_omitted_after_restart",
        "history_opened_without_run",
        "explicit_run_completed",
        "second_instance_read_only",
        "corrupt_profile_quarantined",
        "healthy_profile_remains_usable",
        "current_selection_all_exercised",
        "syntax_autocomplete_exercised",
        "result_inspection_completed",
        "history_filters_and_metrics_visible",
        "history_source_exact_retained",
        "history_source_plus_one_omitted",
        "tab_bound_enforced",
        "persistence_opt_out_and_clear",
    ] {
        assert!(
            driver.contains(checkpoint),
            "native J2 driver is missing sanitized checkpoint `{checkpoint}`"
        );
    }
    assert!(
        driver.contains("dbotter.installed-j2-ax-observations.v1"),
        "native J2 observations need a versioned safe schema"
    );
}

#[test]
fn j2_driver_builder_is_reproducible_and_source_bound() {
    let builder = tracked_source("scripts/build-native-j2-ax-driver.sh");
    for required in [
        "set -euo pipefail",
        "scripts/native-j2-ax-driver.swift",
        "xcrun --sdk macosx swiftc",
        "-O",
        "-whole-module-optimization",
        "-framework",
        "ApplicationServices",
        "-framework",
        "AppKit",
    ] {
        assert!(
            builder.contains(required),
            "J2 driver builder is missing reproducible input `{required}`"
        );
    }
}

#[test]
fn preview_source_verification_tracks_the_j2_installed_dependencies() {
    let hermetic = tracked_source("scripts/verify-hermetic.sh");
    for dependency in [
        "tests/daily_use_j2_installed_contract.rs",
        "scripts/verify-installed-j2.sh",
        "scripts/native-j2-ax-driver.swift",
        "scripts/build-native-j2-ax-driver.sh",
        "scripts/scan-private-workspace.py",
        "tests/fixtures/installed-j2/compose.yml",
    ] {
        assert!(
            hermetic.contains(dependency),
            "hermetic Preview verification is missing J2 dependency `{dependency}`"
        );
    }

    let release = tracked_source("scripts/check-release-contract.sh");
    for dependency in [
        "scripts/verify-installed-j2.sh",
        "scripts/native-j2-ax-driver.swift",
        "scripts/build-native-j2-ax-driver.sh",
    ] {
        assert!(
            release.contains(dependency),
            "release contract is missing installed J2 dependency `{dependency}`"
        );
    }
}

#[test]
fn installed_fixture_and_private_scanner_are_fail_closed_and_consumed() {
    let verifier = tracked_source("scripts/verify-installed-j2.sh");
    for required in [
        "com.docker.compose.project",
        "dbotter-installed-j2",
        "com.docker.compose.service",
        "ai.2lab.dbotter.fixture",
        "installed-j2-v1",
        "127.0.0.1\",\"HostPort\":\"33316",
        "scripts/scan-private-workspace.py",
        "--forbidden-env DBOTTER_MYSQL_PASSWORD",
        "--forbidden-env DBOTTER_MYSQL_ROOT_PASSWORD",
        "--forbidden-env DBOTTER_J2_RESULT_SENTINEL",
        "SELECT CONCAT(@@global.general_log, ':', @@global.log_output)",
        "mysql@sha256:",
    ] {
        assert!(
            verifier.contains(required),
            "installed verifier does not enforce fixture/scanner token `{required}`"
        );
    }
    for forbidden in ["TRUNCATE TABLE mysql.general_log", "SET GLOBAL general_log"] {
        assert!(
            !verifier.contains(forbidden),
            "installed verifier must not mutate shared MySQL logging with `{forbidden}`"
        );
    }

    let compose = tracked_source("tests/fixtures/installed-j2/compose.yml");
    for required in [
        "name: dbotter-installed-j2",
        "image: mysql:8.4",
        "--general-log=ON",
        "--log-output=TABLE",
        "ai.2lab.dbotter.fixture: installed-j2-v1",
        "127.0.0.1:33316:3306",
        "/var/lib/mysql",
    ] {
        assert!(
            compose.contains(required),
            "installed fixture is missing isolation token `{required}`"
        );
    }

    let scanner = tracked_source("scripts/scan-private-workspace.py");
    for required in [
        "MAX_FILE_BYTES",
        "MAX_TREE_BYTES",
        "MAX_ENTRIES",
        "MAX_DEPTH",
        "os.O_NOFOLLOW",
        "os.O_DIRECTORY",
        "os.fstat",
        "os.scandir(directory_fd)",
        "json.dumps",
        "base64.b64encode",
        "workspace file changed during the scan",
    ] {
        assert!(
            scanner.contains(required),
            "private scanner is missing fail-closed token `{required}`"
        );
    }
}

#[cfg(unix)]
#[test]
fn private_scanner_rejects_encoded_values_and_symlinks() {
    const ENV_NAME: &str = "DBOTTER_TEST_FORBIDDEN_VALUE";
    const SECRET: &str = "j2-quote-\"-slash-\\-private";
    let scanner = repository_path("scripts/scan-private-workspace.py");

    let clean = tempfile::tempdir().expect("private scanner clean root");
    fs::set_permissions(clean.path(), fs::Permissions::from_mode(0o700))
        .expect("private scanner clean root mode");
    let clean_file = clean.path().join("manifest.json");
    fs::write(&clean_file, b"{\"schema\":1}\n").expect("private scanner clean file");
    fs::set_permissions(&clean_file, fs::Permissions::from_mode(0o600))
        .expect("private scanner clean file mode");
    let clean_status = Command::new("python3")
        .arg(&scanner)
        .arg("--root")
        .arg(clean.path())
        .arg("--forbidden-env")
        .arg(ENV_NAME)
        .env(ENV_NAME, SECRET)
        .status()
        .expect("run clean private scanner");
    assert!(clean_status.success(), "clean private tree must pass");

    let escaped = tempfile::tempdir().expect("private scanner encoded root");
    fs::set_permissions(escaped.path(), fs::Permissions::from_mode(0o700))
        .expect("private scanner encoded root mode");
    let escaped_file = escaped.path().join("shard.json");
    let encoded = serde_json::to_string(SECRET).expect("encoded scanner value");
    fs::write(&escaped_file, encoded).expect("private scanner encoded file");
    fs::set_permissions(&escaped_file, fs::Permissions::from_mode(0o600))
        .expect("private scanner encoded file mode");
    let encoded_status = Command::new("python3")
        .arg(&scanner)
        .arg("--root")
        .arg(escaped.path())
        .arg("--forbidden-env")
        .arg(ENV_NAME)
        .env(ENV_NAME, SECRET)
        .status()
        .expect("run encoded private scanner");
    assert!(
        !encoded_status.success(),
        "JSON-escaped forbidden values must fail"
    );

    let linked = tempfile::tempdir().expect("private scanner symlink root");
    fs::set_permissions(linked.path(), fs::Permissions::from_mode(0o700))
        .expect("private scanner symlink root mode");
    symlink(&clean_file, linked.path().join("unsafe-link"))
        .expect("private scanner symlink fixture");
    let linked_status = Command::new("python3")
        .arg(&scanner)
        .arg("--root")
        .arg(linked.path())
        .arg("--forbidden-env")
        .arg(ENV_NAME)
        .env(ENV_NAME, SECRET)
        .status()
        .expect("run symlink private scanner");
    assert!(
        !linked_status.success(),
        "workspace symlinks must fail closed"
    );
}
