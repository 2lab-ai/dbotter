use eframe::egui::{self, CornerRadius, Stroke, StrokeKind};

pub fn named_author_id(
    response: egui::Response,
    value: &'static str,
    name: &'static str,
) -> egui::Response {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_author_id(value);
        node.set_label(name);
    });
    draw_focus_ring(&response);
    response
}

pub fn named_author_id_with_label(
    response: egui::Response,
    value: &'static str,
    name: String,
) -> egui::Response {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_author_id(value);
        node.set_label(name);
    });
    draw_focus_ring(&response);
    response
}

pub fn named_dynamic_author_id(
    response: egui::Response,
    value: String,
    name: &'static str,
) -> egui::Response {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_author_id(value);
        node.set_label(name);
    });
    draw_focus_ring(&response);
    response
}

pub fn named_dynamic_value_author_id(
    response: egui::Response,
    author_id: String,
    name: String,
    value: String,
) -> egui::Response {
    response.ctx.accesskit_node_builder(response.id, |node| {
        node.set_author_id(author_id);
        node.set_label(name);
        node.set_value(value);
    });
    draw_focus_ring(&response);
    response
}

fn draw_focus_ring(response: &egui::Response) {
    if !response.has_focus() {
        return;
    }
    let painter = response.ctx.layer_painter(response.layer_id);
    painter.rect_stroke(
        response.rect.expand(4.0),
        CornerRadius::ZERO,
        Stroke::new(4.0, egui::Color32::WHITE),
        StrokeKind::Outside,
    );
    painter.rect_stroke(
        response.rect.expand(2.0),
        CornerRadius::ZERO,
        Stroke::new(2.0, egui::Color32::BLACK),
        StrokeKind::Outside,
    );
}

#[cfg(test)]
pub(crate) fn accesskit_author_node<'a>(
    update: &'a egui::accesskit::TreeUpdate,
    author_id: &str,
) -> (egui::accesskit::NodeId, &'a egui::accesskit::Node) {
    update
        .nodes
        .iter()
        .find_map(|(node_id, node)| {
            (node.author_id() == Some(author_id)).then_some((*node_id, node))
        })
        .unwrap_or_else(|| panic!("missing actual AccessKit author id {author_id}"))
}

#[cfg(test)]
pub(crate) fn assert_accesskit_value_confined(
    update: &egui::accesskit::TreeUpdate,
    author_id: &str,
    sentinel: &str,
) {
    use std::collections::HashMap;

    let (intended_id, intended) = accesskit_author_node(update, author_id);
    assert_eq!(
        intended.value(),
        Some(sentinel),
        "{author_id} must read back the exact user value"
    );

    let parents = update
        .nodes
        .iter()
        .flat_map(|(parent, node)| node.children().iter().map(|child| (*child, *parent)))
        .collect::<HashMap<_, _>>();
    let is_intended_subtree = |mut node_id| {
        loop {
            if node_id == intended_id {
                return true;
            }
            let Some(parent) = parents.get(&node_id) else {
                return false;
            };
            node_id = *parent;
        }
    };

    let mut value_nodes = 0_usize;
    for (node_id, node) in &update.nodes {
        if node.value().is_some_and(|value| value.contains(sentinel)) {
            value_nodes += 1;
            assert!(
                is_intended_subtree(*node_id),
                "{sentinel:?} escaped the {author_id} AccessKit value subtree: {node:?}"
            );
        }

        let mut without_value = node.clone();
        without_value.clear_value();
        assert!(
            !format!("{without_value:?}").contains(sentinel),
            "{sentinel:?} escaped into a non-value AccessKit property: {node:?}"
        );
    }
    assert!(value_nodes > 0, "{author_id} must expose one value subtree");
}

#[cfg(test)]
pub(crate) fn assert_accesskit_omits(update: &egui::accesskit::TreeUpdate, sentinel: &str) {
    assert!(
        !format!("{update:?}").contains(sentinel),
        "secret sentinel escaped into the actual AccessKit tree"
    );
}
