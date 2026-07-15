//! Pure MySQL catalog explorer state plus its egui renderer.

use std::collections::{HashMap, HashSet};

use eframe::egui;

use crate::drivers::mysql_catalog::{CatalogRetention, bounded_select_template};
use crate::model::{
    CatalogLevel, CatalogNode, CatalogNodeIdentity, CatalogNodeKind, CatalogPage, CatalogPageToken,
    CatalogRequest, OperationId, PublicSummary,
};

const OPENAI_CANVAS: egui::Color32 = egui::Color32::WHITE;
const OPENAI_INK: egui::Color32 = egui::Color32::BLACK;
const OPENAI_INK_60: egui::Color32 = egui::Color32::from_gray(102);
const OPENAI_HAIRLINE: egui::Color32 = egui::Color32::from_gray(224);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BranchKey {
    Schemas,
    Relations(String),
    Columns(String, String),
}

impl BranchKey {
    fn for_page(page: &CatalogPage) -> Option<Self> {
        match (page.level, page.parent.as_ref()) {
            (CatalogLevel::Schemas, None) => Some(Self::Schemas),
            (CatalogLevel::Relations, Some(CatalogNodeIdentity::Schema { schema })) => {
                Some(Self::Relations(schema.clone()))
            }
            (CatalogLevel::Columns, Some(CatalogNodeIdentity::Relation { schema, relation })) => {
                Some(Self::Columns(schema.clone(), relation.clone()))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct CatalogBranch {
    nodes: Vec<CatalogNode>,
    continuation: Option<CatalogRequest>,
    truncated: bool,
    stale: bool,
}

#[derive(Debug, Clone)]
struct PendingCatalogRequest {
    request: CatalogRequest,
    append: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlExplorerIntent {
    RefreshSchemas {
        prefix: Option<String>,
    },
    LoadMore(CatalogRequest),
    LoadRelations {
        schema: String,
        prefix: Option<String>,
        token: Option<CatalogPageToken>,
    },
    LoadColumns {
        schema: String,
        relation: String,
        prefix: Option<String>,
        token: Option<CatalogPageToken>,
    },
    Retry(CatalogRequest),
    InsertTemplate(String),
}

#[derive(Debug, Default)]
pub struct MySqlExplorerState {
    prefix: String,
    branches: HashMap<BranchKey, CatalogBranch>,
    expanded_schemas: HashSet<String>,
    expanded_relations: HashSet<(String, String)>,
    pending: HashMap<OperationId, PendingCatalogRequest>,
    retry: Option<CatalogRequest>,
    last_error: Option<PublicSummary>,
    retention: CatalogRetention,
}

impl MySqlExplorerState {
    pub fn mark_submitted(&mut self, request: CatalogRequest) {
        self.pending.insert(
            request.operation_id(),
            PendingCatalogRequest {
                append: request.page_token().is_some(),
                request,
            },
        );
        self.last_error = None;
    }

    pub fn handle_loaded(&mut self, mut page: CatalogPage) {
        let pending = self.pending.remove(&page.identity.operation_id);
        if pending
            .as_ref()
            .is_some_and(|pending| pending.request.level() != page.level)
        {
            return;
        }
        let Some(key) = BranchKey::for_page(&page) else {
            return;
        };
        let append = pending.as_ref().is_some_and(|pending| pending.append);
        let continuation = pending.as_ref().and_then(|pending| {
            page.next_token
                .clone()
                .map(|token| request_with_page_token(&pending.request, token))
        });
        if !append && let Some(previous) = self.branches.get(&key) {
            self.retention.remove(&previous.nodes);
        }
        let outcome = self.retention.retain(std::mem::take(&mut page.nodes));
        let branch = self.branches.entry(key).or_default();
        if !append {
            branch.nodes.clear();
        }
        branch.nodes.extend(outcome.nodes);
        branch.truncated = page.truncated || outcome.truncated;
        branch.continuation = if branch.truncated { None } else { continuation };
        branch.stale = false;
        self.retry = None;
        self.last_error = None;
    }

    pub fn handle_failed(&mut self, request: CatalogRequest, summary: PublicSummary) {
        self.pending.remove(&request.operation_id());
        let key = branch_for_request(&request);
        if let Some(branch) = self.branches.get_mut(&key) {
            branch.stale = true;
        }
        self.retry = Some(request);
        self.last_error = Some(summary);
    }

    pub fn clear(&mut self) {
        self.branches.clear();
        self.expanded_schemas.clear();
        self.expanded_relations.clear();
        self.pending.clear();
        self.retry = None;
        self.last_error = None;
        self.retention.clear();
    }

    pub fn dismiss_error(&mut self) {
        self.last_error = None;
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn retained_counts(&self) -> crate::model::CatalogRetainedCounts {
        self.retention.counts()
    }

    pub fn retained_utf8_bytes(&self) -> usize {
        self.retention.retained_utf8_bytes()
    }

    pub fn is_stale_for(&self, request: &CatalogRequest) -> bool {
        self.branches
            .get(&branch_for_request(request))
            .is_some_and(|branch| branch.stale)
    }

    pub fn retry_request(&self) -> Option<&CatalogRequest> {
        self.retry.as_ref()
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<MySqlExplorerIntent> {
        let mut intents = Vec::new();
        ui.scope(|ui| {
            apply_openai_component_style(ui);
            egui::Frame::new()
                .fill(OPENAI_CANVAS)
                .stroke(egui::Stroke::new(1.0, OPENAI_HAIRLINE))
                .corner_radius(egui::CornerRadius::ZERO)
                .inner_margin(16)
                .show(ui, |ui| {
                    ui.set_min_width(300.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new("MySQL catalog")
                                .color(OPENAI_INK)
                                .strong()
                                .size(18.0),
                        )
                        .selectable(false),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Schemas, relations, and columns load one page at a time.",
                        )
                        .color(OPENAI_INK_60),
                    );
                    ui.add_space(12.0);

                    ui.label(
                        egui::RichText::new("Name prefix")
                            .color(OPENAI_INK)
                            .strong(),
                    );
                    let prefix_response = ui.add(
                        egui::TextEdit::singleline(&mut self.prefix)
                            .id(egui::Id::new("mysql.catalog.prefix"))
                            .hint_text("Optional literal prefix")
                            .desired_width(f32::INFINITY),
                    );
                    if prefix_response.has_focus() {
                        ui.label(
                            egui::RichText::new(
                                "Prefix applies to the next refresh or expansion. Existing Load more keeps its page prefix.",
                            )
                            .color(OPENAI_INK_60),
                        );
                    }
                    ui.add_space(8.0);

                    ui.horizontal_wrapped(|ui| {
                        if primary_button(ui, "Refresh schemas", !self.is_pending()).clicked() {
                            intents.push(MySqlExplorerIntent::RefreshSchemas {
                                prefix: normalized_prefix(&self.prefix),
                            });
                        }
                        if secondary_button(ui, "Clear catalog", !self.is_pending()).clicked() {
                            self.clear();
                        }
                        if self.is_pending() {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Loading catalog page…").color(OPENAI_INK_60),
                            );
                        }
                    });

                    if let Some(summary) = self.last_error {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Catalog request failed. {}",
                                summary.message()
                            ))
                            .color(OPENAI_INK)
                            .strong(),
                        );
                        ui.label(
                            egui::RichText::new("The previous page is retained and marked stale.")
                                .color(OPENAI_INK_60),
                        );
                    }

                    ui.add_space(12.0);
                    self.show_catalog_tree(ui, &mut intents);

                    let counts = self.retention.counts();
                    ui.add_space(12.0);
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "Retained: {} schemas · {} relations · {} columns · {} UTF-8 bytes",
                            counts.schemas,
                            counts.relations,
                            counts.columns,
                            self.retention.retained_utf8_bytes()
                        ))
                        .color(OPENAI_INK_60),
                    );
                });
        });
        intents
    }

    fn show_catalog_tree(&mut self, ui: &mut egui::Ui, intents: &mut Vec<MySqlExplorerIntent>) {
        let Some(schemas) = self.branches.get(&BranchKey::Schemas).cloned() else {
            ui.label(
                egui::RichText::new("No catalog page loaded. Refresh schemas to begin.")
                    .color(OPENAI_INK_60),
            );
            return;
        };
        if schemas.stale {
            ui.label(
                egui::RichText::new("Stale schema page")
                    .color(OPENAI_INK)
                    .strong(),
            );
        }
        if schemas.nodes.is_empty() {
            ui.label(
                egui::RichText::new("No schemas match this scope and prefix.").color(OPENAI_INK_60),
            );
        }
        for schema_node in schemas.nodes {
            let CatalogNodeIdentity::Schema { schema } = schema_node.identity else {
                continue;
            };
            let expanded = self.expanded_schemas.contains(&schema);
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(&schema_node.name)
                        .color(OPENAI_INK)
                        .strong(),
                );
                let label = if expanded {
                    "Hide relations"
                } else {
                    "Show relations"
                };
                if secondary_button(ui, label, !self.is_pending()).clicked() {
                    if expanded {
                        self.expanded_schemas.remove(&schema);
                    } else {
                        self.expanded_schemas.insert(schema.clone());
                        if !self
                            .branches
                            .contains_key(&BranchKey::Relations(schema.clone()))
                        {
                            intents.push(MySqlExplorerIntent::LoadRelations {
                                schema: schema.clone(),
                                prefix: normalized_prefix(&self.prefix),
                                token: None,
                            });
                        }
                    }
                }
                if self
                    .branches
                    .contains_key(&BranchKey::Relations(schema.clone()))
                    && secondary_button(ui, "Refresh relations", !self.is_pending()).clicked()
                {
                    intents.push(MySqlExplorerIntent::LoadRelations {
                        schema: schema.clone(),
                        prefix: normalized_prefix(&self.prefix),
                        token: None,
                    });
                }
            });
            if self.expanded_schemas.contains(&schema) {
                self.show_relations(ui, &schema, intents);
            }
        }
        if let Some(request) = schemas.continuation {
            ui.add_space(8.0);
            if secondary_button(ui, "Load more schemas", !self.is_pending()).clicked() {
                intents.push(MySqlExplorerIntent::LoadMore(request));
            }
        }
        if schemas.truncated {
            cap_recovery(ui);
        }
    }

    fn show_relations(
        &mut self,
        ui: &mut egui::Ui,
        schema: &str,
        intents: &mut Vec<MySqlExplorerIntent>,
    ) {
        let key = BranchKey::Relations(schema.to_owned());
        let Some(relations) = self.branches.get(&key).cloned() else {
            ui.indent("mysql.catalog.relations.loading", |ui| {
                ui.label(egui::RichText::new("Relations not loaded yet.").color(OPENAI_INK_60));
            });
            return;
        };
        ui.indent(("mysql.catalog.relations", schema), |ui| {
            if relations.stale {
                ui.label(
                    egui::RichText::new("Stale relation page")
                        .color(OPENAI_INK)
                        .strong(),
                );
            }
            if relations.nodes.is_empty() {
                ui.label(egui::RichText::new("No relations match.").color(OPENAI_INK_60));
            }
            for relation_node in relations.nodes {
                let CatalogNodeIdentity::Relation { relation, .. } = relation_node.identity else {
                    continue;
                };
                let relation_key = (schema.to_owned(), relation.clone());
                let expanded = self.expanded_relations.contains(&relation_key);
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let kind = match relation_node.kind {
                        CatalogNodeKind::View => "View",
                        CatalogNodeKind::Table => "Table",
                        CatalogNodeKind::Schema | CatalogNodeKind::Column => "Relation",
                    };
                    ui.label(
                        egui::RichText::new(format!("{} · {kind}", relation_node.name))
                            .color(OPENAI_INK),
                    );
                    let label = if expanded {
                        "Hide columns"
                    } else {
                        "Show columns"
                    };
                    if secondary_button(ui, label, !self.is_pending()).clicked() {
                        if expanded {
                            self.expanded_relations.remove(&relation_key);
                        } else {
                            self.expanded_relations.insert(relation_key.clone());
                            if !self.branches.contains_key(&BranchKey::Columns(
                                schema.to_owned(),
                                relation.clone(),
                            )) {
                                intents.push(MySqlExplorerIntent::LoadColumns {
                                    schema: schema.to_owned(),
                                    relation: relation.clone(),
                                    prefix: normalized_prefix(&self.prefix),
                                    token: None,
                                });
                            }
                        }
                    }
                    if self.branches.contains_key(&BranchKey::Columns(
                        relation_key.0.clone(),
                        relation.clone(),
                    )) && secondary_button(ui, "Refresh columns", !self.is_pending()).clicked()
                    {
                        intents.push(MySqlExplorerIntent::LoadColumns {
                            schema: schema.to_owned(),
                            relation: relation.clone(),
                            prefix: normalized_prefix(&self.prefix),
                            token: None,
                        });
                    }
                    if secondary_button(ui, "Insert SELECT", true).clicked() {
                        intents.push(MySqlExplorerIntent::InsertTemplate(
                            bounded_select_template(schema, &relation),
                        ));
                    }
                });
                if self.expanded_relations.contains(&relation_key) {
                    self.show_columns(ui, schema, &relation, intents);
                }
            }
            if let Some(request) = relations.continuation
                && secondary_button(ui, "Load more relations", !self.is_pending()).clicked()
            {
                intents.push(MySqlExplorerIntent::LoadMore(request));
            }
            if relations.truncated {
                cap_recovery(ui);
            }
        });
    }

    fn show_columns(
        &mut self,
        ui: &mut egui::Ui,
        schema: &str,
        relation: &str,
        intents: &mut Vec<MySqlExplorerIntent>,
    ) {
        let key = BranchKey::Columns(schema.to_owned(), relation.to_owned());
        let Some(columns) = self.branches.get(&key).cloned() else {
            return;
        };
        ui.indent(("mysql.catalog.columns", schema, relation), |ui| {
            if columns.stale {
                ui.label(
                    egui::RichText::new("Stale column page")
                        .color(OPENAI_INK)
                        .strong(),
                );
            }
            for column in columns.nodes {
                let nullability = if column.nullable == Some(true) {
                    "nullable"
                } else {
                    "required"
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(column.name)
                            .color(OPENAI_INK)
                            .monospace(),
                    );
                    ui.label(
                        egui::RichText::new(column.type_name.unwrap_or_default())
                            .color(OPENAI_INK_60)
                            .monospace(),
                    );
                    ui.label(egui::RichText::new(nullability).color(OPENAI_INK_60));
                });
            }
            if let Some(request) = columns.continuation
                && secondary_button(ui, "Load more columns", !self.is_pending()).clicked()
            {
                intents.push(MySqlExplorerIntent::LoadMore(request));
            }
            if columns.truncated {
                cap_recovery(ui);
            }
        });
    }
}

fn branch_for_request(request: &CatalogRequest) -> BranchKey {
    match request {
        CatalogRequest::Schemas { .. } => BranchKey::Schemas,
        CatalogRequest::Relations { schema, .. } => BranchKey::Relations(schema.clone()),
        CatalogRequest::Columns {
            schema, relation, ..
        } => BranchKey::Columns(schema.clone(), relation.clone()),
    }
}

fn request_with_page_token(
    request: &CatalogRequest,
    page_token: CatalogPageToken,
) -> CatalogRequest {
    match request {
        CatalogRequest::Schemas {
            identity,
            prefix,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Schemas {
            identity: identity.clone(),
            prefix: prefix.clone(),
            page_token: Some(page_token),
            page_size: *page_size,
            timeout: *timeout,
        },
        CatalogRequest::Relations {
            identity,
            schema,
            prefix,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Relations {
            identity: identity.clone(),
            schema: schema.clone(),
            prefix: prefix.clone(),
            page_token: Some(page_token),
            page_size: *page_size,
            timeout: *timeout,
        },
        CatalogRequest::Columns {
            identity,
            schema,
            relation,
            prefix,
            page_size,
            timeout,
            ..
        } => CatalogRequest::Columns {
            identity: identity.clone(),
            schema: schema.clone(),
            relation: relation.clone(),
            prefix: prefix.clone(),
            page_token: Some(page_token),
            page_size: *page_size,
            timeout: *timeout,
        },
    }
}

fn normalized_prefix(prefix: &str) -> Option<String> {
    let prefix = prefix.trim();
    (!prefix.is_empty()).then(|| prefix.to_owned())
}

fn apply_openai_component_style(ui: &mut egui::Ui) {
    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
    let visuals = ui.visuals_mut();
    visuals.dark_mode = false;
    visuals.override_text_color = Some(OPENAI_INK);
    visuals.weak_text_color = Some(OPENAI_INK_60);
    visuals.panel_fill = OPENAI_CANVAS;
    visuals.extreme_bg_color = OPENAI_CANVAS;
    visuals.text_edit_bg_color = Some(OPENAI_CANVAS);
    visuals.selection.bg_fill = OPENAI_INK;
    visuals.selection.stroke = egui::Stroke::new(1.0, OPENAI_CANVAS);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::ZERO;
        widget.bg_stroke = egui::Stroke::new(1.0, OPENAI_HAIRLINE);
    }
    visuals.widgets.active.bg_stroke = egui::Stroke::new(2.0, OPENAI_INK);
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, OPENAI_INK);
}

fn primary_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).color(OPENAI_CANVAS))
            .fill(OPENAI_INK)
            .stroke(egui::Stroke::new(1.0, OPENAI_INK))
            .corner_radius(egui::CornerRadius::ZERO)
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

fn secondary_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).color(OPENAI_INK))
            .fill(OPENAI_CANVAS)
            .stroke(egui::Stroke::new(1.0, OPENAI_INK))
            .corner_radius(egui::CornerRadius::ZERO)
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

fn cap_recovery(ui: &mut egui::Ui) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new("Catalog retention limit reached.")
            .color(OPENAI_INK)
            .strong(),
    );
    ui.label(
        egui::RichText::new("Clear catalog, enter a narrower prefix, then refresh.")
            .color(OPENAI_INK_60),
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::model::{
        CatalogRetainedCounts, OperationId, ProfileGeneration, ProfileId, RequestIdentity,
    };

    fn request(operation: u64, token: Option<CatalogPageToken>) -> CatalogRequest {
        CatalogRequest::Schemas {
            identity: RequestIdentity::new(
                ProfileId("mysql-ui".to_owned()),
                ProfileGeneration(1),
                OperationId(operation),
            ),
            prefix: None,
            page_token: token,
            page_size: 2,
            timeout: Duration::from_secs(5),
        }
    }

    fn node(name: &str) -> CatalogNode {
        CatalogNode {
            identity: CatalogNodeIdentity::Schema {
                schema: name.to_owned(),
            },
            kind: CatalogNodeKind::Schema,
            name: name.to_owned(),
            type_name: None,
            nullable: None,
            ordinal: None,
        }
    }

    fn page(operation: u64, nodes: Vec<CatalogNode>) -> CatalogPage {
        CatalogPage {
            identity: request(operation, None).identity().clone(),
            level: CatalogLevel::Schemas,
            parent: None,
            nodes,
            next_token: None,
            retained_counts: CatalogRetainedCounts::default(),
            retained_utf8_bytes: 0,
            truncated: false,
            stale: false,
            loaded_at: "2026-07-15T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn refresh_replaces_append_extends_and_clear_releases_retention() {
        let mut state = MySqlExplorerState::default();
        state.mark_submitted(request(1, None));
        state.handle_loaded(page(1, vec![node("a"), node("b")]));
        assert_eq!(state.retained_counts().schemas, 2);

        state.mark_submitted(request(2, Some(CatalogPageToken("opaque".to_owned()))));
        state.handle_loaded(page(2, vec![node("c")]));
        assert_eq!(state.retained_counts().schemas, 3);

        state.mark_submitted(request(3, None));
        state.handle_loaded(page(3, vec![node("z")]));
        assert_eq!(state.retained_counts().schemas, 1);

        state.clear();
        assert_eq!(state.retained_counts(), CatalogRetainedCounts::default());
        assert_eq!(state.retained_utf8_bytes(), 0);
    }

    #[test]
    fn permission_refresh_failure_keeps_stale_branch_and_exposes_exact_retry() {
        let mut state = MySqlExplorerState::default();
        state.mark_submitted(request(1, None));
        state.handle_loaded(page(1, vec![node("kept")]));
        let failed = request(2, None);
        state.mark_submitted(failed.clone());
        state.handle_failed(failed.clone(), PublicSummary::PermissionDenied);

        assert_eq!(state.retained_counts().schemas, 1);
        assert_eq!(state.retry_request(), Some(&failed));
        assert!(state.is_stale_for(&failed));
    }

    #[test]
    fn changing_filter_does_not_rewrite_existing_load_more_context() {
        let mut state = MySqlExplorerState::default();
        let initial = CatalogRequest::Schemas {
            identity: RequestIdentity::new(
                ProfileId("mysql-ui".to_owned()),
                ProfileGeneration(1),
                OperationId(10),
            ),
            prefix: Some("original_".to_owned()),
            page_token: None,
            page_size: 2,
            timeout: Duration::from_secs(5),
        };
        state.mark_submitted(initial);
        let mut loaded = page(10, vec![node("original_a"), node("original_b")]);
        loaded.next_token = Some(CatalogPageToken("opaque-service-token".to_owned()));
        state.handle_loaded(loaded);

        state.prefix = "changed_".to_owned();
        let continuation = state.branches[&BranchKey::Schemas]
            .continuation
            .as_ref()
            .expect("captured continuation");
        assert_eq!(continuation.prefix(), Some("original_"));
        assert_eq!(continuation.page_size(), 2);
        assert_eq!(
            continuation.page_token(),
            Some(&CatalogPageToken("opaque-service-token".to_owned()))
        );
        assert_eq!(
            normalized_prefix(&state.prefix),
            Some("changed_".to_owned())
        );
    }

    #[test]
    fn ordinary_explorer_text_meets_wcag_aa_contrast_on_white() {
        fn relative_luminance(channel: u8) -> f64 {
            let channel = f64::from(channel) / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        }

        fn contrast(foreground: egui::Color32, background: egui::Color32) -> f64 {
            let foreground = 0.2126 * relative_luminance(foreground.r())
                + 0.7152 * relative_luminance(foreground.g())
                + 0.0722 * relative_luminance(foreground.b());
            let background = 0.2126 * relative_luminance(background.r())
                + 0.7152 * relative_luminance(background.g())
                + 0.0722 * relative_luminance(background.b());
            let (lighter, darker) = if foreground >= background {
                (foreground, background)
            } else {
                (background, foreground)
            };
            (lighter + 0.05) / (darker + 0.05)
        }

        assert!(
            contrast(OPENAI_INK_60, OPENAI_CANVAS) >= 4.5,
            "ordinary informational text must be at least 4.5:1 on the white canvas"
        );
    }

    #[test]
    fn mysql_explorer_boundary_is_exact_openai_gray_and_keeps_focus_geometry() {
        assert_eq!(
            OPENAI_HAIRLINE,
            egui::Color32::from_gray(0x91),
            "MySQL boundaries must use exact #919191"
        );

        let context = egui::Context::default();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            apply_openai_component_style(ui);
            let visuals = ui.visuals();
            for widget in [
                &visuals.widgets.noninteractive,
                &visuals.widgets.inactive,
                &visuals.widgets.hovered,
                &visuals.widgets.active,
                &visuals.widgets.open,
            ] {
                assert_eq!(widget.corner_radius, egui::CornerRadius::ZERO);
            }
            assert!(visuals.widgets.active.bg_stroke.width >= 2.0);
            assert_eq!(visuals.widgets.active.bg_stroke.color, OPENAI_INK);
        });
    }

    #[test]
    fn actual_mysql_actionable_controls_are_at_least_44_points() {
        let context = egui::Context::default();
        let mut heights = Vec::new();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            apply_openai_component_style(ui);
            let mut prefix = String::new();
            heights.push((
                "prefix",
                ui.add(egui::TextEdit::singleline(&mut prefix))
                    .rect
                    .height(),
            ));
            heights.push(("primary", primary_button(ui, "Primary", true).rect.height()));
            heights.push((
                "secondary",
                secondary_button(ui, "Secondary", true).rect.height(),
            ));
        });

        let undersized = heights
            .into_iter()
            .filter(|(_, height)| *height < 44.0)
            .map(|(control, height)| format!("{control}={height}pt"))
            .collect::<Vec<_>>();
        assert!(
            undersized.is_empty(),
            "MySQL actionable controls must be at least 44pt: {}",
            undersized.join(", ")
        );
    }

    #[test]
    fn mysql_production_has_no_legacy_gray224_or_32_point_control_path() {
        let source = include_str!("mysql_explorer.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .expect("unit-test boundary")
            .0;
        assert!(!production.contains("from_gray(224)"));
        assert!(!production.contains("32.0"));
    }
}
