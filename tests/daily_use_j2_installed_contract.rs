use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

fn repository_path(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn tracked_source(path: &str) -> String {
    fs::read_to_string(repository_path(path))
        .unwrap_or_else(|_| panic!("missing canonical J2 installed dependency: {path}"))
}

fn has_bound_author_id(source: &str, id: &str) -> bool {
    let literal = format!("\"{id}\"");
    let lines = source.lines().collect::<Vec<_>>();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(&literal))
        .any(|(line, _)| {
            let start = line.saturating_sub(8);
            let end = (line + 9).min(lines.len());
            lines[start..end].join("\n").contains("author_id")
        })
}

fn source_between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source
        .find(start)
        .unwrap_or_else(|| panic!("source is missing section start `{start}`"));
    let end = source[start..]
        .find(end)
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("source is missing section end `{end}`"));
    &source[start..end]
}

#[cfg(target_os = "macos")]
struct ChildProcessGuard(std::process::Child);

#[cfg(target_os = "macos")]
impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
        "persistence_off_edit_save_disabled_execute",
        "failed_query_error_retained",
        "failed_query_error_retained_after_later_results",
        "private_store_payload_scan_clean",
        "private_store_payload_scan_pass_count",
        "MAX_PROFILE_SHARD_BYTES=33554432",
        "scripts/scan-private-workspace.py",
        "scripts/exact-executable-process-set.sh",
        "workspace-contract",
        "dbotter.workspace-contract.v1",
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
        "result.mode.value",
        "result.value.status",
        "result.value.content",
        "result.filter",
        "result.sort.0",
        "result.copy.cell",
        "result.copy.row",
        "result.error.status",
        "editor.syntax.status",
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
        "persistence_off_edit_save_disabled_execute",
        "failed_query_error_retained",
        "failed_query_error_retained_after_later_results",
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

    let syntax = source_between(
        &driver,
        "private func exerciseSyntaxAndAutocomplete",
        "private func exerciseTabBound",
    );
    assert!(
        syntax.contains("waitForStatus(") && syntax.contains("\"editor.syntax.status\""),
        "installed syntax exercise must wait for the rendered syntax status"
    );
    let result = source_between(
        &driver,
        "private func exerciseResultInspection",
        "private func inspectHistory",
    );
    for identifier in [
        "result.mode.value",
        "result.value.status",
        "result.value.content",
        "result.copy.row",
    ] {
        assert!(
            result.contains(identifier),
            "installed result exercise must observe `{identifier}`"
        );
    }
    let failed = source_between(
        &driver,
        "try setEditorSource(failedSource",
        "let exactHistorySource",
    );
    assert!(
        failed.contains("waitForStatus(") && failed.contains("\"result.error.status\""),
        "failed execution must visibly retain `result.error.status` before history work continues"
    );
    assert!(
        failed.contains("beforeFailed")
            && failed.contains("waitForResultDelta(")
            && failed.contains("failedResultTabs"),
        "failed execution must create one identifiable retained output tab"
    );
    let retained_after_later_results = source_between(
        &driver,
        "let exactHistorySource",
        "try press(try single(\"editor.tab.1\"",
    );
    assert!(
        retained_after_later_results.contains("failedResultTab")
            && retained_after_later_results.contains("try press(")
            && retained_after_later_results.contains("\"result.error.status\"")
            && retained_after_later_results.contains("failedQueryErrorRetainedAfterLaterResults"),
        "the installed journey must reselect the earlier error output after later results"
    );
}

#[test]
fn production_and_installed_driver_expose_the_remaining_j2_ax_boundaries() {
    for (path, identifier) in [
        ("src/ui/editor.rs", "editor.syntax.status"),
        ("src/ui/result_view.rs", "result.mode.value"),
        ("src/ui/result_view.rs", "result.value.status"),
        ("src/ui/result_view.rs", "result.value.content"),
        ("src/ui/result_view.rs", "result.copy.row"),
        ("src/ui/app.rs", "result.error.status"),
    ] {
        let source = tracked_source(path);
        assert!(
            has_bound_author_id(&source, identifier),
            "production renderer `{path}` must bind J2 AX id `{identifier}`"
        );
    }
}

#[test]
fn persistence_opt_out_edits_observes_save_disabled_and_executes_before_turning_back_on() {
    let driver = tracked_source("scripts/native-j2-ax-driver.swift");
    let body = source_between(
        &driver,
        "private func exercisePersistenceOptOutAndClear",
        "private func exerciseSyntaxAndAutocomplete",
    );
    let toggle = "workspace.persistence.toggle";
    let first_toggle = body
        .find(toggle)
        .expect("persistence exercise must switch Off");
    let second_toggle = body[first_toggle + toggle.len()..]
        .find(toggle)
        .map(|offset| first_toggle + toggle.len() + offset)
        .expect("persistence exercise must eventually switch back On");
    let off_status = body[first_toggle..]
        .find("$0.hasPrefix(\"Off\")")
        .map(|offset| first_toggle + offset)
        .expect("persistence exercise must visibly observe Off");
    let edit = body
        .find("j2_opt_out_private_marker")
        .expect("persistence exercise must edit a private opt-out marker");
    let save_disabled = body[edit..second_toggle]
        .find("\"editor.save\"")
        .map(|offset| edit + offset)
        .expect("persistence exercise must observe Save while Off");
    let save_observation = &body[save_disabled..second_toggle];
    assert!(
        save_observation.contains("kAXEnabledAttribute")
            && save_observation.contains("== false")
            && !body[edit..second_toggle].contains("press(try single(\"editor.save\""),
        "`editor.save` must be observed as AX disabled, never pressed, while persistence is Off"
    );
    let execute = body[save_disabled..]
        .find("\"editor.execute\"")
        .map(|offset| save_disabled + offset)
        .expect("persistence exercise must execute while Off");
    assert!(
        first_toggle < off_status
            && off_status < edit
            && edit < save_disabled
            && save_disabled < execute
            && execute < second_toggle,
        "the installed journey must edit, observe Save disabled, and execute while persistence is visibly Off"
    );
    assert!(
        body.contains("j2_clear_private_marker"),
        "the durable clear exercise needs a distinct private marker"
    );
}

#[test]
fn installed_private_scan_is_final_for_all_markers_and_reports_its_pass_count() {
    let verifier = tracked_source("scripts/verify-installed-j2.sh");
    assert_eq!(
        verifier.matches("\"$scanner\" \\").count(),
        2,
        "installed verifier must scan once after the seed save and once after every writer stops"
    );
    assert!(
        verifier.contains("private_store_payload_scan_pass_count=0"),
        "private scan pass count must start at zero"
    );
    assert_eq!(
        verifier
            .matches(
                "private_store_payload_scan_pass_count=$((private_store_payload_scan_pass_count + 1))",
            )
            .count(),
        2,
        "each successful private scan must increment the pass count exactly once"
    );
    let last_workspace_write = verifier
        .rfind("stop_pid \"$corrupt_pid\"")
        .expect("installed verifier must stop the last workspace writer");
    let final_scan = verifier
        .rfind("\"$scanner\" \\")
        .expect("installed verifier must run the private scanner");
    let receipt = verifier
        .rfind("output_parent=\"$(dirname \"$output\")\"")
        .expect("installed verifier must assemble a final receipt");
    assert!(
        last_workspace_write < final_scan && final_scan < receipt,
        "the final private scan must run after every installed app writer and before receipt assembly"
    );

    let final_scan_block = &verifier[final_scan..receipt];
    for forbidden_env in [
        "DBOTTER_MYSQL_PASSWORD",
        "DBOTTER_MYSQL_ROOT_PASSWORD",
        "DBOTTER_J2_RESULT_SENTINEL",
        "DBOTTER_J2_OPT_OUT_MARKER",
        "DBOTTER_J2_CLEAR_MARKER",
    ] {
        assert!(
            final_scan_block.contains(&format!("--forbidden-env {forbidden_env}")),
            "final private scan must forbid `{forbidden_env}`"
        );
    }
    let scan_pass = final_scan_block
        .find("private_store_payload_scan_pass_count")
        .expect("successful final scan must increment its pass count");
    assert!(
        scan_pass > final_scan_block.find('\n').unwrap_or_default(),
        "private scan pass count must be recorded only after the scanner succeeds"
    );
    for receipt_token in [
        "--argjson private_store_payload_scan_pass_count",
        "payload_scan_pass_count: $private_store_payload_scan_pass_count",
        ".private_store.payload_scan_pass_count == 2",
    ] {
        assert!(
            verifier.contains(receipt_token),
            "installed receipt must retain final scan proof `{receipt_token}`"
        );
    }
}

#[test]
fn installed_private_scan_requires_the_last_writer_to_be_confirmed_dead() {
    let verifier = tracked_source("scripts/verify-installed-j2.sh");
    let stop_pid = source_between(&verifier, "stop_pid() {", "mysql_root_exec() {");
    let kill = stop_pid
        .rfind("kill -KILL \"$pid\"")
        .expect("stop_pid must escalate to KILL");
    let final_probe = stop_pid[kill..]
        .rfind("kill -0 \"$pid\"")
        .map(|offset| kill + offset)
        .expect("stop_pid must probe after its KILL wait");
    let hard_failure = stop_pid[final_probe..]
        .find("fail ")
        .map(|offset| final_probe + offset)
        .expect("a surviving installed process must fail verification");
    assert!(
        kill < final_probe && final_probe < hard_failure,
        "the final private scan may run only after stop_pid confirms the writer is dead"
    );

    let final_stop = verifier
        .rfind("stop_pid \"$corrupt_pid\"")
        .expect("installed verifier must stop the last workspace writer");
    let final_scan = verifier
        .rfind("\"$scanner\" \\")
        .expect("installed verifier must run the final private scan");
    let between_stop_and_scan = &verifier[final_stop..final_scan];
    assert!(
        between_stop_and_scan.contains("\"$process_guard\" --assert-empty \"$executable\"")
            && between_stop_and_scan.contains("fail "),
        "the text-vnode exact installed executable process set must be empty immediately before the final scan"
    );
    assert!(
        !verifier.contains("pgrep -f"),
        "argv-regex process discovery may not guard installed writer shutdown"
    );
}

#[test]
fn installed_process_guard_uses_exact_text_vnode_identity() {
    let guard = tracked_source("scripts/exact-executable-process-set.sh");
    for required in [
        "set -euo pipefail",
        "--assert-empty",
        "lsof -a -d txt -Fp \"$executable\"",
        "exact executable process set is not empty",
    ] {
        assert!(
            guard.contains(required),
            "exact installed process guard is missing `{required}`"
        );
    }
    assert!(
        !guard.contains("pgrep"),
        "exact installed process identity may not depend on argv matching"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn installed_process_guard_rejects_a_masked_argv_writer() {
    let directory = tempfile::tempdir().expect("masked-writer tempdir");
    let source = directory.path().join("masked-writer.c");
    let executable = directory.path().join("dbotter-masked-writer");
    fs::write(
        &source,
        "#include <unistd.h>\nint main(void) { for (;;) pause(); }\n",
    )
    .expect("write masked writer source");
    let compile = Command::new("xcrun")
        .args(["clang", "-Os"])
        .arg(&source)
        .args(["-o"])
        .arg(&executable)
        .output()
        .expect("compile unique masked writer executable");
    assert!(
        compile.status.success(),
        "masked writer compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let child = Command::new("/bin/bash")
        .args(["-c", "exec -a j2-masked-writer \"$1\"", "--"])
        .arg(&executable)
        .spawn()
        .expect("launch masked-argv writer");
    let mut child = ChildProcessGuard(child);
    let pid = child.0.id();
    let mut masked = false;
    for _ in 0..50 {
        let ps = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .expect("inspect masked writer argv");
        let command = String::from_utf8_lossy(&ps.stdout);
        if command.contains("j2-masked-writer")
            && !command.contains(executable.as_os_str().to_string_lossy().as_ref())
        {
            masked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        masked,
        "writer argv was not masked before the process-set probe"
    );

    let old_probe = Command::new("pgrep")
        .args(["-f", executable.as_os_str().to_string_lossy().as_ref()])
        .output()
        .expect("run the superseded argv-regex probe");
    let old_probe_pids = String::from_utf8_lossy(&old_probe.stdout);
    assert!(
        !old_probe_pids.lines().any(|line| line == pid.to_string()),
        "the regression fixture must demonstrate that pgrep -f misses the masked writer"
    );

    let guard = Command::new(repository_path("scripts/exact-executable-process-set.sh"))
        .args(["--assert-empty"])
        .arg(&executable)
        .output()
        .expect("run exact executable process guard");
    assert!(
        !guard.status.success(),
        "a masked-argv writer must block the final installed receipt"
    );
    assert!(
        String::from_utf8_lossy(&guard.stderr)
            .contains("exact executable process set is not empty"),
        "the exact guard must fail closed for the masked writer"
    );

    child.0.kill().expect("stop masked writer");
    child.0.wait().expect("reap masked writer");
}

#[test]
fn installed_exact_executable_runs_the_bounded_workspace_contract_probe() {
    let verifier = tracked_source("scripts/verify-installed-j2.sh");
    assert!(
        verifier.contains("\"$executable\" workspace-contract --format json"),
        "the exact installed executable must emit the workspace contract JSON"
    );
    for required in [
        "dbotter.workspace-contract.v1",
        "editor_tabs_per_profile",
        "editor_tabs_total",
        "editor_source_bytes",
        "history_entries_per_profile",
        "history_entries_total",
        "history_source_bytes",
        "profile_shard_bytes",
        "workspace_store_bytes",
        "editor_tabs_per_profile_exact",
        "editor_tabs_per_profile_plus_one_rejected",
        "editor_tabs_total_exact",
        "editor_tabs_total_plus_one_rejected",
        "editor_source_bytes_exact",
        "editor_source_bytes_plus_one_rejected",
        "history_entries_per_profile_exact",
        "history_entries_per_profile_plus_one_rejected",
        "history_entries_total_exact",
        "history_entries_total_plus_one_rejected",
        "history_source_bytes_exact",
        "history_source_bytes_plus_one_rejected",
        "profile_shard_bytes_exact",
        "profile_shard_bytes_plus_one_rejected",
        "workspace_store_bytes_exact",
        "workspace_store_bytes_plus_one_rejected",
    ] {
        assert!(
            verifier.contains(required),
            "installed workspace probe must validate `{required}`"
        );
    }
    for exact_limit in [
        ".limits.editor_tabs_per_profile == 20",
        ".limits.editor_tabs_total == 100",
        ".limits.editor_source_bytes == 262144",
        ".limits.history_entries_per_profile == 2000",
        ".limits.history_entries_total == 10000",
        ".limits.history_source_bytes == 65536",
        ".limits.profile_shard_bytes == 33554432",
        ".limits.workspace_store_bytes == 134217728",
    ] {
        assert!(
            verifier.contains(exact_limit),
            "installed workspace probe must freeze `{exact_limit}`"
        );
    }
}

#[test]
fn workspace_contract_command_reports_exact_and_plus_one_boundary_probes() {
    let output = Command::new(env!("CARGO_BIN_EXE_dbotter"))
        .args(["workspace-contract", "--format", "json"])
        .output()
        .expect("run local dbotter workspace-contract probe");
    assert!(
        output.status.success(),
        "workspace-contract command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let contract: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("workspace contract must be JSON");
    assert_eq!(
        contract["schema"].as_str(),
        Some("dbotter.workspace-contract.v1")
    );
    let limits = contract["limits"]
        .as_object()
        .expect("workspace contract limits object");
    let probes = contract["probes"]
        .as_object()
        .expect("workspace contract probes object");
    for (limit, exact, expected) in [
        (
            "editor_tabs_per_profile",
            "editor_tabs_per_profile_exact",
            20_u64,
        ),
        ("editor_tabs_total", "editor_tabs_total_exact", 100),
        ("editor_source_bytes", "editor_source_bytes_exact", 262_144),
        (
            "history_entries_per_profile",
            "history_entries_per_profile_exact",
            2_000,
        ),
        (
            "history_entries_total",
            "history_entries_total_exact",
            10_000,
        ),
        ("history_source_bytes", "history_source_bytes_exact", 65_536),
        (
            "profile_shard_bytes",
            "profile_shard_bytes_exact",
            33_554_432,
        ),
        (
            "workspace_store_bytes",
            "workspace_store_bytes_exact",
            134_217_728,
        ),
    ] {
        assert_eq!(
            limits.get(limit).and_then(serde_json::Value::as_u64),
            Some(expected),
            "workspace limit `{limit}` drifted"
        );
        assert_eq!(
            probes.get(exact).and_then(serde_json::Value::as_bool),
            Some(true),
            "workspace exact-bound probe `{exact}` did not pass"
        );
        let plus_one = format!("{limit}_plus_one_rejected");
        assert_eq!(
            probes.get(&plus_one).and_then(serde_json::Value::as_bool),
            Some(true),
            "workspace plus-one probe `{plus_one}` did not reject"
        );
    }
    assert_eq!(
        limits.len(),
        8,
        "workspace contract must freeze eight limits"
    );
    assert_eq!(
        probes.len(),
        16,
        "workspace contract must freeze exact/+1 proof for every limit"
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
fn installed_j2_verifier_runs_every_ax_phase_through_source_bound_guard() {
    let verifier = tracked_source("scripts/verify-installed-j2.sh");
    let guard = tracked_source("scripts/run-source-bound-ax-driver.py");
    for required in [
        "DBOTTER_J2_AX_DRIVER_PATH",
        "driver_candidate=\"$temporary/native-j2-ax-driver\"",
        "\"$builder\" --output \"$driver_candidate\"",
        "driver_identity=\"$temporary/native-j2-ax-driver.identity.json\"",
        "\"$ax_guard\" capture",
        "run_ax_driver()",
    ] {
        assert!(
            verifier.contains(required),
            "installed verifier is missing source-bound AX guard token `{required}`"
        );
    }
    assert_eq!(
        verifier.matches("run_ax_driver \\\n  --phase").count(),
        6,
        "all six AX acceptance phases must execute through the source-bound guard"
    );
    assert!(
        !verifier.contains("\"$driver\" \\\n  --phase"),
        "the verifier must not reopen the unguarded AX driver pathname"
    );
    for required in [
        "canonical_path",
        "device",
        "inode",
        "mode",
        "uid",
        "size",
        "sha256",
        "cdhashes",
        "before execution",
        "after execution",
    ] {
        assert!(
            guard.contains(required),
            "source-bound AX guard is missing pinned identity field `{required}`"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn source_bound_ax_driver_rejects_atomic_inode_replacement_before_execution() {
    let guard = repository_path("scripts/run-source-bound-ax-driver.py");
    assert!(
        guard.is_file(),
        "source-bound AX guard must exist before the atomic replacement contract can pass"
    );

    let temporary = tempfile::tempdir().expect("create source-bound AX fixture");
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .expect("secure source-bound AX fixture");
    let candidate = temporary.path().join("candidate");
    let driver = temporary.path().join("stable");
    let replacement = temporary.path().join("replacement");
    let identity = temporary.path().join("identity.json");

    fs::copy("/usr/bin/true", &candidate).expect("copy source-bound candidate");
    fs::copy("/usr/bin/true", &driver).expect("copy source-bound stable driver");
    for path in [&candidate, &driver] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("set source-bound executable mode");
    }

    let capture = Command::new(&guard)
        .args(["capture", "--candidate"])
        .arg(&candidate)
        .arg("--driver")
        .arg(&driver)
        .arg("--identity")
        .arg(&identity)
        .output()
        .unwrap_or_else(|error| panic!("run source-bound AX capture: {error}"));
    assert!(
        capture.status.success(),
        "source-bound AX capture failed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );

    let original_inode = fs::metadata(&driver)
        .expect("stat original stable driver")
        .ino();
    fs::copy("/usr/bin/printf", &replacement).expect("copy atomic replacement");
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755))
        .expect("set replacement executable mode");
    fs::rename(&replacement, &driver).expect("atomically replace stable driver");
    let replacement_inode = fs::metadata(&driver)
        .expect("stat replacement stable driver")
        .ino();
    assert_ne!(
        original_inode, replacement_inode,
        "fixture must replace the stable path with a distinct inode"
    );

    let marker = "REPLACEMENT_EXECUTED";
    let run = Command::new(&guard)
        .args(["run", "--candidate"])
        .arg(&candidate)
        .arg("--driver")
        .arg(&driver)
        .arg("--identity")
        .arg(&identity)
        .arg("--")
        .arg(marker)
        .output()
        .expect("run source-bound AX replacement probe");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        !run.status.success(),
        "atomically replaced stable driver must fail closed"
    );
    assert!(
        !stdout.contains(marker),
        "atomically replaced executable ran before the identity guard rejected it"
    );
    assert!(
        stderr.contains("source-bound AX driver identity changed before execution"),
        "replacement failure must name the safe identity boundary: {stderr}"
    );
    assert!(
        !stderr.contains(&*temporary.path().to_string_lossy()),
        "identity failure must not disclose fixture paths"
    );
}

#[test]
fn preview_source_verification_tracks_the_j2_installed_dependencies() {
    let hermetic = tracked_source("scripts/verify-hermetic.sh");
    for dependency in [
        "tests/daily_use_j2_installed_contract.rs",
        "scripts/verify-installed-j2.sh",
        "scripts/native-j2-ax-driver.swift",
        "scripts/build-native-j2-ax-driver.sh",
        "scripts/run-source-bound-ax-driver.py",
        "scripts/exact-executable-process-set.sh",
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
        "scripts/run-source-bound-ax-driver.py",
        "scripts/exact-executable-process-set.sh",
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
        "type: tmpfs",
        "target: /var/lib/mysql",
        "/var/lib/mysql",
    ] {
        assert!(
            compose.contains(required),
            "installed fixture is missing isolation token `{required}`"
        );
    }
    assert!(
        !compose.contains("MYSQL_PWD:"),
        "installed fixture must not leak an application client password into MySQL entrypoint initialization"
    );

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
