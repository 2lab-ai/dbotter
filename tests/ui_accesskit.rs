#![cfg(feature = "desktop")]

use std::collections::BTreeSet;

use dbotter::ui::NativeUiHarness;
use eframe::egui::{Context, RawInput};

fn author_ids(harness: &mut NativeUiHarness) -> BTreeSet<String> {
    let context = Context::default();
    context.enable_accesskit();
    let output = context.run_ui(RawInput::default(), |ui| harness.show(ui));
    output
        .platform_output
        .accesskit_update
        .expect("the native harness must emit AccessKit")
        .nodes
        .into_iter()
        .filter_map(|(_, node)| node.author_id().map(str::to_owned))
        .collect()
}

#[test]
fn first_run_actions_have_stable_author_ids_and_mongodb_is_planned() {
    let ids = author_ids(&mut NativeUiHarness::first_run());

    for expected in [
        "connection.new",
        "connection.new.mysql",
        "connection.new.redis",
        "connection.mongodb.planned",
    ] {
        assert!(ids.contains(expected), "missing author id {expected}");
    }
}

#[test]
fn p6_inventory_exposes_the_contractual_axidentifiers() {
    let ids = author_ids(&mut NativeUiHarness::p6_inventory());

    for expected in [
        "profile.connection_id",
        "profile.host",
        "profile.redis_tls.ca_file",
        "profile.redis_tls.ca_file.pick",
        "profile.credential.session.keep",
        "profile.credential.session.replace",
        "profile.credential.session.forget",
        "editor.target",
        "editor.row_limit",
        "editor.timeout",
        "profile.delete.active_warning",
    ] {
        assert!(ids.contains(expected), "missing author id {expected}");
    }
}
