mod accessibility;
mod adapter;
mod app;
mod editor;
mod layout;
mod model;
mod mysql_explorer;
mod native_harness;
mod profile_form;
mod redis_explorer;
mod result_view;
mod runtime;
#[cfg(test)]
mod runtime_contract_tests;
mod theme;

pub use adapter::{
    CONTROL_CAPACITY, DraftTestIntent, EVENT_CAPACITY, MUTATION_CAPACITY, ServicePort, SubmitError,
    UiCommand, UiPort, WORK_CAPACITY, bounded_ports, controller_ports,
};
pub use app::DEFAULT_EXECUTE_ROW_LIMIT;
pub use editor::{
    EDITOR_CANCEL_ID, EDITOR_EXECUTE_ID, EDITOR_INPUT_ID, EDITOR_ROW_LIMIT_ID, EDITOR_TARGET_ID,
    EDITOR_TIMEOUT_ID, EditorCursor, EditorExecuteIntent, EditorIntent, EditorSurface,
    EditorValidationError, build_execute_intent, classify_execute_operation, editor_target_label,
    pending_cancel_intent,
};
pub use layout::{
    CompactFallback, FallbackSurface, LayoutMode, NativeLayout, Pane, ResolvedLayout, SplitLayout,
    WorkspaceGeometry,
};
pub use model::{
    ConfigPresentation, ConnectionFailureOutcome, ConnectionState, EditorTab, EditorTabError,
    EditorTabId, PostCloseState, ProfileSnapshot, ProfileWorkspace, ResultAreaTab, UiEvent,
    UiModel, WorkspaceKey,
};
pub use mysql_explorer::{MySqlExplorerIntent, MySqlExplorerState};
pub use native_harness::NativeUiHarness;
pub use result_view::{
    RESULT_ACTION_HEIGHT, RESULT_ROW_HEIGHT, copy_all_rows, copy_cell, copy_selected_rows,
};
pub use runtime::{RegisteredTask, RuntimeHandle, TaskScope, spawn_with_service};
pub use theme::OpenAiTheme;

use crate::error::AppError;

const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/dbotter-icon.png");

fn app_icon() -> Result<eframe::egui::IconData, AppError> {
    eframe::icon_data::from_png_bytes(APP_ICON_PNG)
        .map_err(|error| AppError::Desktop(format!("invalid embedded app icon: {error}")))
}

pub async fn run(config_path: std::path::PathBuf) -> Result<(), AppError> {
    let icon = app_icon()?;
    let (ui, service) = adapter::controller_ports();
    let shutdown = ui.shutdown_requester();
    let runtime = runtime::spawn(service, config_path);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([840.0, 560.0])
            .with_icon(icon),
        ..eframe::NativeOptions::default()
    };
    let native_result = eframe::run_native(
        "dbotter",
        options,
        Box::new(move |_| Ok(Box::new(app::DbotterApp::new(ui)))),
    )
    .map_err(|error| AppError::Desktop(error.to_string()));
    let _ = shutdown.request_shutdown();
    let runtime_result = runtime
        .wait()
        .await
        .map_err(|error| AppError::Desktop(format!("controller runtime failed: {error}")));
    native_result?;
    runtime_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_app_icon_decodes() {
        let dimensions = app_icon()
            .map(|icon| (icon.width, icon.height, icon.rgba.len()))
            .map_err(|error| error.to_string());

        assert_eq!(
            dimensions,
            Ok((1254, 1254, 1254_usize * 1254_usize * 4_usize))
        );
    }

    #[test]
    fn native_shutdown_requester_survives_ui_port_move() {
        let (ui, service) = adapter::controller_ports();
        let shutdown = ui.shutdown_requester();
        drop(ui);

        assert_eq!(shutdown.request_shutdown(), Ok(()));
        assert_eq!(
            *service.shutdown_rx.borrow(),
            Some(crate::model::OperationId(u64::MAX))
        );
    }
}
