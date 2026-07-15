use std::collections::BTreeSet;

use chrono::{DateTime, SecondsFormat, Utc};
use eframe::egui;
use egui_extras::{Column as TableColumn, TableBuilder};

use crate::export::{clipboard_scalar, tsv_field};
use crate::model::{Cell, ExportFormat, ResultId, ResultSnapshot};

use super::accessibility::{named_author_id, named_dynamic_value_author_id};

pub const RESULT_ACTION_HEIGHT: f32 = 44.0;
pub const RESULT_ROW_HEIGHT: f32 = 44.0;

#[derive(Clone, Debug, Default)]
pub(crate) struct ResultViewState {
    result_id: Option<ResultId>,
    selected_rows: BTreeSet<usize>,
    selected_cell: Option<(usize, usize)>,
}

impl ResultViewState {
    pub(crate) fn reset_for(&mut self, result_id: ResultId) {
        self.result_id = Some(result_id);
        self.selected_rows.clear();
        self.selected_cell = None;
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
    }

    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        result: &ResultSnapshot,
        export_enabled: bool,
    ) -> Option<ExportFormat> {
        self.synchronize(result);
        render_provenance(ui, result);
        for notice in &result.notices {
            ui.small(notice.message());
        }

        let mut export = None;
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

            let rows_enabled = !self.selected_rows.is_empty();
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
                && let Some(value) = copy_selected_rows(result, &self.selected_rows)
            {
                ui.ctx().copy_text(value);
            }

            let all_enabled = !result.columns.is_empty();
            let copy_all_button = ui.add_enabled(
                all_enabled,
                egui::Button::new("Copy all").min_size(egui::vec2(104.0, RESULT_ACTION_HEIGHT)),
            );
            if named_author_id(copy_all_button, "result.copy.all", "Copy all result rows").clicked()
                && let Some(value) = copy_all_rows(result)
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
                let enabled = export_enabled && (format == ExportFormat::Json || tabular);
                let button = ui.add_enabled(
                    enabled,
                    egui::Button::new(label).min_size(egui::vec2(112.0, RESULT_ACTION_HEIGHT)),
                );
                if named_author_id(button, author_id, name).clicked() {
                    export = Some(format);
                }
            }
        });

        if result.columns.is_empty() {
            return export;
        }
        render_table(ui, result, self);
        export
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

fn render_table(ui: &mut egui::Ui, result: &ResultSnapshot, state: &mut ResultViewState) {
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
                        let value = format!("{}\n{}", column.name, column.type_name);
                        let response = ui.label(&value);
                        named_dynamic_value_author_id(
                            response,
                            format!("result.column.{column_index}"),
                            format!("Result column {}", column_index + 1),
                            value,
                        );
                    });
                }
            })
            .body(|body| {
                body.rows(RESULT_ROW_HEIGHT, result.rows.len(), |mut row| {
                    let row_index = row.index();
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

fn display_cell(cell: &Cell) -> String {
    if matches!(cell, Cell::Null) {
        "NULL".to_owned()
    } else {
        clipboard_scalar(cell)
    }
}
