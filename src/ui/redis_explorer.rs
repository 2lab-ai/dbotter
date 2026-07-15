//! Redis keyspace explorer state and component-local OpenAI visual treatment.
//!
//! Raw key bytes remain the selection identity. Lossy display, hex and base64
//! are presentation only and never flow back into an inspect request.

use eframe::egui;

use crate::drivers::redis_browser::RedisScanAccumulator;
use crate::model::{
    Cell, MAX_REDIS_FILTER_BYTES, OperationId, ProfileGeneration, ProfileId, RedisKeyFilter,
    RedisKeyId, RedisTtl, RedisValuePreview,
};
use crate::public_error::PublicOperationError;

use super::accessibility::named_dynamic_value_author_id;
use super::model::UiEvent;

const BLACK: egui::Color32 = egui::Color32::BLACK;
const WHITE: egui::Color32 = egui::Color32::WHITE;
const MUTED: egui::Color32 = egui::Color32::from_gray(88);
const BORDER: egui::Stroke = egui::Stroke {
    width: 1.0,
    color: BLACK,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FilterMode {
    #[default]
    LiteralPrefix,
    Glob,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RedisExplorerIntent {
    Scan {
        filter: RedisKeyFilter,
        cursor: u64,
        restart: bool,
    },
    Inspect {
        key: RedisKeyId,
    },
    Cancel {
        operation_id: OperationId,
    },
}

#[derive(Clone)]
struct PendingScan {
    operation_id: OperationId,
    filter: RedisKeyFilter,
    cursor: u64,
    restart: bool,
    cancel_requested: bool,
}

#[derive(Clone)]
struct PendingInspect {
    operation_id: OperationId,
    key: RedisKeyId,
    cancel_requested: bool,
}

pub(super) struct RedisExplorer {
    profile: Option<(ProfileId, ProfileGeneration)>,
    filter_text: String,
    filter_mode: FilterMode,
    scan: RedisScanAccumulator,
    pending_scan: Option<PendingScan>,
    scan_error: Option<PublicOperationError>,
    selected_key: Option<RedisKeyId>,
    preview: Option<RedisValuePreview>,
    pending_inspect: Option<PendingInspect>,
    inspect_error: Option<PublicOperationError>,
    inspect_stale: bool,
    submit_error: Option<String>,
}

impl Default for RedisExplorer {
    fn default() -> Self {
        Self {
            profile: None,
            filter_text: String::new(),
            filter_mode: FilterMode::LiteralPrefix,
            scan: RedisScanAccumulator::new(RedisKeyFilter::LiteralPrefix(String::new())),
            pending_scan: None,
            scan_error: None,
            selected_key: None,
            preview: None,
            pending_inspect: None,
            inspect_error: None,
            inspect_stale: false,
            submit_error: None,
        }
    }
}

impl RedisExplorer {
    pub fn set_profile(&mut self, profile: Option<(ProfileId, ProfileGeneration)>) {
        if self.profile == profile {
            return;
        }
        *self = Self {
            profile,
            ..Self::default()
        };
    }

    pub fn begin_scan(
        &mut self,
        operation_id: OperationId,
        filter: RedisKeyFilter,
        cursor: u64,
        restart: bool,
    ) {
        self.pending_scan = Some(PendingScan {
            operation_id,
            filter,
            cursor,
            restart,
            cancel_requested: false,
        });
        self.submit_error = None;
    }

    pub fn begin_inspect(&mut self, operation_id: OperationId, key: RedisKeyId) {
        self.pending_inspect = Some(PendingInspect {
            operation_id,
            key,
            cancel_requested: false,
        });
        self.submit_error = None;
    }

    pub fn cancel_submitted(&mut self, operation_id: OperationId) {
        if let Some(pending) = self.pending_scan.as_mut()
            && pending.operation_id == operation_id
        {
            pending.cancel_requested = true;
        }
        if let Some(pending) = self.pending_inspect.as_mut()
            && pending.operation_id == operation_id
        {
            pending.cancel_requested = true;
        }
        self.submit_error = None;
    }

    pub fn submission_failed(&mut self, message: impl Into<String>) {
        self.submit_error = Some(message.into());
    }

    pub fn dismiss_errors(&mut self) {
        self.scan_error = None;
        self.inspect_error = None;
        self.submit_error = None;
    }

    pub fn handle_event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::RedisKeysLoaded { page, .. }
                if self.matches_profile(
                    &page.identity.profile_id,
                    page.identity.profile_generation,
                ) && self
                    .pending_scan
                    .as_ref()
                    .is_some_and(|pending| pending.operation_id == page.identity.operation_id) =>
            {
                let Some(pending) = self.pending_scan.take() else {
                    return;
                };
                if pending.restart {
                    self.scan.restart(pending.filter);
                    self.selected_key = None;
                    self.preview = None;
                    self.pending_inspect = None;
                    self.inspect_error = None;
                    self.inspect_stale = false;
                }
                self.scan.apply_page(page.clone());
                self.scan_error = None;
                self.submit_error = None;
            }
            UiEvent::RedisKeysFailed { request, error, .. }
                if self.matches_profile(request.profile_id(), request.profile_generation())
                    && self.pending_scan.as_ref().is_some_and(|pending| {
                        pending.operation_id == request.operation_id()
                            && pending.cursor == request.cursor
                    }) =>
            {
                let Some(_pending) = self.pending_scan.take() else {
                    return;
                };
                self.scan.mark_stale();
                self.scan_error = Some(error.clone());
            }
            UiEvent::RedisKeyInspected { preview, .. }
                if self.matches_profile(
                    &preview.identity.profile_id,
                    preview.identity.profile_generation,
                ) && self.pending_inspect.as_ref().is_some_and(|pending| {
                    pending.operation_id == preview.identity.operation_id
                        && pending.key == preview.key.id
                }) =>
            {
                self.pending_inspect = None;
                self.selected_key = Some(preview.key.id.clone());
                self.preview = Some(preview.clone());
                self.inspect_error = None;
                self.inspect_stale = preview.stale;
                self.submit_error = None;
            }
            UiEvent::RedisKeyInspectFailed { request, error, .. }
                if self.matches_profile(request.profile_id(), request.profile_generation())
                    && self.pending_inspect.as_ref().is_some_and(|pending| {
                        pending.operation_id == request.operation_id() && pending.key == request.key
                    }) =>
            {
                self.pending_inspect = None;
                self.inspect_error = Some(error.clone());
                self.inspect_stale = self.preview.is_some();
            }
            UiEvent::ConfigUncertain { .. } => {
                self.pending_scan = None;
                self.pending_inspect = None;
                self.scan.mark_stale();
                self.inspect_stale = self.preview.is_some();
                self.submit_error = Some("Reload profiles before browsing Redis.".to_owned());
            }
            UiEvent::RuntimeShutdown { .. } => {
                self.pending_scan = None;
                self.pending_inspect = None;
            }
            _ => {}
        }
    }

    fn matches_profile(&self, profile_id: &ProfileId, generation: ProfileGeneration) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|(active_id, active_generation)| {
                active_id == profile_id && *active_generation == generation
            })
    }

    #[cfg(test)]
    pub(super) fn test_retained_raw_keys(&self) -> Vec<Vec<u8>> {
        self.scan
            .keys()
            .iter()
            .map(|entry| entry.id.as_bytes().to_vec())
            .collect()
    }

    fn current_filter(&self) -> RedisKeyFilter {
        match self.filter_mode {
            FilterMode::LiteralPrefix => RedisKeyFilter::LiteralPrefix(self.filter_text.clone()),
            FilterMode::Glob => RedisKeyFilter::Glob(self.filter_text.clone()),
        }
    }

    fn filter_error(&self) -> Option<&'static str> {
        (self.filter_text.len() > MAX_REDIS_FILTER_BYTES)
            .then_some("Filter error: use at most 512 UTF-8 bytes.")
    }

    fn page_filter_matches_draft(&self) -> bool {
        self.scan.filter() == &self.current_filter()
    }

    fn load_more_intent(&self) -> Option<RedisExplorerIntent> {
        (self.pending_scan.is_none()
            && !self.scan.is_complete()
            && !self.scan.keys().is_empty()
            && self.page_filter_matches_draft())
        .then(|| RedisExplorerIntent::Scan {
            filter: self.scan.filter().clone(),
            cursor: self.scan.next_cursor(),
            restart: false,
        })
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        actions_enabled: bool,
    ) -> Option<RedisExplorerIntent> {
        openai_scope(ui, |ui| {
            let mut intent = None;
            egui::Frame::new()
                .fill(WHITE)
                .stroke(BORDER)
                .corner_radius(egui::CornerRadius::ZERO)
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Redis Explorer");
                        ui.label(
                            egui::RichText::new("SCAN · weak consistency")
                                .small()
                                .color(MUTED),
                        );
                    });
                    ui.label(
                        egui::RichText::new(
                            "Browse raw key identities without blocking the server. Results may change between pages.",
                        )
                        .color(MUTED),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Allocation disclosure: the driver may materialize one whole Redis response frame before retained preview caps apply.",
                        )
                        .small()
                        .color(MUTED),
                    );
                    ui.add_space(8.0);

                    ui.horizontal_wrapped(|ui| {
                        egui::ComboBox::from_id_salt("redis.filter_mode")
                            .selected_text(match self.filter_mode {
                                FilterMode::LiteralPrefix => "Literal prefix",
                                FilterMode::Glob => "Glob",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.filter_mode,
                                    FilterMode::LiteralPrefix,
                                    "Literal prefix",
                                );
                                ui.selectable_value(
                                    &mut self.filter_mode,
                                    FilterMode::Glob,
                                    "Glob",
                                );
                            });
                        let filter_width = (ui.available_width() - 96.0).max(160.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.filter_text)
                                .id_source("redis.filter")
                                .hint_text("Filter keys")
                                .desired_width(filter_width),
                        );
                        let refresh_enabled = actions_enabled
                            && self.pending_scan.is_none()
                            && self.filter_error().is_none();
                        if ui
                            .push_id("redis.refresh", |ui| {
                                primary_button(ui, "Refresh", refresh_enabled)
                            })
                            .inner
                            .clicked()
                        {
                            intent = Some(RedisExplorerIntent::Scan {
                                filter: self.current_filter(),
                                cursor: 0,
                                restart: true,
                            });
                        }
                    });
                    if let Some(error) = self.filter_error() {
                        ui.label(egui::RichText::new(error).strong());
                    } else {
                        ui.label(
                            egui::RichText::new(match self.filter_mode {
                                FilterMode::LiteralPrefix => {
                                    "Literal mode escapes Redis glob metacharacters and appends *."
                                }
                                FilterMode::Glob => "Glob mode sends the pattern unchanged.",
                            })
                            .small()
                            .color(MUTED),
                        );
                    }
                    if let Some(message) = &self.submit_error {
                        ui.label(egui::RichText::new(format!("Submission error: {message}")).strong());
                    }

                    ui.add_space(8.0);
                    render_scan_status(ui, self, &mut intent);
                    ui.add_space(8.0);

                    if ui.available_width() >= 720.0 {
                        ui.columns(2, |columns| {
                            render_keys(&mut columns[0], self, actions_enabled, &mut intent);
                            render_preview(&mut columns[1], self, actions_enabled, &mut intent);
                        });
                    } else {
                        render_keys(ui, self, actions_enabled, &mut intent);
                        ui.add_space(16.0);
                        render_preview(ui, self, actions_enabled, &mut intent);
                    }
                });
            intent
        })
    }
}

fn render_scan_status(
    ui: &mut egui::Ui,
    explorer: &mut RedisExplorer,
    intent: &mut Option<RedisExplorerIntent>,
) {
    ui.horizontal_wrapped(|ui| {
        if let Some(pending) = explorer.pending_scan.as_ref() {
            ui.spinner();
            ui.label(if pending.cancel_requested {
                format!(
                    "Status: cancelling SCAN operation {}…",
                    pending.operation_id.0
                )
            } else {
                format!(
                    "Status: scanning keys · operation {}",
                    pending.operation_id.0
                )
            });
            if !pending.cancel_requested
                && ui
                    .push_id("redis.scan.cancel", |ui| {
                        secondary_button(ui, "Cancel", true)
                    })
                    .inner
                    .clicked()
            {
                *intent = Some(RedisExplorerIntent::Cancel {
                    operation_id: pending.operation_id,
                });
            }
        } else if let Some(error) = explorer.scan_error.as_ref() {
            ui.label(
                egui::RichText::new(format!("Scan error: {}", error.summary.message())).strong(),
            );
        } else if explorer.scan.keys().is_empty() && !explorer.scan.is_complete() {
            ui.label("Status: ready to scan.");
        } else {
            ui.label(format!(
                "Status: {} retained keys · cursor {} · weak consistency{}",
                explorer.scan.keys().len(),
                explorer.scan.next_cursor(),
                if explorer.scan.stale() {
                    " · stale"
                } else {
                    ""
                }
            ));
        }
    });
    if explorer.scan.skipped_oversize() > 0 || explorer.scan.truncated() {
        ui.label(format!(
            "Retention notice: {} oversize keys skipped; capped results are not selectable.",
            explorer.scan.skipped_oversize()
        ));
    }
}

fn render_keys(
    ui: &mut egui::Ui,
    explorer: &mut RedisExplorer,
    actions_enabled: bool,
    intent: &mut Option<RedisExplorerIntent>,
) {
    ui.strong("Keys");
    ui.separator();
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt("redis.key_list")
        .max_height(240.0)
        .show(ui, |ui| {
            for (index, entry) in explorer.scan.keys().iter().enumerate() {
                let selected = explorer.selected_key.as_ref() == Some(&entry.id);
                let display = bounded_text(&entry.display, 96);
                let response = ui
                    .push_id(("redis.key", index), |ui| {
                        ui.selectable_label(selected, &display)
                    })
                    .inner;
                let response = named_dynamic_value_author_id(
                    response,
                    format!("redis.key.{index}"),
                    format!("Redis key {}", index + 1),
                    display,
                );
                response
                    .clone()
                    .on_hover_text("Select this key to inspect its bounded preview.");
                if response.clicked() {
                    clicked = Some(entry.id.clone());
                }
            }
        });
    if let Some(key) = clicked {
        if explorer.selected_key.as_ref() != Some(&key) {
            explorer.preview = None;
            explorer.inspect_error = None;
            explorer.inspect_stale = false;
        }
        explorer.selected_key = Some(key);
    }

    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        let load_more = explorer.load_more_intent();
        let can_load = actions_enabled && load_more.is_some();
        if ui
            .push_id("redis.load_more", |ui| {
                secondary_button(ui, "Load more", can_load)
            })
            .inner
            .clicked()
        {
            *intent = load_more;
        }
    });
    if !explorer.scan.keys().is_empty() && !explorer.page_filter_matches_draft() {
        ui.label(
            egui::RichText::new("Filter changed. Refresh before loading another SCAN page.")
                .small()
                .color(MUTED),
        );
    }
}

fn render_preview(
    ui: &mut egui::Ui,
    explorer: &mut RedisExplorer,
    actions_enabled: bool,
    intent: &mut Option<RedisExplorerIntent>,
) {
    ui.strong("Inspector");
    ui.separator();

    if let Some(pending) = explorer.pending_inspect.as_ref() {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            ui.label(if pending.cancel_requested {
                format!(
                    "Status: cancelling inspect operation {}…",
                    pending.operation_id.0
                )
            } else {
                format!("Status: inspecting · operation {}", pending.operation_id.0)
            });
            if !pending.cancel_requested
                && ui
                    .push_id("redis.inspect.cancel", |ui| {
                        secondary_button(ui, "Cancel", true)
                    })
                    .inner
                    .clicked()
            {
                *intent = Some(RedisExplorerIntent::Cancel {
                    operation_id: pending.operation_id,
                });
            }
        });
    }
    if let Some(error) = explorer.inspect_error.as_ref() {
        ui.label(
            egui::RichText::new(format!("Inspect error: {}", error.summary.message())).strong(),
        );
    }

    let inspect_enabled =
        actions_enabled && explorer.pending_inspect.is_none() && explorer.selected_key.is_some();
    ui.horizontal_wrapped(|ui| {
        if ui
            .push_id("redis.inspect", |ui| {
                primary_button(ui, "Inspect selected", inspect_enabled)
            })
            .inner
            .clicked()
            && let Some(key) = explorer.selected_key.clone()
        {
            *intent = Some(RedisExplorerIntent::Inspect { key });
        }
    });

    let Some(preview) = explorer.preview.as_ref() else {
        ui.add_space(8.0);
        ui.label("Select a retained key to inspect its representative value.");
        return;
    };
    ui.add_space(8.0);
    ui.label(format!(
        "Key: {} · {} bytes",
        bounded_text(&preview.key.display, 96),
        preview.key.id.as_bytes().len()
    ));
    ui.label(format!("Hex: {}", bounded_text(&preview.key.hex, 192)));
    ui.label(format!("Type: {:?}", preview.value_type));
    ui.label(format!("TTL: {}", ttl_text(preview.ttl)));
    ui.label(format!(
        "Size: {} · retained: {} items / {} bytes{}",
        preview
            .size
            .map_or_else(|| "unsupported".to_owned(), |size| size.to_string()),
        preview.retained_items,
        preview.retained_bytes,
        if explorer.inspect_stale || preview.stale {
            " · stale"
        } else {
            ""
        }
    ));
    if preview.truncated {
        ui.label("Retention notice: this is a bounded representative preview.");
    }
    for notice in &preview.notices {
        ui.label(egui::RichText::new(notice.message()).small().color(MUTED));
    }
    egui::ScrollArea::vertical()
        .id_salt("redis.preview_values")
        .max_height(180.0)
        .show(ui, |ui| {
            for (index, cell) in preview.items.iter().enumerate() {
                ui.push_id(("redis.preview", index), |ui| {
                    ui.label(bounded_text(&cell_text(cell), 2_048));
                });
            }
        });
}

fn openai_scope<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope(|ui| {
        let style = ui.style_mut();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.visuals.dark_mode = false;
        style.visuals.override_text_color = Some(BLACK);
        style.visuals.panel_fill = WHITE;
        style.visuals.window_fill = WHITE;
        style.visuals.extreme_bg_color = WHITE;
        style.visuals.selection.bg_fill = BLACK;
        style.visuals.selection.stroke = egui::Stroke::new(2.0, WHITE);
        fn square_widget(visuals: &mut egui::style::WidgetVisuals) {
            visuals.corner_radius = egui::CornerRadius::ZERO;
            visuals.bg_fill = WHITE;
            visuals.weak_bg_fill = WHITE;
            visuals.bg_stroke = BORDER;
            visuals.fg_stroke = egui::Stroke::new(1.0, BLACK);
        }
        square_widget(&mut style.visuals.widgets.noninteractive);
        square_widget(&mut style.visuals.widgets.inactive);
        square_widget(&mut style.visuals.widgets.hovered);
        square_widget(&mut style.visuals.widgets.active);
        square_widget(&mut style.visuals.widgets.open);
        style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, MUTED);
        style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(2.0, BLACK);
        style.visuals.widgets.active.bg_stroke = egui::Stroke::new(2.0, BLACK);
        add_contents(ui)
    })
    .inner
}

fn primary_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).color(WHITE))
            .fill(BLACK)
            .stroke(BORDER)
            .corner_radius(egui::CornerRadius::ZERO),
    )
}

fn secondary_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).color(BLACK))
            .fill(WHITE)
            .stroke(BORDER)
            .corner_radius(egui::CornerRadius::ZERO),
    )
}

fn bounded_text(value: &str, maximum_characters: usize) -> String {
    let mut characters = value.chars();
    let retained = characters
        .by_ref()
        .take(maximum_characters)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{retained}…")
    } else {
        retained
    }
}

fn ttl_text(ttl: RedisTtl) -> String {
    match ttl {
        RedisTtl::Missing => "missing".to_owned(),
        RedisTtl::Persistent => "persistent".to_owned(),
        RedisTtl::ExpiresIn(milliseconds) => format!("expires in {milliseconds} ms"),
    }
}

fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_owned(),
        Cell::Bool(value) => value.to_string(),
        Cell::Int(value) => value.to_string(),
        Cell::UInt(value) => value.to_string(),
        Cell::Float(value) => value.to_string(),
        Cell::Decimal(value) | Cell::Text(value) | Cell::DateTime(value) => value.clone(),
        Cell::Bytes { preview, len } => format!("base64:{preview} · {len} bytes"),
        Cell::Json(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RedisExplorer, RedisExplorerIntent};
    use crate::model::{
        Cell, OperationId, OperationKind, ProfileFieldId, ProfileGeneration, ProfileId, PublicCode,
        PublicSummary, RedisKeyEntry, RedisKeyFilter, RedisKeyId, RedisKeyInspectRequest,
        RedisKeyPage, RedisScanConsistency, RedisScanRequest, RedisTtl, RedisValuePreview,
        RedisValueType, RequestIdentity, SessionGeneration, TransientAllocationQualification,
    };
    use crate::public_error::{PublicOperationError, RecoveryAction, SafeContext};
    use crate::service::SessionDisposition;
    use crate::ui::model::{ConnectionFailureOutcome, UiEvent};

    fn identity(operation: u64) -> RequestIdentity {
        RequestIdentity::new(
            ProfileId("redis-ui".to_owned()),
            ProfileGeneration(4),
            OperationId(operation),
        )
    }

    fn page(operation: u64, cursor: u64, keys: &[&[u8]]) -> RedisKeyPage {
        let keys = keys
            .iter()
            .map(|key| RedisKeyEntry::new(RedisKeyId(key.to_vec())))
            .collect::<Vec<_>>();
        RedisKeyPage {
            identity: identity(operation),
            next_cursor: cursor,
            retained_count: keys.len(),
            retained_bytes: keys.iter().map(|key| key.id.as_bytes().len()).sum(),
            keys,
            skipped_oversize: 0,
            consistency: RedisScanConsistency::Weak,
            truncated: false,
            stale: false,
        }
    }

    fn scan_request(operation: u64, cursor: u64) -> RedisScanRequest {
        RedisScanRequest {
            identity: identity(operation),
            filter: RedisKeyFilter::LiteralPrefix("p5:".to_owned()),
            cursor,
            count_hint: 100,
            timeout: Duration::from_secs(5),
        }
    }

    fn preview(operation: u64, key: &[u8], value: &str) -> RedisValuePreview {
        RedisValuePreview {
            identity: identity(operation),
            key: RedisKeyEntry::new(RedisKeyId(key.to_vec())),
            value_type: RedisValueType::String,
            ttl: RedisTtl::Persistent,
            size: Some(value.len() as u64),
            items: vec![Cell::Text(value.to_owned())],
            retained_items: 1,
            retained_bytes: value.len(),
            truncated: false,
            stale: false,
            transient_allocation: TransientAllocationQualification::RedisWholeRespFrame,
            notices: Vec::new(),
        }
    }

    fn loaded(page: RedisKeyPage) -> UiEvent {
        UiEvent::RedisKeysLoaded {
            page,
            session_generation: SessionGeneration(9),
            session_disposition: SessionDisposition::Keep,
        }
    }

    fn inspected(preview: RedisValuePreview) -> UiEvent {
        UiEvent::RedisKeyInspected {
            preview,
            session_generation: SessionGeneration(9),
            session_disposition: SessionDisposition::Keep,
        }
    }

    fn public_error(
        kind: OperationKind,
        operation_id: OperationId,
        summary: PublicSummary,
        code: PublicCode,
    ) -> PublicOperationError {
        PublicOperationError::new_or_internal(
            kind,
            summary,
            code,
            &SafeContext::profile(ProfileId("redis-ui".to_owned()), operation_id),
        )
    }

    fn scan_failed(request: RedisScanRequest, summary: PublicSummary) -> UiEvent {
        scan_failed_with_code(request, summary, PublicCode::None)
    }

    fn scan_failed_with_code(
        request: RedisScanRequest,
        summary: PublicSummary,
        code: PublicCode,
    ) -> UiEvent {
        let operation_id = request.operation_id();
        UiEvent::RedisKeysFailed {
            request,
            error: public_error(OperationKind::BrowseRedis, operation_id, summary, code),
            session_generation: Some(SessionGeneration(9)),
            session_disposition: Some(SessionDisposition::Evict),
            connection_outcome: ConnectionFailureOutcome::Disconnected,
        }
    }

    fn inspect_failed(request: RedisKeyInspectRequest, summary: PublicSummary) -> UiEvent {
        let operation_id = request.operation_id();
        UiEvent::RedisKeyInspectFailed {
            request,
            error: public_error(
                OperationKind::InspectRedis,
                operation_id,
                summary,
                PublicCode::None,
            ),
            session_generation: Some(SessionGeneration(9)),
            session_disposition: Some(SessionDisposition::Keep),
            connection_outcome: ConnectionFailureOutcome::Preserve,
        }
    }

    fn explorer() -> RedisExplorer {
        let mut explorer = RedisExplorer::default();
        explorer.set_profile(Some((
            ProfileId("redis-ui".to_owned()),
            ProfileGeneration(4),
        )));
        explorer
    }

    #[test]
    fn scan_pages_accumulate_by_raw_identity_and_only_cursor_zero_completes() {
        let mut explorer = explorer();
        let filter = RedisKeyFilter::LiteralPrefix("p5:".to_owned());
        explorer.begin_scan(OperationId(1), filter.clone(), 0, true);
        explorer.handle_event(&loaded(page(1, 41, &[b"p5:a", b"p5:\xff"])));
        assert_eq!(explorer.scan.keys().len(), 2);
        assert!(!explorer.scan.is_complete());

        explorer.begin_scan(OperationId(2), filter, 41, false);
        explorer.handle_event(&loaded(page(2, 0, &[b"p5:a", b"p5:b"])));
        assert_eq!(explorer.scan.keys().len(), 3);
        assert!(explorer.scan.is_complete());
    }

    #[test]
    fn load_more_never_reuses_a_cursor_after_the_draft_filter_changes() {
        let mut explorer = explorer();
        explorer.filter_text = "p5:".to_owned();
        explorer.begin_scan(
            OperationId(1),
            RedisKeyFilter::LiteralPrefix("p5:".to_owned()),
            0,
            true,
        );
        explorer.handle_event(&loaded(page(1, 41, &[b"p5:a"])));
        assert!(explorer.page_filter_matches_draft());
        assert_eq!(
            explorer.load_more_intent(),
            Some(RedisExplorerIntent::Scan {
                filter: RedisKeyFilter::LiteralPrefix("p5:".to_owned()),
                cursor: 41,
                restart: false,
            })
        );

        explorer.filter_text = "other:".to_owned();
        assert!(!explorer.page_filter_matches_draft());
        assert_eq!(explorer.load_more_intent(), None);
    }

    #[test]
    fn tls_ca_and_hostname_codes_route_to_only_their_exact_profile_fields() {
        let ca_error = public_error(
            OperationKind::BrowseRedis,
            OperationId(21),
            PublicSummary::TlsVerificationFailed,
            PublicCode::RedisTlsCaUntrustedIssuer,
        );
        assert_eq!(
            ca_error.recovery.as_slice(),
            &[RecoveryAction::EditProfile(
                ProfileId("redis-ui".to_owned()),
                ProfileFieldId::RedisCaFile,
            )]
        );

        let host_error = public_error(
            OperationKind::BrowseRedis,
            OperationId(22),
            PublicSummary::TlsVerificationFailed,
            PublicCode::TlsHostnameMismatch,
        );
        assert_eq!(
            host_error.recovery.as_slice(),
            &[RecoveryAction::EditProfile(
                ProfileId("redis-ui".to_owned()),
                ProfileFieldId::Host,
            )]
        );
        assert_ne!(ca_error.code, host_error.code);
    }

    #[test]
    fn failed_refresh_preserves_stale_keys_and_public_error() {
        let mut explorer = explorer();
        explorer.begin_scan(
            OperationId(1),
            RedisKeyFilter::LiteralPrefix("p5:".to_owned()),
            0,
            true,
        );
        explorer.handle_event(&loaded(page(1, 0, &[b"p5:kept"])));
        explorer.begin_scan(
            OperationId(2),
            RedisKeyFilter::LiteralPrefix("new:".to_owned()),
            0,
            true,
        );
        let request = RedisScanRequest {
            filter: RedisKeyFilter::LiteralPrefix("new:".to_owned()),
            ..scan_request(2, 0)
        };
        explorer.handle_event(&scan_failed(request, PublicSummary::OperationTimedOut));

        assert_eq!(explorer.scan.keys()[0].id.as_bytes(), b"p5:kept");
        assert!(explorer.scan.stale());
        assert_eq!(
            explorer.scan_error.as_ref().map(|error| error.summary),
            Some(PublicSummary::OperationTimedOut)
        );
    }

    #[test]
    fn cancelled_scan_is_terminal_keeps_cached_keys_and_ignores_late_success() {
        let mut explorer = explorer();
        let filter = RedisKeyFilter::LiteralPrefix("p5:".to_owned());
        explorer.begin_scan(OperationId(1), filter.clone(), 0, true);
        explorer.handle_event(&loaded(page(1, 0, &[b"p5:cached"])));

        explorer.begin_scan(OperationId(2), filter, 0, true);
        explorer.cancel_submitted(OperationId(2));
        explorer.handle_event(&scan_failed(
            scan_request(2, 0),
            PublicSummary::OperationCancelled,
        ));

        assert!(explorer.pending_scan.is_none());
        assert_eq!(explorer.scan.keys()[0].id.as_bytes(), b"p5:cached");
        assert!(explorer.scan.stale());
        assert_eq!(
            explorer.scan_error.as_ref().map(|error| error.summary),
            Some(PublicSummary::OperationCancelled)
        );
        assert!(explorer.scan_error.is_some());

        explorer.handle_event(&loaded(page(2, 0, &[b"p5:late"])));
        assert_eq!(explorer.scan.keys().len(), 1, "late success is ignored");
    }

    #[test]
    fn inspect_events_require_exact_operation_and_raw_key_and_preserve_stale_preview() {
        let mut explorer = explorer();
        let raw = RedisKeyId(vec![b'p', b'5', b':', 0xff]);
        explorer.begin_inspect(OperationId(7), raw.clone());
        explorer.handle_event(&inspected(preview(6, raw.as_bytes(), "late")));
        assert!(explorer.preview.is_none(), "late operation is ignored");
        explorer.handle_event(&inspected(preview(7, raw.as_bytes(), "current")));
        assert!(matches!(
            &explorer.preview.as_ref().expect("preview").items[0],
            Cell::Text(value) if value == "current"
        ));

        explorer.begin_inspect(OperationId(8), raw.clone());
        explorer.handle_event(&inspect_failed(
            RedisKeyInspectRequest {
                identity: identity(8),
                key: raw.clone(),
                timeout: Duration::from_secs(5),
            },
            PublicSummary::ResourceStale,
        ));
        assert!(explorer.inspect_stale);
        assert!(explorer.inspect_error.is_some());
        assert!(explorer.preview.is_some());
    }

    #[test]
    fn profile_generation_switch_clears_raw_selection_pending_and_preview() {
        let mut explorer = explorer();
        let raw = RedisKeyId(b"p5:key".to_vec());
        explorer.selected_key = Some(raw.clone());
        explorer.preview = Some(preview(1, raw.as_bytes(), "value"));
        explorer.begin_inspect(OperationId(2), raw);

        explorer.set_profile(Some((
            ProfileId("redis-ui".to_owned()),
            ProfileGeneration(5),
        )));

        assert!(explorer.selected_key.is_none());
        assert!(explorer.preview.is_none());
        assert!(explorer.pending_inspect.is_none());
        assert!(explorer.scan.keys().is_empty());
    }

    #[test]
    fn component_source_uses_openai_local_tokens_and_stable_control_ids() {
        let source = include_str!("redis_explorer.rs");
        for required in [
            "Color32::BLACK",
            "Color32::WHITE",
            "CornerRadius::ZERO",
            "redis.filter_mode",
            "redis.filter",
            "redis.refresh",
            "redis.load_more",
            "redis.inspect",
            "Status:",
            "weak consistency",
            "Redis response frame",
        ] {
            assert!(source.contains(required), "missing UI contract: {required}");
        }
        let gradient_type = ["Grad", "ient"].concat();
        let shadow_type = ["Sha", "dow"].concat();
        assert!(!source.contains(&gradient_type));
        assert!(!source.contains(&shadow_type));
    }
}
