mod adapter;
mod app;
mod model;
mod profile_form;
mod runtime;

use crate::error::AppError;

pub fn run() -> Result<(), AppError> {
    let (ui, service) = adapter::bounded_ports(64);
    runtime::spawn(service);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([840.0, 560.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "dbotter",
        options,
        Box::new(move |_| Ok(Box::new(app::DbotterApp::new(ui)))),
    )
    .map_err(|error| AppError::Desktop(error.to_string()))
}
