// В релизе окно консоли не нужно — приложение графическое.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod engine;
mod model;
mod theme;

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
        Box::new(|cc| {
            // Тему ставим один раз при старте: пересобирать `Style` в кадре
            // отрисовки незачем, он не меняется во время работы.
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app::SavioApp::default()))
        }),
    )
}
