mod adapter;
mod app;
mod model;
mod profile_form;
mod runtime;

pub use adapter::UiCommand;

use crate::error::AppError;

const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/dbotter-icon.png");

fn app_icon() -> Result<eframe::egui::IconData, AppError> {
    eframe::icon_data::from_png_bytes(APP_ICON_PNG)
        .map_err(|error| AppError::Desktop(format!("invalid embedded app icon: {error}")))
}

pub fn run(config_path: std::path::PathBuf) -> Result<(), AppError> {
    let icon = app_icon()?;
    let (ui, service) = adapter::bounded_ports(64);
    runtime::spawn(service, config_path);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([840.0, 560.0])
            .with_icon(icon),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "dbotter",
        options,
        Box::new(move |_| Ok(Box::new(app::DbotterApp::new(ui)))),
    )
    .map_err(|error| AppError::Desktop(error.to_string()))
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
}
