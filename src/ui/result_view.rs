use std::collections::BTreeSet;
use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};

use crate::export::{clipboard_scalar, tsv_field};
use crate::model::{Cell, ExportFormat, OperationId, ResultId, ResultSnapshot};

use super::accessibility::{named_author_id, named_dynamic_value_author_id};

pub const RESULT_ACTION_HEIGHT: f32 = 44.0;
pub const RESULT_ROW_HEIGHT: f32 = 44.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ResultDisplayMode {
    #[default]
    Grid,
    Record,
    Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultViewIntent {
    Export(ExportFormat),
    Cancel(OperationId),
}

#[derive(Clone, Default)]
pub(crate) struct ResultViewState {
    result_id: Option<ResultId>,
    selected_rows: BTreeSet<usize>,
    selected_cell: Option<(usize, usize)>,
    needs_initial_selection: bool,
    pending_export: Option<OperationId>,
    display_mode: ResultDisplayMode,
    filter_text: String,
    sort: Option<(usize, SortDirection)>,
}

impl fmt::Debug for ResultViewState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResultViewState")
            .field("result_id", &self.result_id)
            .field("selected_rows", &self.selected_rows)
            .field("selected_cell", &self.selected_cell)
            .field("needs_initial_selection", &self.needs_initial_selection)
            .field("pending_export", &self.pending_export)
            .field("display_mode", &self.display_mode)
            .field("filter_text", &"<redacted>")
            .field("sort", &self.sort)
            .finish()
    }
}

impl ResultViewState {
    pub(crate) const fn has_pending_export(&self) -> bool {
        self.pending_export.is_some()
    }

    pub(crate) fn reset_for(&mut self, result_id: ResultId) {
        self.result_id = Some(result_id);
        self.selected_rows.clear();
        self.selected_cell = None;
        self.needs_initial_selection = true;
        self.pending_export = None;
        self.display_mode = ResultDisplayMode::Grid;
        self.filter_text.clear();
        self.sort = None;
    }

    fn synchronize(&mut self, result: &ResultSnapshot) {
        if self.result_id != Some(result.provenance.result_id) {
            self.reset_for(result.provenance.result_id);
        }
        self.selected_rows
            .retain(|row_index| *row_index < result.rows.len());
        if self.selected_cell.is_some_and(|(row_index, column_index)| {
            row_index >= result.rows.len() || column_index >= result.columns.len()
        }) {
            self.selected_cell = None;
        }
        if self.needs_initial_selection {
            if !result.rows.is_empty() && !result.columns.is_empty() {
                self.selected_rows.insert(0);
                self.selected_cell = Some((0, 0));
            }
            self.needs_initial_selection = false;
        }
    }

    pub(crate) fn begin_export(&mut self, result_id: ResultId, operation_id: OperationId) -> bool {
        if self.result_id != Some(result_id) || self.pending_export.is_some() {
            return false;
        }
        self.pending_export = Some(operation_id);
        true
    }

    pub(crate) fn finish_export(&mut self, result_id: ResultId, operation_id: OperationId) -> bool {
        if self.result_id != Some(result_id) || self.pending_export != Some(operation_id) {
            return false;
        }
        self.pending_export = None;
        true
    }

    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        result: &ResultSnapshot,
        export_enabled: bool,
    ) -> Option<ResultViewIntent> {
        self.synchronize(result);
        render_provenance(ui, result);
        for notice in &result.notices {
            ui.small(notice.message());
        }

        self.render_inspection_toolbar(ui);
        let visible_rows = self.visible_row_indices(result);
        self.reconcile_visible_selection(result, &visible_rows);
        let visible_status = format!("{} visible of {}", visible_rows.len(), result.rows.len());
        let visible = ui.small(&visible_status);
        named_dynamic_value_author_id(
            visible,
            "result.filter.status".to_owned(),
            "Visible result rows".to_owned(),
            visible_status,
        );

        let mut intent = None;
        ui.horizontal_wrapped(|ui| {
            let cell_enabled = self
                .selected_cell
                .is_some_and(|(row, column)| copy_cell(result, row, column).is_some());
            let copy_cell_button = ui.add_enabled(
                cell_enabled,
                egui::Button::new("Copy cell").min_size(egui::vec2(104.0, RESULT_ACTION_HEIGHT)),
            );
            if named_author_id(
                copy_cell_button,
                "result.copy.cell",
                "Copy selected result cell",
            )
            .clicked()
                && let Some((row, column)) = self.selected_cell
                && let Some(value) = copy_cell(result, row, column)
            {
                ui.ctx().copy_text(value);
            }

            let selected_visible_rows = visible_rows
                .iter()
                .copied()
                .filter(|row| self.selected_rows.contains(row))
                .collect::<Vec<_>>();
            let rows_enabled = !selected_visible_rows.is_empty();
            let copy_rows_button = ui.add_enabled(
                rows_enabled,
                egui::Button::new("Copy rows").min_size(egui::vec2(104.0, RESULT_ACTION_HEIGHT)),
            );
            if named_author_id(
                copy_rows_button,
                "result.copy.row",
                "Copy selected result rows",
            )
            .clicked()
                && let Some(value) = copy_rows(result, selected_visible_rows.iter().copied())
            {
                ui.ctx().copy_text(value);
            }

            let all_enabled = !result.columns.is_empty();
            let copy_all_button = ui.add_enabled(
                all_enabled,
                egui::Button::new("Copy visible").min_size(egui::vec2(112.0, RESULT_ACTION_HEIGHT)),
            );
            if named_author_id(
                copy_all_button,
                "result.copy.all",
                "Copy all visible result rows",
            )
            .clicked()
                && let Some(value) = copy_rows(result, visible_rows.iter().copied())
            {
                ui.ctx().copy_text(value);
            }

            for (format, label, author_id, name) in [
                (
                    ExportFormat::Csv,
                    "Export CSV",
                    "result.export.csv",
                    "Export result as CSV",
                ),
                (
                    ExportFormat::Tsv,
                    "Export TSV",
                    "result.export.tsv",
                    "Export result as TSV",
                ),
                (
                    ExportFormat::Json,
                    "Export JSON",
                    "result.export.json",
                    "Export result as JSON",
                ),
            ] {
                let tabular = !result.columns.is_empty();
                let enabled = export_enabled
                    && self.pending_export.is_none()
                    && (format == ExportFormat::Json || tabular);
                let button = ui.add_enabled(
                    enabled,
                    egui::Button::new(label).min_size(egui::vec2(112.0, RESULT_ACTION_HEIGHT)),
                );
                if named_author_id(button, author_id, name).clicked() {
                    intent = Some(ResultViewIntent::Export(format));
                }
            }

            if let Some(operation_id) = self.pending_export {
                let status = ui.strong("Exporting…");
                named_author_id(status, "result.export.status", "Result export in progress");
                let cancel = ui.add(
                    egui::Button::new("Cancel export")
                        .min_size(egui::vec2(112.0, RESULT_ACTION_HEIGHT)),
                );
                if named_author_id(cancel, "result.export.cancel", "Cancel result export").clicked()
                {
                    intent = Some(ResultViewIntent::Cancel(operation_id));
                }
            }
        });

        if result.columns.is_empty() {
            return intent;
        }
        match self.display_mode {
            ResultDisplayMode::Grid => render_table(ui, result, self, &visible_rows),
            ResultDisplayMode::Record => render_record(ui, result, self, &visible_rows),
            ResultDisplayMode::Value => render_value(ui, result, self, &visible_rows),
        }
        intent
    }

    fn render_inspection_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let grid = ui.add_sized(
                [72.0, RESULT_ACTION_HEIGHT],
                egui::Button::new("Grid").selected(self.display_mode == ResultDisplayMode::Grid),
            );
            let grid = named_author_id(grid, "result.mode.grid", "Show result as a grid");
            if grid.clicked() {
                self.display_mode = ResultDisplayMode::Grid;
            }
            grid.ctx.accesskit_node_builder(grid.id, |node| {
                node.set_selected(self.display_mode == ResultDisplayMode::Grid);
            });
            let record = ui.add_sized(
                [80.0, RESULT_ACTION_HEIGHT],
                egui::Button::new("Record")
                    .selected(self.display_mode == ResultDisplayMode::Record),
            );
            let record =
                named_author_id(record, "result.mode.record", "Show selected result record");
            if record.clicked() {
                self.display_mode = ResultDisplayMode::Record;
            }
            record.ctx.accesskit_node_builder(record.id, |node| {
                node.set_selected(self.display_mode == ResultDisplayMode::Record);
            });
            let value = ui.add_sized(
                [72.0, RESULT_ACTION_HEIGHT],
                egui::Button::new("Value").selected(self.display_mode == ResultDisplayMode::Value),
            );
            let value = named_author_id(
                value,
                "result.mode.value",
                "Show selected result cell value",
            );
            if value.clicked() {
                self.display_mode = ResultDisplayMode::Value;
            }
            value.ctx.accesskit_node_builder(value.id, |node| {
                node.set_selected(self.display_mode == ResultDisplayMode::Value);
            });

            let filter = ui.add_sized(
                [220.0, RESULT_ACTION_HEIGHT],
                egui::TextEdit::singleline(&mut self.filter_text)
                    .id_salt("result.filter")
                    .char_limit(256)
                    .hint_text("Filter visible values"),
            );
            named_author_id(filter, "result.filter", "Filter result values locally");
            let clear = ui.add_enabled(
                !self.filter_text.is_empty(),
                egui::Button::new("Clear filter").min_size(egui::vec2(104.0, RESULT_ACTION_HEIGHT)),
            );
            if named_author_id(clear, "result.filter.clear", "Clear local result filter").clicked()
            {
                self.filter_text.clear();
            }
        });
    }

    fn visible_row_indices(&self, result: &ResultSnapshot) -> Vec<usize> {
        let filter = self.filter_text.trim().to_lowercase();
        let mut rows = result
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row_index, row)| {
                (filter.is_empty()
                    || row
                        .iter()
                        .map(display_cell)
                        .any(|value| value.to_lowercase().contains(&filter)))
                .then_some(row_index)
            })
            .collect::<Vec<_>>();
        if let Some((column, direction)) = self.sort {
            rows.sort_by(|left, right| {
                let left_value = result.rows[*left].get(column).map(display_cell);
                let right_value = result.rows[*right].get(column).map(display_cell);
                let value_ordering = left_value.cmp(&right_value);
                let value_ordering = match direction {
                    SortDirection::Ascending => value_ordering,
                    SortDirection::Descending => value_ordering.reverse(),
                };
                value_ordering.then_with(|| left.cmp(right))
            });
        }
        rows
    }

    fn reconcile_visible_selection(&mut self, result: &ResultSnapshot, visible_rows: &[usize]) {
        if result.columns.is_empty() || visible_rows.is_empty() {
            self.selected_cell = None;
            return;
        }
        if self
            .selected_cell
            .is_none_or(|(row, _)| !visible_rows.contains(&row))
        {
            let row = visible_rows[0];
            self.selected_cell = Some((row, 0));
            self.selected_rows.insert(row);
        }
    }
}

pub fn copy_cell(result: &ResultSnapshot, row: usize, column: usize) -> Option<String> {
    result
        .rows
        .get(row)
        .and_then(|cells| cells.get(column))
        .map(clipboard_scalar)
}

pub fn copy_selected_rows(
    result: &ResultSnapshot,
    selected_rows: &BTreeSet<usize>,
) -> Option<String> {
    if selected_rows.is_empty() {
        return None;
    }
    copy_rows(result, selected_rows.iter().copied())
}

pub fn copy_all_rows(result: &ResultSnapshot) -> Option<String> {
    copy_rows(result, 0..result.rows.len())
}

fn copy_rows(
    result: &ResultSnapshot,
    row_indices: impl IntoIterator<Item = usize>,
) -> Option<String> {
    if result.columns.is_empty() {
        return None;
    }

    let mut output = String::new();
    for (column_index, column) in result.columns.iter().enumerate() {
        if column_index > 0 {
            output.push('\t');
        }
        output.push_str(&tsv_field(&column.name));
    }
    output.push('\n');

    for row_index in row_indices {
        let row = result.rows.get(row_index)?;
        if row.len() != result.columns.len() {
            return None;
        }
        for (column_index, cell) in row.iter().enumerate() {
            if column_index > 0 {
                output.push('\t');
            }
            output.push_str(&tsv_field(&clipboard_scalar(cell)));
        }
        output.push('\n');
    }
    Some(output)
}

fn render_provenance(ui: &mut egui::Ui, result: &ResultSnapshot) {
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("{} rows", result.rows.len()));
        ui.label(format!("{} affected", result.affected_rows));
        ui.label(format!("{} ms", result.provenance.duration_ms));
        ui.label(format!("Driver: {}", result.provenance.driver));
        ui.label(format!("Operation: {}", result.provenance.operation_id.0));
        ui.label(format!(
            "Completed: {}",
            completed_at_text(result.provenance.completed_at_unix_ms)
        ));
        if let Some(last_insert_id) = result.last_insert_id {
            ui.label(format!("Last insert id: {last_insert_id}"));
        }
        if result.truncated {
            ui.strong("Warning: result is truncated");
        }
    });
    let profile_value = result.provenance.profile_id.as_str().to_owned();
    let profile = ui.label(format!("Profile: {profile_value}"));
    named_dynamic_value_author_id(
        profile,
        "result.provenance.profile".to_owned(),
        "Result profile".to_owned(),
        profile_value,
    );
}

fn completed_at_text(unix_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(unix_ms)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| "Unavailable".to_owned())
}

fn render_table(
    ui: &mut egui::Ui,
    result: &ResultSnapshot,
    state: &mut ResultViewState,
    visible_rows: &[usize],
) {
    let column_count = result.columns.len();
    let table_surface = ui.vertical(|ui| {
        let mut table = TableBuilder::new(ui)
            .striped(false)
            .resizable(true)
            .column(TableColumn::auto());
        if column_count > 1 {
            table = table.columns(TableColumn::remainder(), column_count - 1);
        }
        table
            .header(RESULT_ROW_HEIGHT, |mut header| {
                for (column_index, column) in result.columns.iter().enumerate() {
                    header.col(|ui| {
                        let direction = match state.sort {
                            Some((sorted_column, SortDirection::Ascending))
                                if sorted_column == column_index =>
                            {
                                "Ascending"
                            }
                            Some((sorted_column, SortDirection::Descending))
                                if sorted_column == column_index =>
                            {
                                "Descending"
                            }
                            _ => "Unsorted",
                        };
                        let marker = match direction {
                            "Ascending" => " ↑",
                            "Descending" => " ↓",
                            _ => "",
                        };
                        let value = format!("{}{}\n{}", column.name, marker, column.type_name);
                        let response = ui.add_sized(
                            [ui.available_width(), RESULT_ROW_HEIGHT],
                            egui::Button::new(&value).frame(false),
                        );
                        let sort = named_dynamic_value_author_id(
                            response,
                            format!("result.sort.{column_index}"),
                            format!("Sort by result column {}", column.name),
                            format!("{} · {direction}", column.name),
                        );
                        if sort.clicked() {
                            match state.sort {
                                Some((sorted_column, SortDirection::Ascending))
                                    if sorted_column == column_index =>
                                {
                                    state.sort = Some((column_index, SortDirection::Descending));
                                }
                                Some((sorted_column, SortDirection::Descending))
                                    if sorted_column == column_index =>
                                {
                                    state.sort = None;
                                }
                                _ => {
                                    state.sort = Some((column_index, SortDirection::Ascending));
                                }
                            }
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(RESULT_ROW_HEIGHT, visible_rows.len(), |mut row| {
                    let row_index = visible_rows[row.index()];
                    let cells = &result.rows[row_index];
                    for column_index in 0..column_count {
                        row.col(|ui| match cells.get(column_index) {
                            Some(cell) => {
                                let value = display_cell(cell);
                                let selected =
                                    state.selected_cell == Some((row_index, column_index));
                                let response = ui.add_sized(
                                    [ui.available_width(), RESULT_ROW_HEIGHT],
                                    egui::Button::selectable(selected, &value).frame(false),
                                );
                                let response = named_dynamic_value_author_id(
                                    response,
                                    format!("result.cell.{row_index}.{column_index}"),
                                    format!(
                                        "Result row {} column {}",
                                        row_index + 1,
                                        column_index + 1
                                    ),
                                    value,
                                );
                                if response.clicked() {
                                    let additive = ui.input(|input| {
                                        input.modifiers.command
                                            || input.modifiers.ctrl
                                            || input.modifiers.shift
                                    });
                                    if !additive {
                                        state.selected_rows.clear();
                                    }
                                    if additive && state.selected_rows.contains(&row_index) {
                                        state.selected_rows.remove(&row_index);
                                    } else {
                                        state.selected_rows.insert(row_index);
                                    }
                                    state.selected_cell = Some((row_index, column_index));
                                }
                            }
                            None => {
                                ui.strong("Error: <missing>");
                            }
                        });
                    }
                });
            });
    });
    named_author_id(table_surface.response, "result.table", "Query result table");
}

fn render_record(
    ui: &mut egui::Ui,
    result: &ResultSnapshot,
    state: &mut ResultViewState,
    visible_rows: &[usize],
) {
    if visible_rows.is_empty() {
        let empty = ui.weak("No records match the local filter.");
        named_author_id(empty, "result.record.empty", "No matching result records");
        return;
    }
    let selected_row = state
        .selected_cell
        .map(|(row, _)| row)
        .filter(|row| visible_rows.contains(row))
        .unwrap_or(visible_rows[0]);
    let position = visible_rows
        .iter()
        .position(|row| *row == selected_row)
        .unwrap_or(0);
    let mut next_position = None;
    ui.horizontal_wrapped(|ui| {
        let previous = ui.add_enabled(
            position > 0,
            egui::Button::new("Previous").min_size(egui::vec2(96.0, RESULT_ACTION_HEIGHT)),
        );
        if named_author_id(
            previous,
            "result.record.previous",
            "Previous visible result record",
        )
        .clicked()
        {
            next_position = position.checked_sub(1);
        }
        let status_value = format!("Record {} of {}", position + 1, visible_rows.len());
        let status = ui.strong(&status_value);
        named_dynamic_value_author_id(
            status,
            "result.record.status".to_owned(),
            "Selected result record".to_owned(),
            status_value,
        );
        let next = ui.add_enabled(
            position + 1 < visible_rows.len(),
            egui::Button::new("Next").min_size(egui::vec2(80.0, RESULT_ACTION_HEIGHT)),
        );
        if named_author_id(next, "result.record.next", "Next visible result record").clicked() {
            next_position = Some(position + 1);
        }
    });
    if let Some(next_position) = next_position {
        let row = visible_rows[next_position];
        let column = state.selected_cell.map_or(0, |(_, column)| {
            column.min(result.columns.len().saturating_sub(1))
        });
        state.selected_rows.clear();
        state.selected_rows.insert(row);
        state.selected_cell = Some((row, column));
    }

    let selected_row = state
        .selected_cell
        .map(|(row, _)| row)
        .filter(|row| visible_rows.contains(row))
        .unwrap_or(visible_rows[0]);
    let record_surface = ui.vertical(|ui| {
        egui::Grid::new(("result.record.grid", result.provenance.result_id.0))
            .num_columns(2)
            .spacing(egui::vec2(16.0, 8.0))
            .striped(false)
            .show(ui, |ui| {
                for (column_index, column) in result.columns.iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.strong(&column.name);
                        ui.small(&column.type_name);
                    });
                    let value = result.rows[selected_row]
                        .get(column_index)
                        .map_or_else(|| "<missing>".to_owned(), display_cell);
                    let selected = state.selected_cell == Some((selected_row, column_index));
                    let field = ui.add_sized(
                        [ui.available_width().max(160.0), RESULT_ACTION_HEIGHT],
                        egui::Button::selectable(selected, &value).frame(false),
                    );
                    if named_dynamic_value_author_id(
                        field,
                        format!("result.record.field.{column_index}"),
                        format!("Record field {}", column.name),
                        value,
                    )
                    .clicked()
                    {
                        state.selected_rows.clear();
                        state.selected_rows.insert(selected_row);
                        state.selected_cell = Some((selected_row, column_index));
                    }
                    ui.end_row();
                }
            });
    });
    named_author_id(
        record_surface.response,
        "result.record",
        "Selected result record detail",
    );
}

fn render_value(
    ui: &mut egui::Ui,
    result: &ResultSnapshot,
    state: &ResultViewState,
    visible_rows: &[usize],
) {
    let selected_cell = state
        .selected_cell
        .filter(|(row, column)| {
            visible_rows.contains(row)
                && *column < result.columns.len()
                && result
                    .rows
                    .get(*row)
                    .is_some_and(|cells| *column < cells.len())
        })
        .or_else(|| visible_rows.first().copied().map(|row| (row, 0)));
    let Some((row, column)) = selected_cell else {
        let empty = ui.weak("Select a result cell to inspect its full value.");
        named_author_id(empty, "result.value.empty", "No selected result cell value");
        return;
    };
    let Some(column_metadata) = result.columns.get(column) else {
        return;
    };
    let Some(cell) = result.rows.get(row).and_then(|cells| cells.get(column)) else {
        return;
    };

    let status_value = format!(
        "Row {} · Column {} · {} · {}",
        row + 1,
        column + 1,
        column_metadata.name,
        column_metadata.type_name
    );
    let status = ui.strong(&status_value);
    named_dynamic_value_author_id(
        status,
        "result.value.status".to_owned(),
        "Selected result cell".to_owned(),
        status_value,
    );
    ui.separator();

    let value = clipboard_scalar(cell);
    let mut rendered_value = value.clone();
    let content = ui.add_sized(
        [
            ui.available_width(),
            ui.available_height().max(RESULT_ROW_HEIGHT * 3.0),
        ],
        egui::TextEdit::multiline(&mut rendered_value)
            .font(egui::TextStyle::Monospace)
            .desired_width(f32::INFINITY)
            .interactive(false),
    );
    named_dynamic_value_author_id(
        content,
        "result.value.content".to_owned(),
        "Full selected result cell value".to_owned(),
        value,
    );
}

fn display_cell(cell: &Cell) -> String {
    if matches!(cell, Cell::Null) {
        "NULL".to_owned()
    } else {
        clipboard_scalar(cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Column, DriverKind, ProfileGeneration, ProfileId, QueryResult, ResultProvenance,
        ResultRetentionPolicy,
    };

    fn local_view_result() -> ResultSnapshot {
        ResultSnapshot::retain(
            QueryResult {
                columns: vec![Column {
                    name: "value".to_owned(),
                    type_name: "TEXT".to_owned(),
                }],
                rows: ["zulu", "alpha", "alpha", "bravo"]
                    .into_iter()
                    .map(|value| vec![Cell::Text(value.to_owned())])
                    .collect(),
                affected_rows: 0,
                last_insert_id: None,
                elapsed_ms: 1,
                truncated: false,
                backend_notices_present: false,
            },
            ResultProvenance {
                result_id: ResultId(41),
                profile_id: ProfileId("local-view".to_owned()),
                profile_generation: ProfileGeneration(1),
                operation_id: OperationId(42),
                driver: DriverKind::MySql,
                completed_at_unix_ms: 0,
                duration_ms: 1,
            },
            ResultRetentionPolicy::mysql(4),
        )
    }

    #[test]
    fn filter_and_sort_are_local_stable_and_debug_redacts_filter_text() {
        let result = local_view_result();
        let mut state = ResultViewState::default();
        state.reset_for(result.provenance.result_id);
        state.filter_text = "BRAVO-sensitive-filter".to_owned();
        assert!(format!("{state:?}").contains("<redacted>"));
        assert!(!format!("{state:?}").contains("BRAVO-sensitive-filter"));

        state.filter_text = "BRAVO".to_owned();
        assert_eq!(state.visible_row_indices(&result), vec![3]);

        state.filter_text.clear();
        state.sort = Some((0, SortDirection::Ascending));
        assert_eq!(state.visible_row_indices(&result), vec![1, 2, 3, 0]);
        state.sort = Some((0, SortDirection::Descending));
        let visible = state.visible_row_indices(&result);
        assert_eq!(visible, vec![0, 3, 1, 2]);
        assert_eq!(
            copy_rows(&result, visible).as_deref(),
            Some("value\nzulu\nbravo\nalpha\nalpha\n")
        );
    }
}
