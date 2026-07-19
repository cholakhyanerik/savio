//! Состояние и отрисовка интерфейса.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui;

use crate::engine::{self, Handle};
use crate::model::{
    Event, Format, MediaInfo, Progress, human_bytes, human_duration, looks_like_url,
};
use crate::theme;

const LOG_LIMIT: usize = 400;

enum State {
    Idle,
    Running,
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

pub struct SavioApp {
    url: String,
    format: Format,
    out_dir: Option<PathBuf>,
    state: State,
    progress: Progress,
    stage: String,
    info: Option<MediaInfo>,
    log: Vec<String>,
    rx: Option<Receiver<Event>>,
    handle: Option<Handle>,
    /// Проверяем наличие yt-dlp один раз на старте, чтобы сразу показать
    /// внятную подсказку вместо провала при первой попытке скачать.
    setup_error: Option<String>,

    // Строки ниже пересобираются только при изменении состояния, а не в кадре.
    // `ui()` вызывается 60 раз в секунду: `format!` и `join` здесь стоили бы
    // сотен лишних аллокаций в секунду на ровном месте.
    /// Путь к папке сохранения в виде текста.
    out_dir_display: String,
    /// «Автор · длительность» — собирается один раз, когда приходят метаданные.
    meta_line: String,
    /// Строка под прогресс-баром: стадия, проценты, объём, скорость, остаток.
    progress_line: String,
    /// Путь к готовому файлу.
    done_path_display: String,
    /// Ссылка не похожа на ссылку. Только подсветка поля — кнопку не блокирует.
    url_invalid: bool,
}

impl Default for SavioApp {
    fn default() -> Self {
        let out_dir = default_download_dir();
        Self {
            url: String::new(),
            format: Format::Mp4,
            out_dir_display: display_dir(out_dir.as_deref()),
            out_dir,
            state: State::Idle,
            progress: Progress::default(),
            stage: String::new(),
            info: None,
            log: Vec::new(),
            rx: None,
            handle: None,
            setup_error: engine::discover().err(),
            meta_line: String::new(),
            progress_line: String::new(),
            done_path_display: String::new(),
            url_invalid: false,
        }
    }
}

fn default_download_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");

    let downloads = PathBuf::from(home?).join("Downloads");
    downloads.is_dir().then_some(downloads)
}

fn display_dir(dir: Option<&Path>) -> String {
    match dir {
        Some(dir) => dir.display().to_string(),
        None => "не выбрана".to_owned(),
    }
}

impl SavioApp {
    fn can_start(&self) -> bool {
        !matches!(self.state, State::Running)
            && !self.url.trim().is_empty()
            && self.out_dir.is_some()
            && self.setup_error.is_none()
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(out_dir) = self.out_dir.clone() else {
            return;
        };

        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();

        match engine::start(
            self.url.trim().to_owned(),
            self.format,
            out_dir,
            tx,
            move || notify_ctx.request_repaint(),
        ) {
            Ok(handle) => {
                self.rx = Some(rx);
                self.handle = Some(handle);
                self.state = State::Running;
                self.progress = Progress::default();
                self.info = None;
                self.stage = "Запуск…".into();
                self.log.clear();
                self.meta_line.clear();
                self.done_path_display.clear();
                self.rebuild_progress_line();
            }
            Err(err) => {
                self.setup_error = Some(err.clone());
                self.state = State::Failed(err);
            }
        }
    }

    fn cancel(&mut self) {
        if let Some(handle) = &self.handle {
            handle.cancel();
        }
        self.handle = None;
        self.rx = None;
        self.state = State::Cancelled;
        self.stage = "Отменено".into();
        self.progress_line.clear();
    }

    /// Собирает строку под прогресс-баром. Вызывается только на событиях
    /// движка, поэтому `format!` внутри безопасен — это не горячий путь.
    fn rebuild_progress_line(&mut self) {
        use std::fmt::Write as _;

        let p = self.progress;
        let line = &mut self.progress_line;
        line.clear();
        line.push_str(&self.stage);

        let sep = |line: &mut String| {
            if !line.is_empty() {
                line.push_str(" · ");
            }
        };

        if let Some(fraction) = p.fraction() {
            sep(line);
            let _ = write!(line, "{:.0}%", fraction * 100.0);
        }
        if p.total > 0 {
            sep(line);
            let _ = write!(
                line,
                "{} из {}",
                human_bytes(p.downloaded),
                human_bytes(p.total)
            );
        }
        if let Some(speed) = p.speed_bps {
            sep(line);
            let _ = write!(line, "{}/с", human_bytes(speed as u64));
        }
        if let Some(eta) = p.eta_secs {
            sep(line);
            let _ = write!(line, "осталось {}", human_duration(eta));
        }
    }

    fn rebuild_meta_line(&mut self) {
        self.meta_line.clear();
        let Some(info) = &self.info else {
            return;
        };
        if let Some(uploader) = &info.uploader {
            self.meta_line.push_str(uploader);
        }
        if let Some(secs) = info.duration_secs {
            if !self.meta_line.is_empty() {
                self.meta_line.push_str(" · ");
            }
            self.meta_line.push_str(&human_duration(secs as u64));
        }
    }

    /// Короткая подпись состояния для плашки. Строки статические —
    /// в кадре отрисовки ничего не выделяется.
    fn status(&self) -> (&'static str, egui::Color32) {
        match self.state {
            State::Idle => ("Готов к работе", theme::TEXT_SECONDARY),
            State::Running => ("Загрузка", theme::ACCENT),
            State::Done(_) => ("Готово", theme::STATE_SUCCESS),
            State::Failed(_) => ("Ошибка", theme::STATE_ERROR),
            State::Cancelled => ("Отменено", theme::TEXT_SECONDARY),
        }
    }

    /// Сначала собираем сообщения, потом применяем: иначе заимствование
    /// `self.rx` живёт во время мутации `self` и код не компилируется.
    fn drain_events(&mut self) {
        let mut events = Vec::new();
        let mut disconnected = false;

        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        // Строки пересобираем один раз после разбора пачки, а не на каждом
        // событии: прогресс приходит часто, а показать нужно только итог.
        let mut progress_dirty = false;
        let mut meta_dirty = false;

        for event in events {
            match event {
                Event::Info(info) => {
                    self.info = Some(info);
                    meta_dirty = true;
                }
                Event::Stage(stage) => {
                    self.stage = stage;
                    progress_dirty = true;
                }
                Event::Progress(p) => {
                    self.progress = p;
                    progress_dirty = true;
                }
                Event::Log(line) => {
                    self.log.push(line);
                    if self.log.len() > LOG_LIMIT {
                        self.log.drain(..self.log.len() - LOG_LIMIT);
                    }
                }
                Event::Done(path) => {
                    self.stage = "Готово".into();
                    self.done_path_display = path.display().to_string();
                    self.state = State::Done(path);
                    self.handle = None;
                    progress_dirty = true;
                }
                Event::Failed(err) => {
                    self.stage = "Ошибка".into();
                    self.state = State::Failed(err);
                    self.handle = None;
                    progress_dirty = true;
                }
            }
        }

        if progress_dirty {
            self.rebuild_progress_line();
        }
        if meta_dirty {
            self.rebuild_meta_line();
        }

        if disconnected {
            self.rx = None;
            if matches!(self.state, State::Running) {
                self.state = State::Idle;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Отрисовка
// ---------------------------------------------------------------------------

impl eframe::App for SavioApp {
    /// Фон окна до первой отрисовки — тот же, что у панели, иначе при
    /// запуске и ресайзе видна светлая вспышка.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG_ROOT.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG_ROOT))
            .show(ui, |ui| {
                self.header(ui);

                // Прокрутка нужна на минимальном размере окна: без неё
                // кнопка «Скачать» просто обрезалась бы нижней кромкой.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(20, 18))
                            .show(ui, |ui| {
                                if let Some(err) = &self.setup_error {
                                    banner(ui, err, theme::STATE_ERROR);
                                    ui.add_space(12.0);
                                }

                                self.controls_card(ui);
                                ui.add_space(16.0);
                                self.status_section(ui);
                                self.log_section(ui);
                            });
                    });
            });
    }
}

impl SavioApp {
    fn header(&self, ui: &mut egui::Ui) {
        let header = egui::Frame::new()
            .fill(theme::BG_SURFACE)
            .inner_margin(egui::Margin::symmetric(20, 14))
            .show(ui, |ui| {
                // Без этого полоса шапки сжалась бы по ширине текста
                // и не дотянулась бы до правого края окна.
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(
                        egui::RichText::new("Savio")
                            .heading()
                            .strong()
                            .color(theme::TEXT_PRIMARY),
                    );
                    // Акцентная точка — единственный «логотип», который нужен.
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 3.5, theme::ACCENT);
                    ui.label(
                        egui::RichText::new("видео и аудио по ссылке")
                            .color(theme::TEXT_SECONDARY),
                    );
                });
            });

        // Линию рисуем поверх нижней кромки шапки: отдельный виджет-разделитель
        // занял бы место в раскладке и «оторвался» бы от шапки на item_spacing.
        let rect = header.response.rect;
        ui.painter().hline(
            rect.x_range(),
            rect.max.y,
            egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
        );
    }

    fn controls_card(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(theme::BG_SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                field_label(ui, "Ссылка");
                self.url_field(ui);

                ui.add_space(14.0);
                field_label(ui, "Формат");
                self.format_selector(ui);

                ui.add_space(14.0);
                field_label(ui, "Папка сохранения");
                self.folder_row(ui);

                ui.add_space(18.0);
                self.action_button(ui);
            });
    }

    fn url_field(&mut self, ui: &mut egui::Ui) {
        let invalid = self.url_invalid;

        let response = ui
            .scope(|ui| {
                if invalid {
                    // Ошибка валидации: красная рамка в покое, при наведении
                    // и в фокусе (`selection.stroke` — это рамка фокуса).
                    let v = ui.visuals_mut();
                    let error = egui::Stroke::new(1.0, theme::STATE_ERROR);
                    v.widgets.inactive.bg_stroke = error;
                    v.widgets.hovered.bg_stroke = error;
                    v.widgets.active.bg_stroke = error;
                    v.selection.stroke = error;
                }

                ui.add_sized(
                    [ui.available_width(), theme::CONTROL_HEIGHT],
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://…")
                        .text_color(theme::TEXT_PRIMARY)
                        .margin(egui::Margin::symmetric(10, 6)),
                )
            })
            .inner;

        // Пересчитываем только при правке текста, а не каждый кадр.
        if response.changed() {
            let url = self.url.trim();
            self.url_invalid = !url.is_empty() && !looks_like_url(url);
        }

        if invalid {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Похоже, это не ссылка. Нужен адрес вида https://…")
                    .small()
                    .color(theme::STATE_WARNING),
            );
        }
    }

    fn format_selector(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(theme::BG_INPUT)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_STRONG))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS))
            .inner_margin(egui::Margin::same(3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    const GAP: f32 = 3.0;
                    ui.spacing_mut().item_spacing.x = GAP;
                    let width = (ui.available_width() - GAP) / 2.0;
                    self.segment(ui, Format::Mp4, width);
                    self.segment(ui, Format::Mp3, width);
                });
            });
    }

    /// Одна половина переключателя формата.
    ///
    /// Цвета задаём через `visuals`, а не через `Button::fill`: последний,
    /// по документации egui, отключает реакцию на наведение — кнопка
    /// выглядела бы мёртвой.
    fn segment(&mut self, ui: &mut egui::Ui, format: Format, width: f32) {
        let selected = self.format == format;

        let clicked = ui
            .scope(|ui| {
                let v = ui.visuals_mut();
                let (rest, hover, press, text) = if selected {
                    (
                        theme::ACCENT,
                        theme::ACCENT_HOVER,
                        theme::ACCENT_ACTIVE,
                        theme::TEXT_ON_ACCENT,
                    )
                } else {
                    (
                        theme::BG_INPUT,
                        theme::BG_ELEVATED,
                        theme::BG_PRESSED,
                        theme::TEXT_SECONDARY,
                    )
                };

                for (state, fill) in [
                    (&mut v.widgets.inactive, rest),
                    (&mut v.widgets.hovered, hover),
                    (&mut v.widgets.active, press),
                ] {
                    state.weak_bg_fill = fill;
                    state.bg_stroke = egui::Stroke::NONE;
                    state.fg_stroke = egui::Stroke::new(1.0, text);
                    state.corner_radius = egui::CornerRadius::same(theme::RADIUS_SMALL);
                    // Сегмент не должен «распухать» — он зажат в дорожке.
                    state.expansion = 0.0;
                }

                ui.add(
                    egui::Button::new(format.label())
                        .min_size(egui::vec2(width, theme::CONTROL_HEIGHT - 6.0)),
                )
                .clicked()
            })
            .inner;

        if clicked {
            self.format = format;
        }
    }

    fn folder_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let button = ui.add(
                egui::Button::new("Выбрать…")
                    .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
            );
            if button.clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.out_dir_display = display_dir(Some(&dir));
                self.out_dir = Some(dir);
            }

            let color = if self.out_dir.is_some() {
                theme::TEXT_SECONDARY
            } else {
                theme::STATE_WARNING
            };
            // Длинный путь обрезаем, иначе он растягивает окно.
            ui.add(
                egui::Label::new(egui::RichText::new(&self.out_dir_display).color(color))
                    .truncate(),
            )
            .on_hover_text(&self.out_dir_display);
        });
    }

    fn action_button(&mut self, ui: &mut egui::Ui) {
        if matches!(self.state, State::Running) {
            let cancel = ui.add_sized(
                [ui.available_width(), theme::CTA_HEIGHT],
                egui::Button::new("Отмена"),
            );
            if cancel.clicked() {
                self.cancel();
            }
            return;
        }

        let enabled = self.can_start();
        let clicked = ui
            .scope(|ui| {
                let v = ui.visuals_mut();
                // `ui.disable()` не переключает виджет на `noninteractive`,
                // а только глушит прозрачность. Поэтому выключенный вид
                // задаём сами: все три состояния красим приглушённым жёлтым,
                // навести на выключенную кнопку всё равно нельзя.
                let (rest, hover, press) = if enabled {
                    (theme::ACCENT, theme::ACCENT_HOVER, theme::ACCENT_ACTIVE)
                } else {
                    (
                        theme::ACCENT_DISABLED,
                        theme::ACCENT_DISABLED,
                        theme::ACCENT_DISABLED,
                    )
                };

                for (state, fill) in [
                    (&mut v.widgets.inactive, rest),
                    (&mut v.widgets.hovered, hover),
                    (&mut v.widgets.active, press),
                ] {
                    state.weak_bg_fill = fill;
                    state.bg_stroke = egui::Stroke::NONE;
                    state.fg_stroke = egui::Stroke::new(1.0, theme::TEXT_ON_ACCENT);
                    state.corner_radius = egui::CornerRadius::same(theme::RADIUS);
                }
                // Двойное ослабление не нужно: приглушённый жёлтый уже задан
                // явно, а поверх него прозрачность съела бы кнопку целиком.
                v.disabled_alpha = 1.0;

                ui.add_enabled_ui(enabled, |ui| {
                    ui.add_sized(
                        [ui.available_width(), theme::CTA_HEIGHT],
                        egui::Button::new(egui::RichText::new("Скачать").strong()),
                    )
                    .clicked()
                })
                .inner
            })
            .inner;

        if clicked {
            let ctx = ui.ctx().clone();
            self.start(&ctx);
        }
    }

    fn status_section(&mut self, ui: &mut egui::Ui) {
        let (label, color) = self.status();

        ui.horizontal(|ui| {
            status_pill(ui, label, color);

            if let Some(info) = &self.info
                && let Some(title) = &info.title
            {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(title).strong().color(theme::TEXT_PRIMARY),
                    )
                    .truncate(),
                )
                .on_hover_text(title);
            }
        });

        if !self.meta_line.is_empty() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&self.meta_line).small().color(theme::TEXT_SECONDARY));
        }

        ui.add_space(10.0);

        match &self.state {
            State::Running => {
                ui.scope(|ui| {
                    // Жёлоб бара берётся из `extreme_bg_color`.
                    ui.visuals_mut().extreme_bg_color = theme::PROGRESS_TRACK;

                    // Без явного скругления egui рисует бар «таблеткой» —
                    // ровно то, что нужно. Проценты не пишем внутрь бара:
                    // тёмный текст утонул бы в жёлобе, светлый — в заливке.
                    let bar = match self.progress.fraction() {
                        Some(f) => egui::ProgressBar::new(f),
                        // Размер неизвестен — крутим неопределённый индикатор.
                        None => egui::ProgressBar::new(0.0).animate(true),
                    };
                    ui.add(bar.fill(theme::ACCENT).desired_height(8.0));
                });

                if !self.progress_line.is_empty() {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(&self.progress_line)
                            .small()
                            .color(theme::TEXT_SECONDARY),
                    );
                }
            }
            State::Done(path) => {
                if !self.done_path_display.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.done_path_display)
                                .small()
                                .color(theme::TEXT_SECONDARY),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&self.done_path_display);
                    ui.add_space(10.0);
                }
                if let Some(dir) = path.parent().map(Path::to_path_buf)
                    && ui
                        .add(
                            egui::Button::new("Открыть папку")
                                .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                        )
                        .clicked()
                {
                    open_dir(&dir);
                }
            }
            State::Failed(err) => {
                banner(ui, err, theme::STATE_ERROR);
            }
            State::Cancelled => {
                ui.label(
                    egui::RichText::new("Загрузка отменена.")
                        .small()
                        .color(theme::TEXT_SECONDARY),
                );
            }
            State::Idle => {
                ui.label(
                    egui::RichText::new("Вставьте ссылку и нажмите «Скачать».")
                        .small()
                        .color(theme::TEXT_SECONDARY),
                );
            }
        }
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        if self.log.is_empty() {
            return;
        }

        ui.add_space(14.0);
        egui::CollapsingHeader::new(
            egui::RichText::new("Журнал").small().color(theme::TEXT_SECONDARY),
        )
        .show(ui, |ui| {
            egui::Frame::new()
                .fill(theme::BG_ELEVATED)
                .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(150.0)
                        .stick_to_bottom(true)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.label(
                                    egui::RichText::new(line.as_str())
                                        .monospace()
                                        .color(theme::TEXT_MUTED),
                                );
                            }
                        });
                });
        });
    }
}

// ---------------------------------------------------------------------------
// Мелкие элементы
// ---------------------------------------------------------------------------

fn field_label(ui: &mut egui::Ui, text: &'static str) {
    ui.label(
        egui::RichText::new(text)
            .small()
            .color(theme::TEXT_SECONDARY),
    );
    ui.add_space(6.0);
}

/// Плашка состояния: цветная точка плюс подпись тем же цветом.
/// Цветом одним статус не передаём — рядом всегда есть текст.
fn status_pill(ui: &mut egui::Ui, label: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(theme::BG_ELEVATED)
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL))
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, color);
                ui.label(egui::RichText::new(label).small().strong().color(color));
            });
        });
}

/// Сообщение об ошибке или предупреждение: цветная полоса слева, текст справа.
fn banner(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(theme::BG_ELEVATED)
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let height = ui.text_style_height(&egui::TextStyle::Body);
                let (stripe, _) =
                    ui.allocate_exact_size(egui::vec2(3.0, height), egui::Sense::hover());
                ui.painter()
                    .rect_filled(stripe, egui::CornerRadius::same(2), color);
                ui.label(egui::RichText::new(text).color(color));
            });
        });
}

fn open_dir(dir: &Path) {
    #[cfg(windows)]
    let (program, args) = ("explorer", vec![dir.to_string_lossy().into_owned()]);
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![dir.to_string_lossy().into_owned()]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, args) = ("xdg-open", vec![dir.to_string_lossy().into_owned()]);

    let _ = std::process::Command::new(program).args(args).spawn();
}
