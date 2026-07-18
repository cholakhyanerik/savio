//! Состояние и отрисовка интерфейса.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui;

use crate::engine::{self, Handle};
use crate::model::{Event, Format, MediaInfo, Progress, human_bytes, human_duration};

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
}

impl Default for SavioApp {
    fn default() -> Self {
        Self {
            url: String::new(),
            format: Format::Mp4,
            out_dir: default_download_dir(),
            state: State::Idle,
            progress: Progress::default(),
            stage: String::new(),
            info: None,
            log: Vec::new(),
            rx: None,
            handle: None,
            setup_error: engine::discover().err(),
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

        for event in events {
            match event {
                Event::Info(info) => self.info = Some(info),
                Event::Stage(stage) => self.stage = stage,
                Event::Progress(p) => self.progress = p,
                Event::Log(line) => {
                    self.log.push(line);
                    if self.log.len() > LOG_LIMIT {
                        self.log.drain(..self.log.len() - LOG_LIMIT);
                    }
                }
                Event::Done(path) => {
                    self.stage = "Готово".into();
                    self.state = State::Done(path);
                    self.handle = None;
                }
                Event::Failed(err) => {
                    self.stage = "Ошибка".into();
                    self.state = State::Failed(err);
                    self.handle = None;
                }
            }
        }

        if disconnected {
            self.rx = None;
            if matches!(self.state, State::Running) {
                self.state = State::Idle;
            }
        }
    }
}

impl eframe::App for SavioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Savio");
            ui.add_space(4.0);

            if let Some(err) = &self.setup_error {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                ui.add_space(6.0);
            }

            ui.horizontal(|ui| {
                ui.label("Ссылка:");
                ui.add_sized(
                    [ui.available_width(), 20.0],
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://…"),
                );
            });

            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("Формат:");
                ui.radio_value(&mut self.format, Format::Mp4, Format::Mp4.label());
                ui.radio_value(&mut self.format, Format::Mp3, Format::Mp3.label());
            });

            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.label("Папка:");
                let shown = self
                    .out_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "не выбрана".to_owned());
                ui.label(shown);
                if ui.button("Выбрать…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.out_dir = Some(dir);
                    }
            });

            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if matches!(self.state, State::Running) {
                    if ui.button("Отмена").clicked() {
                        self.cancel();
                    }
                } else {
                    let start = ui.add_enabled(
                        self.can_start(),
                        egui::Button::new("Скачать"),
                    );
                    if start.clicked() {
                        let ctx = ui.ctx().clone();
                        self.start(&ctx);
                    }
                }
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);

            if let Some(info) = &self.info {
                if let Some(title) = &info.title {
                    ui.strong(title);
                }
                let mut meta = Vec::new();
                if let Some(uploader) = &info.uploader {
                    meta.push(uploader.clone());
                }
                if let Some(secs) = info.duration_secs {
                    meta.push(human_duration(secs as u64));
                }
                if !meta.is_empty() {
                    ui.weak(meta.join(" · "));
                }
                ui.add_space(6.0);
            }

            match &self.state {
                State::Running => {
                    let bar = match self.progress.fraction() {
                        Some(f) => egui::ProgressBar::new(f).show_percentage(),
                        // Размер неизвестен — крутим неопределённый индикатор.
                        None => egui::ProgressBar::new(0.0).animate(true),
                    };
                    ui.add(bar);

                    let mut parts = vec![self.stage.clone()];
                    if self.progress.total > 0 {
                        parts.push(format!(
                            "{} из {}",
                            human_bytes(self.progress.downloaded),
                            human_bytes(self.progress.total)
                        ));
                    }
                    if let Some(speed) = self.progress.speed_bps {
                        parts.push(format!("{}/с", human_bytes(speed as u64)));
                    }
                    if let Some(eta) = self.progress.eta_secs {
                        parts.push(format!("осталось {}", human_duration(eta)));
                    }
                    ui.weak(parts.join(" · "));
                }
                State::Done(path) => {
                    ui.colored_label(egui::Color32::from_rgb(80, 180, 100), "Готово");
                    ui.weak(path.display().to_string());
                    if let Some(dir) = path.parent().map(Path::to_path_buf)
                        && ui.button("Открыть папку").clicked() {
                            open_dir(&dir);
                        }
                }
                State::Failed(err) => {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                }
                State::Cancelled => {
                    ui.weak("Отменено");
                }
                State::Idle => {
                    ui.weak("Вставьте ссылку и нажмите «Скачать».");
                }
            }

            if !self.log.is_empty() {
                ui.add_space(8.0);
                ui.collapsing("Журнал", |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .stick_to_bottom(true)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.weak(line.as_str());
                            }
                        });
                });
            }
        });
    }
}

use std::path::Path;

fn open_dir(dir: &Path) {
    #[cfg(windows)]
    let (program, args) = ("explorer", vec![dir.to_string_lossy().into_owned()]);
    #[cfg(target_os = "macos")]
    let (program, args) = ("open", vec![dir.to_string_lossy().into_owned()]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, args) = ("xdg-open", vec![dir.to_string_lossy().into_owned()]);

    let _ = std::process::Command::new(program).args(args).spawn();
}
