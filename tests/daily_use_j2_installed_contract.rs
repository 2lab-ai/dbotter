use std::fs;
use std::path::{Path, PathBuf};

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
        "fresh_result_after_explicit_run",
        "second_instance_read_only",
        "corrupt_profile_quarantined",
        "healthy_profile_remains_usable",
    ] {
        assert!(
            verifier.contains(required),
            "installed J2 verifier is missing exact acceptance token `{required}`"
        );
    }
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
