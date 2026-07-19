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
            // Размер до разворачивания: к нему окно вернётся, когда
            // пользователь нажмёт «Восстановить».
            //
            // `with_maximized(true)` здесь намеренно НЕ ставится, хотя просится.
            // Вместе с `with_inner_size` он лишь выставляет окну признак
            // развёрнутого, не меняя саму геометрию: `IsZoomed` отвечает
            // «развёрнуто», а окно остаётся 720×560. Хуже того, из-за этого
            // признака команда развернуть окно на первом кадре становится
            // пустой операцией — система считает, что разворачивать нечего.
            // Поэтому окно разворачивает `app.rs`, однократно и уже на ходу.
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
            // Контекст нужен приложению, чтобы поток установки мог будить
            // окно на перерисовку.
            Ok(Box::new(app::SavioApp::new(&cc.egui_ctx)))
        }),
    )
}
