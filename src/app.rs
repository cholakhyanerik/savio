//! Состояние и отрисовка интерфейса.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui;

use crate::engine::setup;
use crate::engine::{self, Handle, MetaTask, metadata};
use crate::model::{
    Event, Format, MediaInfo, Progress, Tag, human_bytes, human_duration, looks_like_url, meta_kind,
};
use crate::theme;

const LOG_LIMIT: usize = 400;

/// Версия для шапки.
///
/// Берётся из `Cargo.toml` на этапе компиляции, руками здесь ничего дублировать
/// не нужно — иначе рано или поздно разъедется. `concat!` тоже раскрывается
/// компилятором, так что в кадре отрисовки это просто готовая строка без
/// единой аллокации.
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

enum State {
    Idle,
    Running,
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

/// Установка недостающих инструментов при первом запуске.
///
/// Отдельно от `State`: это состояние подготовки, а не загрузки, и живёт оно
/// строго до первого показа основного экрана.
enum Setup {
    /// Всё на месте — обычный случай при любом запуске, кроме первого.
    Ready,
    Installing,
    /// Обновление движка по кнопке. Отдельно от `Installing` только ради
    /// подписи в модалке: «Установка зависимостей» при обновлении сбивала бы
    /// с толку — пользователь ничего не устанавливал.
    Updating,
    /// Установка не удалась. Приложение всё равно открывается: без `yt-dlp`
    /// пользователь увидит привычную подсказку, что делать дальше.
    Failed(String),
}

impl Setup {
    /// Идёт ли работа с внешними инструментами прямо сейчас. Пока идёт,
    /// показана модалка и занят единственный канал событий.
    fn busy(&self) -> bool {
        matches!(self, Setup::Installing | Setup::Updating)
    }
}

/// Какой экран показан.
///
/// Вкладки, а не один длинный экран: в окне минимального размера (520×420)
/// загрузка и работа с метаданными вместе уехали бы в прокрутку целиком.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Download,
    Metadata,
}

/// Состояние вкладки «Метаданные».
///
/// Со своим приёмником событий: чистить метаданные во время скачивания —
/// законный сценарий, и события двух задач не должны попадать в один канал.
/// Движка это не касается, он по-прежнему знает только `Sender<Event>`.
struct MetaPanel {
    path: Option<PathBuf>,
    /// Путь строкой. Собирается при выборе файла, а не в кадре отрисовки.
    path_display: String,
    /// Почему с этим файлом работать нельзя. `None` — можно.
    blocked: Option<String>,
    readable: bool,
    cleanable: bool,
    busy: bool,
    stage: String,
    /// Прочитанные метаданные: `Some` — показываем окно со списком.
    /// Пустой список внутри — законный исход, а не ошибка.
    tags: Option<Vec<Tag>>,
    /// Итог последней операции: текст и цвет плашки.
    outcome: Option<(String, egui::Color32)>,
    /// Показан вопрос «точно перезаписать?».
    confirming: bool,
    rx: Option<Receiver<Event>>,
}

impl MetaPanel {
    fn new() -> Self {
        Self {
            path: None,
            path_display: "файл не выбран".to_owned(),
            blocked: None,
            readable: false,
            cleanable: false,
            busy: false,
            stage: String::new(),
            tags: None,
            outcome: None,
            confirming: false,
            rx: None,
        }
    }

    /// Запоминает выбранный файл и сразу решает, что с ним можно делать.
    ///
    /// Решение принимается один раз здесь, а не в кадре отрисовки: иначе
    /// расширение разбиралось бы 60 раз в секунду ради двух флагов.
    fn select(&mut self, path: PathBuf) {
        let kind = meta_kind(&path);
        self.readable = kind.readable();
        self.cleanable = kind.cleanable();
        self.blocked =
            (!kind.readable() || !kind.cleanable()).then(|| metadata::unsupported_message(kind));
        self.path_display = path.display().to_string();
        self.path = Some(path);
        // Результаты относились к прошлому файлу — показывать их рядом
        // с новым нельзя, это прямой повод перепутать.
        self.tags = None;
        self.outcome = None;
        self.stage.clear();
    }

    fn start(&mut self, task: MetaTask, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else {
            return;
        };

        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        engine::start_metadata(path, task, tx, move || notify_ctx.request_repaint());

        self.rx = Some(rx);
        self.busy = true;
        self.tags = None;
        self.outcome = None;
        self.stage = "Запуск…".to_owned();
    }

    fn drain(&mut self) {
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
                Event::Stage(stage) => self.stage = stage,
                Event::Tags(tags) => {
                    self.tags = Some(tags);
                    self.busy = false;
                }
                Event::Cleaned(freed) => {
                    // Ноль освобождённых байт — это не неудача, а чистый файл.
                    // Сказать об этом надо иначе, иначе «освобождено 0 Б»
                    // выглядит как сломавшаяся операция.
                    self.outcome = Some(if freed == 0 {
                        (
                            "Удалять было нечего: метаданных в файле нет.".to_owned(),
                            theme::TEXT_SECONDARY,
                        )
                    } else {
                        (
                            format!("Метаданные удалены, освобождено {}", human_bytes(freed)),
                            theme::STATE_SUCCESS,
                        )
                    });
                    self.busy = false;
                }
                Event::Failed(err) => {
                    self.outcome = Some((err, theme::STATE_ERROR));
                    self.busy = false;
                }
                // Остальные варианты рождаются только загрузкой и установкой,
                // а у них свой приёмник. Пустая ветка вместо `_` — чтобы
                // компилятор и дальше ловил здесь новые варианты `Event`.
                Event::Info(_)
                | Event::Progress(_)
                | Event::Log(_)
                | Event::Done(_)
                | Event::Ready
                | Event::Warning(_)
                | Event::Notice(_) => {}
            }
        }

        if disconnected {
            self.rx = None;
            self.busy = false;
        }
    }
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
    /// Ход установки недостающих инструментов.
    setup: Setup,
    /// Предупреждение, которое переживает установку: например, что ffmpeg
    /// поставить не удалось. Живёт до конца сеанса и не стирается вместе
    /// с журналом при старте загрузки.
    warning: Option<String>,
    /// Хорошая новость с тем же сроком жизни: до какой версии обновился
    /// движок или что он и так был свежим. Без неё удачное обновление
    /// выглядело бы как молча закрывшаяся модалка.
    notice: Option<String>,
    /// Ручка установки — нужна, чтобы её можно было прервать.
    setup_handle: Option<setup::Handle>,

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
    /// Показанная вкладка.
    tab: Tab,
    /// Состояние вкладки «Метаданные».
    meta: MetaPanel,
    /// Окно нужно развернуть на первом кадре.
    ///
    /// Одного `with_maximized(true)` в `main.rs` мало: вместе с
    /// `with_inner_size` он выставляет окну признак развёрнутого, но не
    /// применяет саму геометрию — окно открывается прежнего размера, хотя
    /// `IsZoomed` уже отвечает «развёрнуто». Проверено на Windows 11.
    /// Команду шлём **однократно**: каждый кадр — и пользователь не смог бы
    /// вернуть окну обычный размер.
    maximize_pending: bool,
}

impl SavioApp {
    /// Собирает приложение и, если нужно, сразу запускает установку.
    ///
    /// Проверка наличия инструментов — это несколько обращений к файловой
    /// системе, поэтому её можно делать прямо здесь: когда всё на месте (любой
    /// запуск, кроме первого) окно открывается без единой задержки, как и
    /// требуется. Сама загрузка идёт в отдельном потоке.
    pub fn new(ctx: &egui::Context) -> Self {
        let out_dir = default_download_dir();
        let mut app = Self {
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
            setup_error: None,
            setup: Setup::Ready,
            warning: None,
            notice: None,
            setup_handle: None,
            meta_line: String::new(),
            progress_line: String::new(),
            done_path_display: String::new(),
            url_invalid: false,
            tab: Tab::Download,
            meta: MetaPanel::new(),
            maximize_pending: true,
        };

        let what = setup::missing();
        if what.any() {
            let (tx, rx) = channel();
            let notify_ctx = ctx.clone();
            app.setup_handle = Some(setup::start(what, tx, move || notify_ctx.request_repaint()));
            app.rx = Some(rx);
            app.setup = Setup::Installing;
            app.stage = "Проверяю, чего не хватает…".into();
            app.rebuild_progress_line();
        } else {
            app.setup_error = engine::discover().err();
        }

        app
    }

    /// Вызывается, когда установка закончилась — успехом или нет.
    /// Инструменты после неё нужно искать заново: до установки их не было.
    fn finish_setup(&mut self, outcome: Setup) {
        self.setup = outcome;
        self.setup_handle = None;
        self.rx = None;
        self.handle = None;
        self.setup_error = engine::discover().err();
        self.stage.clear();
        self.progress = Progress::default();
        self.progress_line.clear();
    }

    fn cancel_setup(&mut self) {
        if let Some(handle) = &self.setup_handle {
            handle.cancel();
        }
        self.finish_setup(Setup::Ready);
    }

    /// Обновление движка по кнопке.
    ///
    /// Идёт по тому же каналу и в ту же модалку, что и установка при первом
    /// запуске: задача та же — скачать бинарник и показать прогресс, поэтому
    /// заводить второй механизм незачем.
    fn start_update(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();

        // Прошлый исход убираем: иначе рядом со свежим результатом висела бы
        // причина позапрошлой неудачи и было бы не понять, к чему она.
        self.notice = None;
        if matches!(self.setup, Setup::Failed(_)) {
            self.setup = Setup::Ready;
        }

        self.setup_handle = Some(setup::start_update(tx, move || {
            notify_ctx.request_repaint()
        }));
        self.rx = Some(rx);
        self.setup = Setup::Updating;
        self.progress = Progress::default();
        self.stage = "Проверяю версию…".into();
        self.rebuild_progress_line();
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
                    // Один и тот же вариант обслуживает обе задачи, поэтому
                    // разводим их по текущему режиму: во время установки это
                    // сбой установки, а не сорвавшаяся загрузка ролика.
                    if self.setup.busy() {
                        self.finish_setup(Setup::Failed(err));
                    } else {
                        self.stage = "Ошибка".into();
                        self.state = State::Failed(err);
                        self.handle = None;
                        progress_dirty = true;
                    }
                }
                Event::Ready => {
                    self.finish_setup(Setup::Ready);
                }
                Event::Warning(text) => {
                    self.warning = Some(text);
                }
                Event::Notice(text) => {
                    self.notice = Some(text);
                }
                // Метаданные ходят по своему каналу — сюда эти события
                // попасть не могут. Ветка выписана явно, а не через `_`,
                // чтобы компилятор и дальше требовал разбирать новые
                // варианты `Event` в обоих приёмниках.
                Event::Tags(_) | Event::Cleaned(_) => {}
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
        self.meta.drain();

        if self.maximize_pending {
            self.maximize_pending = false;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }

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
                                self.tab_bar(ui);
                                ui.add_space(16.0);

                                match self.tab {
                                    Tab::Download => self.download_tab(ui),
                                    Tab::Metadata => self.metadata_tab(ui),
                                }
                            });
                    });
            });

        // Модалки рисуются последними, поверх всего остального.
        let ctx = ui.ctx().clone();
        if self.setup.busy() {
            self.install_modal(&ctx);
        }
        if self.meta.tags.is_some() {
            self.tags_modal(&ctx);
        }
        if self.meta.confirming {
            self.confirm_modal(&ctx);
        }
    }
}

impl SavioApp {
    /// Переключатель вкладок под шапкой.
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
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

                    for (tab, label) in [(Tab::Download, "Загрузка"), (Tab::Metadata, "Метаданные")]
                    {
                        if segment_button(ui, label, self.tab == tab, width) {
                            self.tab = tab;
                        }
                    }
                });
            });
    }

    /// Вкладка загрузки — прежний экран целиком.
    fn download_tab(&mut self, ui: &mut egui::Ui) {
        // Причина неудавшейся установки идёт первой: она объясняет, почему
        // инструмента нет, а баннер ниже — что с этим делать.
        if let Setup::Failed(err) = &self.setup {
            banner(ui, err, theme::STATE_WARNING);
            ui.add_space(12.0);
        }
        if let Some(text) = &self.warning {
            banner(ui, text, theme::STATE_WARNING);
            ui.add_space(12.0);
        }
        if let Some(text) = &self.notice {
            banner(ui, text, theme::STATE_SUCCESS);
            ui.add_space(12.0);
        }
        if let Some(err) = &self.setup_error {
            banner(ui, err, theme::STATE_ERROR);
            ui.add_space(12.0);
        }

        self.controls_card(ui);
        ui.add_space(16.0);
        self.status_section(ui);
        self.maintenance_row(ui);
        self.log_section(ui);
    }
}

impl SavioApp {
    /// Модальное окно установки.
    ///
    /// Закрыться само не может и не должно: `ModalResponse::should_close()`
    /// намеренно не вызывается — это не только предикат «щёлкнули мимо или
    /// нажали Esc», он ещё и поглощает Esc. Пока установка идёт, единственный
    /// выход — кнопка «Отменить», иначе оборвавшаяся загрузка заперла бы
    /// пользователя в окне без выхода.
    fn install_modal(&mut self, ctx: &egui::Context) {
        // Строки статические и выбираются по режиму — в кадре ничего
        // не собирается и не выделяется.
        let (title, subtitle) = if matches!(self.setup, Setup::Updating) {
            (
                "Обновление движка",
                "Savio скачивает свежий yt-dlp. Это занимает несколько секунд.",
            )
        } else {
            (
                "Установка зависимостей",
                "Savio догружает недостающие программы. \
                 Это нужно только при первом запуске — пожалуйста, подождите.",
            )
        };

        let cancelled = egui::Modal::new(egui::Id::new("savio-setup"))
            .backdrop_color(theme::MODAL_BACKDROP)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_SURFACE)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(22)),
            )
            .show(ctx, |ui| {
                // Ширину задаём явно: иначе окно скачет по кадрам вслед за
                // длиной строки прогресса. 400 подобрано так, чтобы строка
                // «стадия · проценты · объём · скорость» помещалась целиком.
                ui.set_width(400.0);

                ui.label(
                    egui::RichText::new(title)
                        .heading()
                        .strong()
                        .color(theme::TEXT_PRIMARY),
                );
                ui.add_space(6.0);
                ui.label(egui::RichText::new(subtitle).color(theme::TEXT_SECONDARY));

                ui.add_space(16.0);

                ui.scope(|ui| {
                    ui.visuals_mut().extreme_bg_color = theme::PROGRESS_TRACK;
                    // Скругление бару не задаём: вместе с `animate` оно
                    // отключает отрисовку бегущей полосы, а она здесь —
                    // единственный признак, что установка не зависла.
                    let bar = match self.progress.fraction() {
                        Some(f) => egui::ProgressBar::new(f),
                        None => egui::ProgressBar::new(0.0).animate(true),
                    };
                    ui.add(bar.fill(theme::ACCENT).desired_height(8.0));
                });

                if !self.progress_line.is_empty() {
                    ui.add_space(8.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.progress_line)
                                .small()
                                .color(theme::TEXT_SECONDARY),
                        )
                        .truncate(),
                    );
                }

                ui.add_space(18.0);
                ui.add(
                    egui::Button::new("Отменить").min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                )
                .clicked()
            })
            .inner;

        if cancelled {
            self.cancel_setup();
        }
    }

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
                    ui.painter().circle_filled(dot.center(), 3.5, theme::ACCENT);
                    ui.label(
                        egui::RichText::new("видео и аудио по ссылке").color(theme::TEXT_SECONDARY),
                    );

                    // Версию прижимаем к правому краю: она нужна, когда
                    // выясняют, почему что-то не работает, но в остальное
                    // время не должна тянуть на себя внимание.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(VERSION)
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                    });
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
    fn segment(&mut self, ui: &mut egui::Ui, format: Format, width: f32) {
        if segment_button(ui, format.label(), self.format == format, width) {
            self.format = format;
        }
    }

    fn folder_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let button = ui.add(
                egui::Button::new("Выбрать…").min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
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
                        egui::RichText::new(title)
                            .strong()
                            .color(theme::TEXT_PRIMARY),
                    )
                    .truncate(),
                )
                .on_hover_text(title);
            }
        });

        if !self.meta_line.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(&self.meta_line)
                    .small()
                    .color(theme::TEXT_SECONDARY),
            );
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

    /// Обслуживание: обновление движка.
    ///
    /// Стоит внизу, рядом с журналом, а не у кнопки «Скачать», и намеренно:
    /// это то, за чем идут, когда что-то перестало работать, — соседство
    /// с журналом и версией в шапке тут уместнее, чем спор за внимание
    /// с главным действием экрана.
    ///
    /// Подпись на отдельной строке под кнопкой, а не сбоку: в окне минимальной
    /// ширины (520) строка рядом с кнопкой не поместилась бы.
    fn maintenance_row(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(12.0);

        // Пока занят единственный канал событий — обновляться нечем: и
        // загрузка, и установка ходят через тот же `rx`.
        let enabled = !matches!(self.state, State::Running) && !self.setup.busy();

        let clicked = ui
            .add_enabled_ui(enabled, |ui| {
                ui.add(
                    egui::Button::new("Обновить движок")
                        .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                )
                .clicked()
            })
            .inner;

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Сайты меняются, и старый yt-dlp перестаёт их скачивать. \
                 Если ссылка вдруг не работает — обновите движок.",
            )
            .small()
            .color(theme::TEXT_MUTED),
        );

        if clicked {
            let ctx = ui.ctx().clone();
            self.start_update(&ctx);
        }
    }

    fn log_section(&mut self, ui: &mut egui::Ui) {
        if self.log.is_empty() {
            return;
        }

        ui.add_space(14.0);
        egui::CollapsingHeader::new(
            egui::RichText::new("Журнал")
                .small()
                .color(theme::TEXT_SECONDARY),
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
// Вкладка «Метаданные»
// ---------------------------------------------------------------------------

impl SavioApp {
    fn metadata_tab(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(theme::BG_SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                field_label(ui, "Файл");
                self.meta_file_row(ui);

                if let Some(blocked) = &self.meta.blocked {
                    ui.add_space(12.0);
                    banner(ui, blocked, theme::STATE_WARNING);
                }

                ui.add_space(18.0);
                self.meta_buttons(ui);
            });

        ui.add_space(16.0);
        self.meta_status(ui);
    }

    fn meta_file_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let pick = ui.add_enabled(
                !self.meta.busy,
                egui::Button::new("Выбрать файл…").min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
            );

            if pick.clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter(
                        "Поддерживаемые файлы",
                        &["mp3", "jpg", "jpeg", "png", "webp", "gif", "tif", "tiff"],
                    )
                    .add_filter("Все файлы", &["*"])
                    .pick_file()
            {
                self.meta.select(path);
            }

            let color = if self.meta.path.is_some() {
                theme::TEXT_SECONDARY
            } else {
                theme::TEXT_MUTED
            };
            // Длинный путь обрезаем, иначе он растягивает окно.
            ui.add(
                egui::Label::new(egui::RichText::new(&self.meta.path_display).color(color))
                    .truncate(),
            )
            .on_hover_text(&self.meta.path_display);
        });
    }

    fn meta_buttons(&mut self, ui: &mut egui::Ui) {
        // Пока файл не выбран, подсказка должна объяснять именно это, а не
        // молча выключенную кнопку.
        let hint = match (&self.meta.path, &self.meta.blocked) {
            (None, _) => Some("Сначала выберите файл."),
            (Some(_), Some(_)) => None, // причина уже показана баннером выше
            _ => None,
        };

        let (read_on, clean_on) = (
            self.meta.readable && !self.meta.busy,
            self.meta.cleanable && !self.meta.busy,
        );

        ui.horizontal(|ui| {
            const GAP: f32 = 10.0;
            ui.spacing_mut().item_spacing.x = GAP;
            let width = (ui.available_width() - GAP) / 2.0;

            let read = ui.add_enabled(
                read_on,
                egui::Button::new("Читать").min_size(egui::vec2(width, theme::CTA_HEIGHT)),
            );
            let read = match hint {
                Some(text) => read.on_disabled_hover_text(text),
                None => read.on_disabled_hover_text(
                    self.meta
                        .blocked
                        .as_deref()
                        .unwrap_or("Сначала выберите файл."),
                ),
            };
            if read.clicked() {
                let ctx = ui.ctx().clone();
                self.meta.start(MetaTask::Read, &ctx);
            }

            // «Удалить» — главное действие вкладки, поэтому акцентная заливка.
            // Выключенный вид задаём явно: `ui.disable()` не переключает виджет
            // на `noninteractive`, а только глушит прозрачность, и выключенная
            // кнопка стала бы неотличима от включённой.
            let clicked = ui
                .scope(|ui| {
                    let v = ui.visuals_mut();
                    let (rest, hover, press) = if clean_on {
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
                    v.disabled_alpha = 1.0;

                    ui.add_enabled(
                        clean_on,
                        egui::Button::new(egui::RichText::new("Удалить").strong())
                            .min_size(egui::vec2(width, theme::CTA_HEIGHT)),
                    )
                    .on_disabled_hover_text(
                        self.meta
                            .blocked
                            .as_deref()
                            .unwrap_or("Сначала выберите файл."),
                    )
                    .clicked()
                })
                .inner;

            if clicked {
                // Файл перезаписывается на месте, и вернуть метаданные будет
                // нельзя. Один вопрос дешевле безвозвратно очищенного оригинала.
                self.meta.confirming = true;
            }
        });
    }

    fn meta_status(&mut self, ui: &mut egui::Ui) {
        if self.meta.busy {
            ui.scope(|ui| {
                ui.visuals_mut().extreme_bg_color = theme::PROGRESS_TRACK;
                // Сколько осталось, здесь неизвестно и не нужно: операция
                // укладывается в доли секунды. Крутим неопределённый индикатор.
                ui.add(
                    egui::ProgressBar::new(0.0)
                        .animate(true)
                        .fill(theme::ACCENT)
                        .desired_height(8.0),
                );
            });
            if !self.meta.stage.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(&self.meta.stage)
                        .small()
                        .color(theme::TEXT_SECONDARY),
                );
            }
            return;
        }

        if let Some((text, color)) = &self.meta.outcome {
            banner(ui, text, *color);
            return;
        }

        ui.label(
            egui::RichText::new(
                "Выберите MP3 или изображение. «Читать» покажет, что записано \
                 в файле, «Удалить» — сотрёт теги, геометку и обложку, \
                 не трогая само содержимое.",
            )
            .small()
            .color(theme::TEXT_MUTED),
        );
    }

    /// Окно со списком прочитанных метаданных.
    fn tags_modal(&mut self, ctx: &egui::Context) {
        let Some(tags) = &self.meta.tags else {
            return;
        };

        // Размеры считаем от окна, а не константами. При фиксированных 440×320
        // в окне минимального размера (520×420) модалка не помещалась: заголовок
        // срезало сверху, кнопку «Закрыть» — снизу, и окно становилось нечем
        // закрыть. Сборка такого не ловит, видно только глазами.
        let screen = ctx.content_rect();
        let width = 440.0_f32.min(screen.width() - 48.0);
        // Вычитаем то, что модалка занимает помимо списка: поля, заголовок,
        // отступы и кнопку.
        let list_height = (screen.height() - 230.0).clamp(110.0, 320.0);

        let close = egui::Modal::new(egui::Id::new("savio-tags"))
            .backdrop_color(theme::MODAL_BACKDROP)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_SURFACE)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(22)),
            )
            .show(ctx, |ui| {
                ui.set_width(width);

                ui.label(
                    egui::RichText::new("Метаданные файла")
                        .heading()
                        .strong()
                        .color(theme::TEXT_PRIMARY),
                );
                ui.add_space(10.0);

                if tags.is_empty() {
                    ui.label(
                        egui::RichText::new("Метаданные не найдены.").color(theme::TEXT_SECONDARY),
                    );
                } else {
                    // Список может быть длинным (у снимка с телефона легко
                    // набирается пара десятков строк) — держим его в прокрутке,
                    // иначе окно вылезет за экран.
                    egui::ScrollArea::vertical()
                        .max_height(list_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for tag in tags {
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    // Имя фиксированной ширины: иначе значения
                                    // не выстроятся в колонку и читать список
                                    // станет заметно тяжелее.
                                    ui.add_sized(
                                        [150.0, ui.text_style_height(&egui::TextStyle::Body)],
                                        egui::Label::new(
                                            egui::RichText::new(&tag.name)
                                                .small()
                                                .color(theme::TEXT_MUTED),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&tag.name);

                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&tag.value)
                                                .color(theme::TEXT_PRIMARY),
                                        )
                                        .wrap(),
                                    );
                                });
                                ui.add_space(6.0);
                            }
                        });
                }

                ui.add_space(18.0);
                ui.add(
                    egui::Button::new("Закрыть").min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                )
                .clicked()
            });

        // В отличие от модалки установки, здесь `should_close` уместен:
        // окно ничего не делает и запереть в нём пользователя нечем, поэтому
        // Esc и щелчок мимо должны закрывать его как обычно.
        if close.inner || close.should_close() {
            self.meta.tags = None;
        }
    }

    /// Подтверждение перезаписи файла.
    fn confirm_modal(&mut self, ctx: &egui::Context) {
        #[derive(PartialEq)]
        enum Answer {
            None,
            Yes,
            No,
        }

        let answer = egui::Modal::new(egui::Id::new("savio-confirm"))
            .backdrop_color(theme::MODAL_BACKDROP)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_SURFACE)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(22)),
            )
            .show(ctx, |ui| {
                ui.set_width(400.0_f32.min(ctx.content_rect().width() - 48.0));

                ui.label(
                    egui::RichText::new("Перезаписать файл?")
                        .heading()
                        .strong()
                        .color(theme::TEXT_PRIMARY),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Метаданные будут стёрты из самого файла, копия не создаётся. \
                         Вернуть их обратно будет нельзя.",
                    )
                    .color(theme::TEXT_SECONDARY),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&self.meta.path_display)
                            .small()
                            .color(theme::TEXT_MUTED),
                    )
                    .truncate(),
                );

                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    const GAP: f32 = 10.0;
                    ui.spacing_mut().item_spacing.x = GAP;
                    let width = (ui.available_width() - GAP) / 2.0;

                    if ui
                        .add(
                            egui::Button::new("Отмена")
                                .min_size(egui::vec2(width, theme::CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        return Answer::No;
                    }
                    if ui
                        .add(
                            egui::Button::new("Удалить")
                                .min_size(egui::vec2(width, theme::CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        return Answer::Yes;
                    }
                    Answer::None
                })
                .inner
            });

        // Esc и щелчок мимо — это отказ. Трактовать их как согласие на
        // необратимую операцию нельзя.
        let dismissed = answer.should_close();
        match answer.inner {
            Answer::Yes => {
                self.meta.confirming = false;
                self.meta.start(MetaTask::Clean, ctx);
            }
            Answer::No => self.meta.confirming = false,
            Answer::None if dismissed => self.meta.confirming = false,
            Answer::None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Мелкие элементы
// ---------------------------------------------------------------------------

/// Один сегмент переключателя: выбранный — жёлтый, остальные — утопленные.
///
/// Цвета задаём через `visuals`, а не через `Button::fill`: последний,
/// по документации egui, отключает реакцию на наведение — кнопка выглядела бы
/// мёртвой. Одна функция на переключатель формата и на вкладки: разъехавшись,
/// два одинаковых на вид элемента смотрелись бы досадной небрежностью.
fn segment_button(ui: &mut egui::Ui, label: &str, selected: bool, width: f32) -> bool {
    ui.scope(|ui| {
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

        ui.add(egui::Button::new(label).min_size(egui::vec2(width, theme::CONTROL_HEIGHT - 6.0)))
            .clicked()
    })
    .inner
}

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
