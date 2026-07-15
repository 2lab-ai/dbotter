#[path = "common/live_evidence.rs"]
mod live_evidence;

use std::fs;

use live_evidence::LiveEvidence;

const SOURCE_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn recorder_publishes_sorted_measured_evidence_without_replacement() {
    let directory = tempfile::tempdir().expect("recorder tempdir");
    let output = directory.path().join("suite.json");
    let mut evidence = LiveEvidence::new(&output, "fixture", "fixture_test", SOURCE_SHA, 123, 2)
        .expect("create recorder");
    let second = evidence.begin("fixture.second");
    evidence.pass(second);
    let first = evidence.begin("fixture.first");
    evidence.pass(first);
    evidence.measure("observed", 7).expect("measurement");
    evidence.finish().expect("finish evidence");

    let before = fs::read(&output).expect("published evidence");
    let document: serde_json::Value = serde_json::from_slice(&before).expect("evidence JSON");
    assert_eq!(document["schema"], "dbotter.live-suite-evidence.v1");
    assert_eq!(document["source"]["commit"], SOURCE_SHA);
    assert_eq!(document["source"]["run_id"], 123);
    assert_eq!(document["source"]["run_attempt"], 2);
    assert_eq!(document["cases"][0]["id"], "fixture.first");
    assert_eq!(document["cases"][1]["id"], "fixture.second");
    assert_eq!(document["cases"][0]["executed"], 1);
    assert_eq!(document["cases"][0]["passed"], 1);
    assert_eq!(document["measurements"]["observed"], 7);

    let mut stale = LiveEvidence::new(&output, "fixture", "fixture_test", SOURCE_SHA, 124, 1)
        .expect("second recorder");
    let checkpoint = stale.begin("fixture.first");
    stale.pass(checkpoint);
    stale.measure("observed", 8).expect("second measurement");
    assert!(
        stale.finish().is_err(),
        "existing evidence must not be replaced"
    );
    assert_eq!(fs::read(&output).expect("unchanged evidence"), before);
}

#[test]
fn recorder_rejects_incomplete_checkpoints_and_duplicate_measurements() {
    let directory = tempfile::tempdir().expect("recorder tempdir");
    let output = directory.path().join("suite.json");
    let mut evidence = LiveEvidence::new(&output, "fixture", "fixture_test", SOURCE_SHA, 1, 1)
        .expect("create recorder");
    let _unfinished = evidence.begin("fixture.unfinished");
    evidence.measure("observed", 1).expect("measurement");
    assert!(evidence.measure("observed", 2).is_err());
    assert!(evidence.finish().is_err());
    assert!(!output.exists());
}
