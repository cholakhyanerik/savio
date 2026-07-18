// В релизе окно консоли не нужно — приложение графическое.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod engine;
mod model;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([520.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Savio",
        options,
        Box::new(|_cc| Ok(Box::new(app::SavioApp::default()))),
    )
}
