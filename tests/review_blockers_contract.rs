use std::fs;
use std::path::PathBuf;

#[test]
fn p1_review_blocker_api_shapes_are_closed() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let service = read(&root, "src/service.rs");
    let secrets = read(&root, "src/secrets.rs");
    let config = read(&root, "src/config.rs");
    let model = read(&root, "src/model.rs");
    let public_error = read(&root, "src/public_error.rs");
    let profile_form = read(&root, "src/ui/profile_form.rs");

    let mut violations = Vec::new();

    let draft_request = section(&service, "pub struct TestDraftRequest", "impl fmt::Debug");
    if draft_request.contains("existing_profile_id") {
        violations.push("TestDraftRequest still carries existing_profile_id");
    }
    if draft_request.contains("session_intent") {
        violations.push("TestDraftRequest still resolves SessionCredentialIntent internally");
    }
    let draft_test = section(
        &service,
        "pub async fn test_draft_connection",
        "pub async fn check",
    );
    if draft_test.contains("resolve_draft_secret") || draft_test.contains("session_secrets") {
        violations.push("test_draft_connection still reaches the session-secret store");
    }
    if secrets.contains("pub fn get(&self, profile_id") {
        violations.push("SessionSecretStore::get remains publicly callable");
    }

    if !config.contains("PostCommitObservation") || !config.contains("MainObservationLoad") {
        violations.push("post-rename observation has no typed injectable outcome");
    }
    if !service.contains("ConfigUncertain") || !service.contains("reload_configuration") {
        violations.push("service has no fail-closed config-uncertain/reload boundary");
    }

    if profile_form.contains("MigrationConsent::Confirmed") {
        violations.push("UI still hardcodes migration confirmation");
    }
    if profile_form.contains("draft_id: DraftId(1)") {
        violations.push("UI still reuses DraftId(1)");
    }
    if profile_form.contains("let command = match &self.mode") {
        violations.push("UI still builds a sensitive command before reserving capacity");
    }

    for required in [
        "AmbiguousSqlMode",
        "UnterminatedSqlToken",
        "MySqlPublicErrorCode",
        "RedisPublicErrorKind",
    ] {
        if !model.contains(required) {
            violations.push(required);
        }
    }
    let public_operation_error = section(
        &public_error,
        "pub struct PublicOperationError",
        "impl std::fmt::Display",
    );
    if public_operation_error.contains("operation_id") {
        violations.push("PublicOperationError still has an extra operation_id field");
    }

    let receipt = section(&model, "pub struct ExecReceipt", "}");
    if receipt.contains("QueryResult") || receipt.contains("result:") {
        violations.push("ExecReceipt still serializes user result values");
    }

    let mutations = section(&config, "pub enum ConfigMutation", "pub enum CommitState");
    if mutations.contains("\n    Update {") || mutations.contains("\n    Delete {") {
        violations.push("ConfigMutation still exposes unchecked Update/Delete variants");
    }

    assert!(
        violations.is_empty(),
        "P1 review blockers:\n- {}",
        violations.join("\n- ")
    );
}

fn read(root: &std::path::Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).expect("contract source reads")
}

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source.find(start).expect("section start exists");
    let tail = &source[start..];
    let end = tail.find(end).expect("section end exists");
    &tail[..end]
}
