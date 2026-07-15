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
