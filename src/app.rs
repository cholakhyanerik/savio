//! Состояние и отрисовка интерфейса.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use eframe::egui;

use crate::engine::settings;
use crate::engine::setup;
use crate::engine::{self, Handle, MetaTask, metadata};
use crate::model::{
    CookieSource, DownloadOptions, Event, Format, MediaInfo, Progress, Quality, Request, Section,
    SectionError, Tag, human_bytes, human_duration, human_speed, looks_like_url, meta_kind,
    parse_section,
};
use crate::theme;

const LOG_LIMIT: usize = 400;

/// Ширина превью обложки в точках.
///
/// 240 — половина того, что остаётся от окна минимальной ширины (520 минус
/// поля дают 480): картинка заметна, но не выдавливает прогресс и журнал
/// за нижнюю кромку. Движок уменьшает обложку до 480 настоящих точек, так что
/// на экране с двойной плотностью превью остаётся резким.
const PREVIEW_WIDTH: f32 = 240.0;

/// Потолок высоты превью.
///
/// Нужен из-за вертикальных роликов: обложка 9:16 при ширине 240 заняла бы
/// 427 точек — больше, чем всё окно минимальной высоты (420). С потолком такая
/// картинка просто становится узкой, а не выдавливает содержимое в прокрутку.
const PREVIEW_MAX_HEIGHT: f32 = 150.0;

/// Потолок высоты раскрытого списка браузеров.
///
/// Считается от числа источников, а не подобран на глаз: добавится браузер —
/// список подрастёт сам, и никто не будет гадать, почему последний пункт
/// уехал в прокрутку. 28 — строка списка (26 точек) плюс промежуток (2),
/// 12 — поля рамки меню сверху и снизу.
const COOKIE_LIST_HEIGHT: f32 = CookieSource::ALL.len() as f32 * 28.0 + 12.0;

/// Сколько секунд висит подпись «Скопировано» после нажатия.
///
/// Буфер обмена пользователю не виден, и без подтверждения кнопка выглядит
/// не сработавшей. Держать подпись постоянно тоже нельзя: через минуту она
/// уже не про текущий журнал.
const COPIED_NOTICE_SECS: f64 = 2.0;

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
    History,
}

/// Сколько загрузок помним.
///
/// Потолок обязателен по Правилу 1, как и `LOG_LIMIT`: без него список рос бы
/// всё время работы окна, а по одной ссылке-плейлисту приезжают сотни готовых
/// файлов подряд. 50 — заведомо больше, чем скачивают за один запуск, и при
/// этом столько, сколько ещё можно пролистать глазами.
const HISTORY_LIMIT: usize = 50;

/// Одна строка истории: что скачали и где оно лежит.
///
/// Строки собираются один раз, на приёме `Event::Done`, — в кадре отрисовки
/// не остаётся ни `format!`, ни `display()` (Правило 1).
struct HistoryEntry {
    /// Имя файла.
    name: String,
    /// Папка, которую открывает кнопка. `None` — открывать нечего,
    /// и кнопки тогда нет.
    dir: Option<PathBuf>,
    /// Та же папка строкой. Пустая, когда `dir` — `None`.
    dir_display: String,
    /// Полный путь: по нему узнаём повторную загрузку того же файла.
    path: PathBuf,
}

/// История загрузок за текущий запуск.
///
/// Живёт только в памяти и только до закрытия окна — на диск не пишется
/// ничего. Отдельный тип, а не голый `Vec`, ровно ради потолка: держать его
/// в одном месте надёжнее, чем помнить про `truncate` в каждом месте, где
/// в список что-то кладут.
#[derive(Default)]
struct History {
    /// Сверху самое свежее: ищут обычно последнее скачанное.
    entries: Vec<HistoryEntry>,
}

impl History {
    /// Запоминает готовый файл.
    ///
    /// Тот же путь второй раз не заводит новую строку, а поднимает старую
    /// наверх: перекачать файл заново — обычное дело (выбрали не тот формат,
    /// оборвалась связь), и две одинаковые строки подряд выглядели бы сбоем.
    fn remember(&mut self, path: &Path) {
        self.entries.retain(|entry| entry.path.as_path() != path);

        // Пустого родителя отбрасываем вместе с отсутствующим: у относительного
        // «file.mp4» `parent()` возвращает не `None`, а `Some("")`, и такой
        // «папкой» проводнику открывать нечего. От yt-dlp приходят абсолютные
        // пути, так что это страховка, а не рабочий случай.
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf);

        self.entries.insert(
            0,
            HistoryEntry {
                // Путь без имени файла (корень диска) в `Event::Done` прийти
                // не может, но пустая строка в списке выглядела бы поломкой —
                // показываем тогда путь целиком.
                name: path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                dir_display: dir
                    .as_deref()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default(),
                dir,
                path: path.to_path_buf(),
            },
        );

        self.entries.truncate(HISTORY_LIMIT);
    }
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
                | Event::Thumbnail(_)
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
    /// Выбранная ступень качества. Отдельно от формата, как и в модели:
    /// переключение MP4 ↔ MP3 не должно её сбрасывать.
    quality: Quality,
    /// Галочки «вшить метаданные / обложку / субтитры».
    options: DownloadOptions,
    /// Границы фрагмента — как их набрал человек. Строки, а не числа: поле
    /// ввода хранит текст, а разбирает его домен (`parse_section`).
    ///
    /// Между запусками намеренно не запоминаются, как и источник cookies:
    /// границы относятся к конкретному ролику, и вчерашние «1:30 — 4:00»,
    /// молча применённые к сегодняшней ссылке, дали бы не тот файл.
    section_start: String,
    section_end: String,
    /// Разобранные границы. Пересобираются на правке поля, а не в кадре:
    /// `ui()` зовут 60 раз в секунду, а меняется это от нажатия клавиши.
    section: Section,
    /// Чем именно плох набранный диапазон. `None` — всё в порядке (в том
    /// числе когда оба поля пусты).
    section_error: Option<SectionError>,
    /// Из какого браузера брать вход в аккаунт.
    ///
    /// Между запусками намеренно не запоминается, в отличие от формата,
    /// качества и папки: это доступ к чужому профилю браузера, и включаться
    /// он должен осознанно, а не сам собой через неделю после того, как
    /// понадобился один раз.
    cookies: CookieSource,
    /// ffmpeg не нашёлся при последней проверке. Снимок с запуска (и с конца
    /// установки) — единственное, что можно спросить, не трогая диск в кадре.
    /// Нужен, чтобы предупредить о бесполезных галочках **до** нажатия
    /// «Скачать»; окончательное слово всё равно за движком в момент запуска.
    ffmpeg_missing: bool,
    out_dir: Option<PathBuf>,
    state: State,
    progress: Progress,
    stage: String,
    info: Option<MediaInfo>,
    /// Обложка ролика, уже залитая в текстуру egui.
    ///
    /// Именно текстура, а не байты: заводится она один раз, на приёме
    /// `Event::Thumbnail`, и в кадре отрисовки остаётся только нарисовать.
    /// Живёт до старта следующей загрузки — освобождает текстуру egui сам,
    /// когда ручку заменяют или бросают.
    thumbnail: Option<egui::TextureHandle>,
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
    /// Оговорка под переключателем качества: источник не отдаёт столько,
    /// сколько запросили. Пустая строка — оговорки нет.
    quality_note: String,
    /// Ссылка не похожа на ссылку. Только подсветка поля — кнопку не блокирует.
    url_invalid: bool,
    /// Когда журнал скопировали, по часам egui. Нужно только для подписи
    /// «Скопировано»: она живёт `COPIED_NOTICE_SECS` и гаснет сама.
    log_copied_at: Option<f64>,
    /// Показанная вкладка.
    tab: Tab,
    /// Состояние вкладки «Метаданные».
    meta: MetaPanel,
    /// Что скачано за этот запуск. Наполняется из `Event::Done`.
    history: History,
    /// Окно нужно развернуть на первом кадре.
    ///
    /// Одного `with_maximized(true)` в `main.rs` мало: вместе с
    /// `with_inner_size` он выставляет окну признак развёрнутого, но не
    /// применяет саму геометрию — окно открывается прежнего размера, хотя
    /// `IsZoomed` уже отвечает «развёрнуто». Проверено на Windows 11.
    /// Команду шлём **однократно**: каждый кадр — и пользователь не смог бы
    /// вернуть окну обычный размер.
    maximize_pending: bool,
    /// Кто запоминает выбор на следующий запуск.
    ///
    /// Пишет на своём потоке: сама запись — это ввод-вывод, а зовут её из
    /// обработчика щелчка, то есть из кадра отрисовки.
    saver: settings::Saver,
}

impl SavioApp {
    /// Собирает приложение и, если нужно, сразу запускает установку.
    ///
    /// Проверка наличия инструментов — это несколько обращений к файловой
    /// системе, поэтому её можно делать прямо здесь: когда всё на месте (любой
    /// запуск, кроме первого) окно открывается без единой задержки, как и
    /// требуется. Сама загрузка идёт в отдельном потоке.
    pub fn new(ctx: &egui::Context) -> Self {
        // Выбор прошлого запуска. Файла нет, он битый или папка исчезла —
        // получаем ровно те умолчания, что раньше были зашиты здесь: MP4,
        // максимальное качество и каталог загрузок.
        let saved = settings::load();
        let out_dir = saved.out_dir.or_else(default_download_dir);

        let mut app = Self {
            url: String::new(),
            format: saved.format,
            quality: saved.quality,
            options: DownloadOptions::default(),
            section_start: String::new(),
            section_end: String::new(),
            section: Section::default(),
            section_error: None,
            cookies: CookieSource::default(),
            ffmpeg_missing: false,
            out_dir_display: display_dir(out_dir.as_deref()),
            out_dir,
            state: State::Idle,
            progress: Progress::default(),
            stage: String::new(),
            info: None,
            thumbnail: None,
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
            quality_note: String::new(),
            url_invalid: false,
            log_copied_at: None,
            tab: Tab::Download,
            meta: MetaPanel::new(),
            history: History::default(),
            maximize_pending: true,
            saver: settings::Saver::spawn(),
        };

        app.ffmpeg_missing = !engine::has_ffmpeg();

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
        // Ради этой строки установка и затевалась: до неё ffmpeg могло не быть.
        self.ffmpeg_missing = !engine::has_ffmpeg();
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

    /// Отдаёт текущий выбор писателю настроек.
    ///
    /// Зовётся из обработчиков переключателей — там же, где вызывается
    /// `rebuild_*`, и ровно по той же причине: собирать снимок 60 раз
    /// в секунду незачем, меняется он от щелчка. **Новое запоминаемое поле
    /// нужно не только добавить в `Settings`, но и не забыть позвать
    /// `remember` там, где его меняют**: пропуск ничего не сломает и никак
    /// не проявится, кроме как «эта настройка почему-то не запоминается».
    fn remember(&self) {
        self.saver.save(settings::Settings {
            format: self.format,
            quality: self.quality,
            out_dir: self.out_dir.clone(),
        });
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
            // Неразобранный диапазон кнопку **блокирует**, в отличие от
            // непохожей на ссылку строки. Разница не в строгости, а в том,
            // чья это проверка: список поддерживаемых сайтов принадлежит
            // yt-dlp, и наша догадка о ссылке не повод запрещать попытку.
            // А границы фрагмента — наши целиком, и промах в них yt-dlp
            // ошибкой не считает: он молча отдаёт файл не с тем содержимым,
            // и заметить подмену можно, только открыв его.
            && self.section_error.is_none()
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(out_dir) = self.out_dir.clone() else {
            return;
        };

        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();

        let request = Request {
            url: self.url.trim().to_owned(),
            format: self.format,
            quality: self.quality,
            options: self.options,
            cookies: self.cookies,
            section: self.section,
        };

        match engine::start(request, out_dir, tx, move || notify_ctx.request_repaint()) {
            Ok(handle) => {
                self.rx = Some(rx);
                self.handle = Some(handle);
                self.state = State::Running;
                self.progress = Progress::default();
                self.info = None;
                // Обложка относилась к прошлой ссылке. Оставить её рядом
                // с новой — прямой повод перепутать ролики, а ровно от этого
                // превью и должно спасать.
                self.thumbnail = None;
                self.stage = "Запуск…".into();
                self.log.clear();
                self.meta_line.clear();
                self.done_path_display.clear();
                // Оговорка относилась к прошлой ссылке: новые высоты приедут
                // вместе с `Event::Info`, а до тех пор говорить нечего.
                self.quality_note.clear();
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
            line.push_str(&human_speed(speed));
        }
        if let Some(eta) = p.eta_secs {
            sep(line);
            let _ = write!(line, "осталось {}", human_duration(eta));
        }
    }

    fn rebuild_meta_line(&mut self) {
        use std::fmt::Write as _;

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
        // Что источник вообще способен отдать. Показываем и при выборе MP3:
        // это свойство ролика, а не запроса, и знать его полезно до того,
        // как выбирать высоту в следующий раз.
        if let Some(height) = info.max_height() {
            if !self.meta_line.is_empty() {
                self.meta_line.push_str(" · ");
            }
            let _ = write!(self.meta_line, "до {height}p");
        }
    }

    /// Оговорка под переключателем качества.
    ///
    /// Ошибкой это не является: цепочка `-f` заканчивается общим запасным
    /// вариантом и молча опустится до лучшего доступного. Но молча — плохо:
    /// «1080p» в окне и 480p в файле выглядят как обман, поэтому о разнице
    /// говорим сразу, как только высоты приехали от `probe`.
    fn rebuild_quality_note(&mut self) {
        use std::fmt::Write as _;

        self.quality_note.clear();

        // У звука высота ни при чём: там ступень — это битрейт, и потолка,
        // о котором `probe` мог бы рассказать, у него нет.
        if self.format != Format::Mp4 {
            return;
        }
        let (Some(want), Some(have)) = (
            self.quality.max_height(),
            self.info.as_ref().and_then(MediaInfo::max_height),
        ) else {
            return;
        };
        if want > have {
            let _ = write!(
                self.quality_note,
                "Выше {have}p этот ролик не отдают — скачается {have}p."
            );
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
    ///
    /// `ctx` нужен обложке: текстура заводится здесь, на приёме события, —
    /// единственный момент, когда это делается за всю загрузку.
    fn drain_events(&mut self, ctx: &egui::Context) {
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
                Event::Thumbnail(cover) => {
                    // Единственная заливка текстуры за всю загрузку — и она
                    // здесь, на приёме события, а не в кадре. Разбор картинки
                    // уже сделал движок: сюда приезжает готовый RGBA.
                    //
                    // Размеры проверяем, хотя движок это уже сделал: между ним
                    // и нами канал, а `ColorImage` на несовпадении не возвращает
                    // ошибку, а паникует. Уронить окно из-за украшения нельзя,
                    // поэтому проверка на обеих сторонах.
                    if cover.is_valid() {
                        self.thumbnail = Some(ctx.load_texture(
                            "savio-cover",
                            egui::ColorImage::from_rgba_unmultiplied(
                                [cover.width, cover.height],
                                &cover.rgba,
                            ),
                            egui::TextureOptions::LINEAR,
                        ));
                    }
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
                    // Единственное место, где пополняется история: другого
                    // признака «файл готов и лежит вот здесь» у UI нет.
                    // Успех без пути (`Event::Stage("Готово (файл уже
                    // существовал)")`) сюда не попадает — записывать нечего.
                    self.history.remember(&path);
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
            self.rebuild_quality_note();
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

    /// Последнее, что успевает случиться перед закрытием.
    ///
    /// Здесь дописывается отложенная настройка: переключить формат и тут же
    /// закрыть окно — обычное дело, а окно дебаунса к этому моменту ещё не
    /// истекло. Полагаться вместо этого на `Drop` нельзя: eframe сразу после
    /// `on_exit` зовёт `std::process::exit(0)`, и деструкторы не выполняются.
    ///
    /// `App::save` не годится по другой причине: он вызывается только с фичей
    /// `persistence`, а она тянет `ron`, `serde` и `home` ради файла, который
    /// у нас и так пишется своими силами через уже имеющийся `serde_json`.
    ///
    /// Подпись без аргумента — вариант для сборки без `glow`; Savio собирается
    /// на wgpu, то есть на умолчаниях eframe.
    fn on_exit(&mut self) {
        self.saver.flush();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_events(ui.ctx());
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
                                    Tab::History => self.history_tab(ui),
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
    ///
    /// Ширину сегмента считаем на каждом шаге заново, а не делим доступную
    /// один раз на число вкладок, — по той же причине, что и в переключателе
    /// качества: округления до пиксельной сетки накапливаются, и дорожка либо
    /// не дотягивается до правого края, либо вылезает за него. Так последнему
    /// сегменту достаётся ровно то, что осталось.
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        // Порядок здесь — порядок на экране. Новая вкладка добавляется строкой
        // сюда: ширину сегментов пересчитает цикл, делённого пополам числа
        // в коде больше нет.
        const TABS: [(Tab, &str); 3] = [
            (Tab::Download, "Загрузка"),
            (Tab::Metadata, "Метаданные"),
            (Tab::History, "История"),
        ];

        egui::Frame::new()
            .fill(theme::BG_INPUT)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_STRONG))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS))
            .inner_margin(egui::Margin::same(3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    const GAP: f32 = 3.0;
                    ui.spacing_mut().item_spacing.x = GAP;

                    let mut left = TABS.len() as f32;
                    for (tab, label) in TABS {
                        let width = (ui.available_width() - GAP * (left - 1.0)) / left;
                        left -= 1.0;

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
                // Подпись зависит от формата: у видео ступени — это высота
                // кадра, у звука — килобиты в секунду.
                field_label(ui, self.format.quality_label());
                self.quality_selector(ui);

                ui.add_space(14.0);
                field_label(ui, "Фрагмент");
                self.section_row(ui);

                ui.add_space(14.0);
                field_label(ui, "Вшить в файл");
                self.embed_options(ui);

                ui.add_space(14.0);
                field_label(ui, "Cookies из браузера");
                self.cookie_selector(ui);

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
                    mark_invalid(ui);
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
            // Подписи сегментов качества и оговорка под ними зависят от
            // формата — пересобрать их надо здесь, а не в кадре отрисовки.
            self.rebuild_quality_note();
            self.remember();
        }
    }

    /// Переключатель качества: шесть ступеней в одной дорожке.
    ///
    /// Ширину сегмента считаем на каждом шаге заново, а не делим доступную
    /// один раз: шесть округлений до пиксельной сетки накопили бы ошибку,
    /// и дорожка либо не дотянулась бы до правого края, либо вылезла за него.
    /// Так каждому достаётся честная доля остатка, а последнему — ровно то,
    /// что осталось.
    fn quality_selector(&mut self, ui: &mut egui::Ui) {
        let format = self.format;
        let mut changed = false;

        egui::Frame::new()
            .fill(theme::BG_INPUT)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_STRONG))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS))
            .inner_margin(egui::Margin::same(3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    const GAP: f32 = 3.0;
                    ui.spacing_mut().item_spacing.x = GAP;

                    let mut left = Quality::ALL.len() as f32;
                    for quality in Quality::ALL {
                        let width = (ui.available_width() - GAP * (left - 1.0)) / left;
                        left -= 1.0;

                        if segment_button(ui, quality.label(format), self.quality == quality, width)
                        {
                            self.quality = quality;
                            changed = true;
                        }
                    }
                });
            });

        if changed {
            self.rebuild_quality_note();
            self.remember();
        }

        if !self.quality_note.is_empty() {
            ui.add_space(6.0);
            note(ui, &self.quality_note, theme::TEXT_SECONDARY);
        }
    }

    /// Два поля «с» и «до»: какой кусок ролика нужен.
    ///
    /// Разбирает набранное домен (`parse_section`), и только по правке текста:
    /// в кадре отрисовки здесь не считается ничего, включая сообщение об
    /// ошибке — оно статическое и лежит в `SectionError`.
    fn section_row(&mut self, ui: &mut egui::Ui) {
        let error = self.section_error;
        let mut changed = false;

        ui.horizontal(|ui| {
            // Ширину делим между двумя полями и тире между ними, а второму
            // отдаём весь остаток, а не такую же половину: два округления до
            // пиксельной сетки не дотянули бы ряд до правого края — та же
            // причина, что и в `quality_selector`.
            const DASH: f32 = 12.0;
            let spacing = ui.spacing().item_spacing.x;
            let width = ((ui.available_width() - DASH - spacing * 2.0) / 2.0).max(40.0);

            changed |= time_field(
                ui,
                &mut self.section_start,
                "с 0:00",
                error.is_some_and(SectionError::at_start),
                width,
            );
            ui.add_sized(
                [DASH, theme::CONTROL_HEIGHT],
                egui::Label::new(egui::RichText::new("—").color(theme::TEXT_MUTED)),
            );
            let rest = ui.available_width();
            changed |= time_field(
                ui,
                &mut self.section_end,
                "до конца",
                error.is_some_and(SectionError::at_end),
                rest,
            );
        });

        if changed {
            match parse_section(&self.section_start, &self.section_end) {
                Ok(section) => {
                    self.section = section;
                    self.section_error = None;
                }
                Err(err) => {
                    // Пока диапазон не разобран, не просим ничего. Кнопка всё
                    // равно выключена (`can_start`), но оставить здесь прошлое
                    // значение значило бы однажды вырезать не тот кусок.
                    self.section = Section::default();
                    self.section_error = Some(err);
                }
            }
        }

        // Оговорка есть всегда, как и у списка браузеров: у пустых полей она
        // объясняет, что писать, у заполненных — чем обрезка отличается от
        // обычной загрузки. Все строки статические.
        ui.add_space(6.0);
        if let Some(err) = self.section_error {
            note(ui, err.message(), theme::STATE_ERROR);
        } else if !self.section.any() {
            note(
                ui,
                "Пусто — ролик скачается целиком. Время можно писать как «90», \
                 «1:30» или «1:02:03».",
                theme::TEXT_MUTED,
            );
        } else if self.ffmpeg_missing {
            note(
                ui,
                "Вырезать нечем: ffmpeg не найден. Ролик скачается целиком.",
                theme::STATE_WARNING,
            );
        } else {
            note(
                ui,
                "Фрагмент вырезает ffmpeg прямо по ходу загрузки: она идёт \
                 заметно медленнее обычной, а проценты и скорость при этом \
                 не показываются. У MP4 начало сдвигается к ближайшему \
                 ключевому кадру — файл может начаться на секунду-другую \
                 раньше запрошенного.",
                theme::TEXT_MUTED,
            );
        }
    }

    /// Три галочки «вшить в файл».
    ///
    /// Столбиком, а не в строку: подписи русские и длинные, а в горизонтальной
    /// раскладке egui берёт для текста режим `Extend` — три штуки подряд в окне
    /// шириной 520 ушли бы за правую кромку. Столбик переносится сам.
    fn embed_options(&mut self, ui: &mut egui::Ui) {
        // Субтитры бывают только у видео: в MP3 их положить некуда. Галочку
        // гасим, но причину говорим по наведению — молча выключенный элемент
        // выглядит поломкой, а не запретом.
        let subs_enabled = self.format == Format::Mp4;

        ui.scope(|ui| {
            // Штатная строка виджета — 32 точки (высота поля ввода). Для галочки
            // это много: три подряд съели бы четверть окна минимальной высоты.
            ui.spacing_mut().interact_size.y = 24.0;

            let v = ui.visuals_mut();
            // `noninteractive` в списке не для полноты: выключенную галочку
            // egui рисует именно им, и без него она осталась бы с чужим
            // скруглением, то есть кружком-радиокнопкой.
            for state in [
                &mut v.widgets.noninteractive,
                &mut v.widgets.inactive,
                &mut v.widgets.hovered,
                &mut v.widgets.active,
            ] {
                // Галочку egui рисует цветом `fg_stroke` — тем же, каким красит
                // подпись рядом. Акцент нужен только самой галочке (в покое
                // коробка пуста, и без цвета выбранное не отличить от
                // невыбранного), поэтому подписям цвет задаётся отдельно,
                // через `RichText`: он перебивает цвет по умолчанию.
                state.fg_stroke = egui::Stroke::new(1.6, theme::ACCENT);
                state.corner_radius = egui::CornerRadius::same(theme::RADIUS_TINY);
                state.expansion = 0.0;
            }
            // Коробка «утоплена», как поле ввода и дорожка переключателя: на
            // заливке карточки она иначе держится на одной тонкой рамке.
            v.widgets.inactive.bg_fill = theme::BG_INPUT;

            checkbox(
                ui,
                &mut self.options.embed_metadata,
                "Метаданные: название, автор, дата",
                true,
            );
            checkbox(ui, &mut self.options.embed_thumbnail, "Обложку ролика", true);
            checkbox(ui, &mut self.options.embed_subs, "Субтитры", subs_enabled)
                .on_disabled_hover_text("Субтитры бывают только у видео — выберите MP4.");
        });

        // Обе оговорки ниже — статические строки: в кадре ничего не собирается.
        if self.ffmpeg_missing && self.options.any() {
            ui.add_space(6.0);
            note(
                ui,
                "Вшивать нечем: ffmpeg не найден. Файл скачается, но без \
                 метаданных, обложки и субтитров.",
                theme::STATE_WARNING,
            );
        }

        // Говорим только тогда, когда знаем наверняка: `probe` уже ответил,
        // и собственных субтитров у ролика нет. Молчание здесь честнее догадки.
        if self.options.embed_subs
            && subs_enabled
            && self.info.as_ref().is_some_and(|info| !info.has_subtitles)
        {
            ui.add_space(6.0);
            note(
                ui,
                "У этого ролика нет своих субтитров — вшивать нечего. \
                 Автоматические Savio не берёт: их пишет робот, и в них ошибки.",
                theme::TEXT_SECONDARY,
            );
        }
    }

    /// Выпадающий список «взять вход из браузера».
    ///
    /// Список, а не поле ввода: имена браузеров принадлежат yt-dlp, их список
    /// закрытый, и опечатка в нём обернулась бы английской руганью вместо
    /// загрузки.
    ///
    /// Оговорка под ним меняется вместе с выбором и в обоих случаях статична —
    /// в кадре отрисовки здесь ничего не собирается.
    fn cookie_selector(&mut self, ui: &mut egui::Ui) {
        // Ширину берём до `ComboBox`: внутри он заводит свою горизонтальную
        // раскладку, и `available_width` там уже другая.
        let width = ui.available_width();

        ui.scope(|ui| {
            let v = ui.visuals_mut();
            // Список — такое же поле ввода, как ссылка и дорожки
            // переключателей, поэтому и «утоплен» глубже карточки. Иначе на
            // заливке `BG_SURFACE` он держался бы на одной тонкой рамке.
            // `open` в списке обязателен: пока раскрыт список, egui рисует
            // кнопку именно этим состоянием, и без него она бы перекрашивалась
            // в момент нажатия.
            for state in [&mut v.widgets.inactive, &mut v.widgets.open] {
                state.weak_bg_fill = theme::BG_INPUT;
            }
            v.widgets.hovered.weak_bg_fill = theme::BG_ELEVATED;

            egui::ComboBox::from_id_salt("savio-cookies")
                .selected_text(self.cookies.label())
                .width(width)
                // Список обязан помещаться целиком. У egui потолок раскрытого
                // списка — `combo_height`, то есть 200 точек: при штатной
                // строке в 32 точки туда влезает пять пунктов из восьми,
                // а остальные три уезжают в прокрутку, полосы которой в покое
                // не видно. В окне минимального размера это выглядит так,
                // будто браузеров всего пять.
                .height(COOKIE_LIST_HEIGHT)
                .show_ui(ui, |ui| {
                    // Строки в списке плотнее, чем кнопки на экране: восемь
                    // штатных строк не поместились бы и в окно минимальной
                    // высоты (420), а для меню 26 точек — обычный размер.
                    let spacing = ui.spacing_mut();
                    spacing.interact_size.y = 26.0;
                    spacing.button_padding.y = 3.0;
                    spacing.item_spacing.y = 2.0;

                    for source in CookieSource::ALL {
                        ui.selectable_value(&mut self.cookies, source, source.label());
                    }
                });
        });

        ui.add_space(6.0);
        if self.cookies.browser().is_some() {
            note(
                ui,
                "Закройте браузер перед загрузкой: пока он открыт, файл cookies \
                 занят и не читается. И учтите: у YouTube cookies чаще мешают — \
                 сайт отвечает пустым списком дорожек. Перестало скачиваться — \
                 верните «Не использовать».",
                theme::STATE_WARNING,
            );
        } else {
            note(
                ui,
                "Для возрастных, приватных и «подтвердите, что вы не робот» \
                 роликов: Savio возьмёт из браузера ваш вход на сайт. Обычные \
                 ссылки скачиваются и без этого.",
                theme::TEXT_MUTED,
            );
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
                self.remember();
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

        // Обложка идёт под названием, а не над плашкой состояния: плашка
        // с названием — это заголовок блока, и картинка над ним оторвала бы
        // его от того, к чему он относится.
        //
        // В кадре здесь ничего не считается и не выделяется: текстура готова,
        // а `Image` — это две пары чисел.
        if let Some(cover) = &self.thumbnail {
            ui.add_space(10.0);
            ui.add(
                egui::Image::new(cover)
                    // `max_size`, а не `max_width`: у вертикальных роликов
                    // ограничивать надо высоту (см. `PREVIEW_MAX_HEIGHT`),
                    // а пропорцию egui сохраняет сам. Ширину окна учитывать
                    // отдельно не нужно — по умолчанию картинка вписывается
                    // в доступное место и лишь потом упирается в этот потолок.
                    .max_size(egui::vec2(PREVIEW_WIDTH, PREVIEW_MAX_HEIGHT))
                    .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL)),
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
            // Журнал очистили перед новой загрузкой — старое подтверждение
            // относилось бы уже не к нему.
            self.log_copied_at = None;
            return;
        }

        ui.add_space(14.0);
        egui::CollapsingHeader::new(
            egui::RichText::new("Журнал")
                .small()
                .color(theme::TEXT_SECONDARY),
        )
        .show(ui, |ui| {
            self.log_copy_row(ui);
            ui.add_space(8.0);

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

    /// Кнопка «Скопировать» над телом журнала.
    ///
    /// Строки журнала рисуются метками, а не полем ввода, и мышью не
    /// выделяются: без кнопки просьба «пришлите журнал» упирается в то, что
    /// переписывать его вручную никто не станет. Кнопка ничего не разбирает
    /// и не запускает — берёт уже готовые строки, поэтому и работы с потоками
    /// здесь нет.
    fn log_copy_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let now = ui.input(|i| i.time);

            let copied = ui
                .add(
                    egui::Button::new("Скопировать")
                        .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                )
                .on_hover_text(
                    "Журнал уйдёт в буфер обмена — его можно вставить \
                     в сообщение о проблеме.",
                )
                .clicked();

            if copied {
                // Единственная аллокация во всей кнопке, и происходит она по
                // нажатию, а не в кадре: `join` выделяет буфер один раз, сразу
                // нужного размера. Журнал уже ограничен `LOG_LIMIT`, так что
                // размер строки предсказуем.
                ui.ctx().copy_text(self.log.join("\n"));
                self.log_copied_at = Some(now);
            }

            if let Some(at) = self.log_copied_at {
                let left = COPIED_NOTICE_SECS - (now - at);
                if left > 0.0 {
                    // 10.7:1 на `BG_ROOT` — порог 4.5:1 проходит с запасом.
                    ui.label(
                        egui::RichText::new("Скопировано")
                            .small()
                            .color(theme::STATE_SUCCESS),
                    );
                    // Кадр к сроку приходится просить: без ввода egui окно не
                    // перерисовывает, и подпись висела бы до первого движения
                    // мыши — то есть заметно дольше положенного.
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs_f64(left));
                } else {
                    self.log_copied_at = None;
                }
            }
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
// Вкладка «История»
// ---------------------------------------------------------------------------

impl SavioApp {
    /// Список скачанного за этот запуск.
    ///
    /// `&self`, а не `&mut self`: вкладка ничего не меняет — она только
    /// показывает уже собранные строки и открывает папку.
    fn history_tab(&self, ui: &mut egui::Ui) {
        let Some((first, rest)) = self.history.entries.split_first() else {
            // Пустой экран без объяснения читается как поломка. Про то, что
            // список не переживает закрытие окна, говорим здесь же: иначе
            // после перезапуска пустая вкладка выглядит потерянными данными.
            note(
                ui,
                "Пока пусто. Сюда попадёт всё, что вы скачаете за этот запуск, — \
                 с кнопкой, открывающей папку файла. На диск список не пишется \
                 и при закрытии Savio очищается.",
                theme::TEXT_MUTED,
            );
            return;
        };

        self.history_card(ui, first);
        for entry in rest {
            ui.add_space(8.0);
            self.history_card(ui, entry);
        }
    }

    /// Одна строка истории.
    ///
    /// Карточка на каждую запись, а не одна на весь список: строки отделяются
    /// друг от друга сами, без разделителей, и список любой длины выглядит
    /// одинаково. Своей прокрутки здесь нет — вкладка целиком лежит в общей,
    /// и вложенная полоса рядом с внешней только мешала бы.
    fn history_card(&self, ui: &mut egui::Ui, entry: &HistoryEntry) {
        egui::Frame::new()
            .fill(theme::BG_SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                // Без этого карточка сжалась бы по ширине имени файла: у
                // короткого имени получилась бы узкая полоска посреди окна.
                ui.set_width(ui.available_width());

                // Имя длинное почти всегда (yt-dlp кладёт в него название
                // ролика целиком) — обрезаем, полное показывается по наведению.
                //
                // `on_hover_text` для этого звать НЕ надо, хотя рука тянется:
                // у `Label` есть `show_tooltip_when_elided`, по умолчанию
                // включённый, и обрезанная метка сама вешает подсказку с
                // полным текстом. Свой вызов её не заменяет, а добавляет
                // вторую: egui считает подсказки на виджет и ставит их одна
                // под другой — выходит две коробки с одним и тем же именем.
                // Ни сборка, ни тесты этого не видят.
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&entry.name)
                            .strong()
                            .color(theme::TEXT_PRIMARY),
                    )
                    .truncate(),
                );

                // Папки нет — значит, и открывать нечего: показываем только имя.
                let Some(dir) = &entry.dir else {
                    return;
                };

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // Кнопка слева, путь справа — как в «Папке сохранения» на
                    // соседней вкладке: одинаковые по смыслу пары должны
                    // выглядеть одинаково.
                    if ui
                        .add(
                            egui::Button::new("Открыть папку")
                                .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                        )
                        .clicked()
                    {
                        // Лежит ли файл на месте, не спрашиваем: это обращение
                        // к диску, а `ui()` идёт 60 раз в секунду (Правило 1).
                        // Папку могли переименовать или унести вместе с
                        // флешкой — тогда об этом скажет проводник, и это
                        // честнее выключенной без объяснения кнопки.
                        open_dir(dir);
                    }

                    // Подсказку с полным путём, как и у имени выше, вешает
                    // сама обрезанная метка.
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&entry.dir_display)
                                .small()
                                .color(theme::TEXT_SECONDARY),
                        )
                        .truncate(),
                    );
                });
            });
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
        // Поля кнопки урезаем: `width` для сегмента — это минимум, а не
        // потолок. egui не сжимает кнопку под доступное место, а раздвигает
        // раскладку, поэтому при штатных 14 точках с каждой стороны шесть
        // сегментов качества («2160p») в окне шириной 520 вылезли бы за
        // кромку. Подпись всё равно стоит по центру выделенной ширины, так
        // что на широких сегментах — вкладках и формате — разницы не видно.
        ui.spacing_mut().button_padding.x = 6.0;

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

/// Одна галочка «вшить в файл».
///
/// Цвет подписи задаём явно и не полагаемся на стиль: egui красит текст
/// флажка тем же `fg_stroke`, которым рисует саму галочку, а он у нас
/// акцентный — иначе весь список подписей стал бы жёлтым.
fn checkbox(
    ui: &mut egui::Ui,
    checked: &mut bool,
    label: &'static str,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Checkbox::new(
            checked,
            egui::RichText::new(label).color(theme::TEXT_PRIMARY),
        ),
    )
}

/// Красная рамка у поля, в котором ошиблись: в покое, при наведении и в фокусе.
///
/// `selection.stroke` в списке не для полноты — это и есть рамка поля, когда
/// в нём стоит курсор. Без неё поле краснеет ровно до того мгновения, когда
/// человек начинает его исправлять.
///
/// Одна функция на все поля ввода: разъехавшись, две одинаковые по смыслу
/// ошибки выглядели бы по-разному.
fn mark_invalid(ui: &mut egui::Ui) {
    let v = ui.visuals_mut();
    let error = egui::Stroke::new(1.0, theme::STATE_ERROR);
    v.widgets.inactive.bg_stroke = error;
    v.widgets.hovered.bg_stroke = error;
    v.widgets.active.bg_stroke = error;
    v.selection.stroke = error;
}

/// Поле ввода времени: «1:30», «1:02:03» или число секунд.
///
/// Возвращает `true`, когда текст правили: разбирать строку в кадре отрисовки
/// незачем, это работа обработчика изменения.
fn time_field(
    ui: &mut egui::Ui,
    value: &mut String,
    hint: &'static str,
    invalid: bool,
    width: f32,
) -> bool {
    ui.scope(|ui| {
        if invalid {
            mark_invalid(ui);
        }

        ui.add_sized(
            [width, theme::CONTROL_HEIGHT],
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .text_color(theme::TEXT_PRIMARY)
                .margin(egui::Margin::symmetric(10, 6)),
        )
        .changed()
    })
    .inner
}

/// Мелкая оговорка под элементом управления.
///
/// `wrap()` обязателен: без него длинная строка ушла бы за кромку окна
/// и растянула бы содержимое прокрутки — см. комментарий в `banner`.
fn note(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.add(egui::Label::new(egui::RichText::new(text).small().color(color)).wrap());
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
                // `wrap()` обязателен: в горизонтальной раскладке egui берёт
                // для текста режим `Extend`, то есть кладёт абзац в одну
                // строку любой длины и молча срезает её кромкой окна. Ни
                // сборка, ни тесты этого не видят, а объяснения в баннерах —
                // длинные: без переноса читалась бы только их первая треть.
                ui.add(egui::Label::new(egui::RichText::new(text).color(color)).wrap());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Потолок обязателен: по одной ссылке-плейлисту `Event::Done` приходит
    /// столько раз, сколько в нём роликов, и без обрезки список рос бы вместе
    /// с памятью — ровно то, от чего защищает `LOG_LIMIT` у журнала.
    #[test]
    fn history_stays_bounded() {
        let mut history = History::default();
        for i in 0..HISTORY_LIMIT + 10 {
            let path = format!("/dl/{i}.mp4");
            history.remember(Path::new(&path));
        }

        assert_eq!(history.entries.len(), HISTORY_LIMIT);
        // Выбрасывается самое старое, а не самое свежее.
        let newest = format!("{}.mp4", HISTORY_LIMIT + 9);
        assert_eq!(history.entries[0].name, newest);
        assert!(
            history.entries.iter().all(|entry| entry.name != "0.mp4"),
            "самая старая запись обязана уйти первой"
        );
    }

    /// Сверху — последнее скачанное: за ним возвращаются чаще всего.
    #[test]
    fn newest_download_comes_first() {
        let mut history = History::default();
        history.remember(Path::new("/dl/первый.mp4"));
        history.remember(Path::new("/dl/второй.mp3"));

        assert_eq!(history.entries[0].name, "второй.mp3");
        assert_eq!(history.entries[1].name, "первый.mp4");
    }

    /// Повторная загрузка того же файла (выбрали не тот формат, оборвалась
    /// связь) не должна плодить одинаковые строки подряд.
    #[test]
    fn repeat_moves_the_entry_up_instead_of_duplicating_it() {
        let mut history = History::default();
        history.remember(Path::new("/dl/a.mp4"));
        history.remember(Path::new("/dl/b.mp4"));
        history.remember(Path::new("/dl/a.mp4"));

        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].name, "a.mp4");
        assert_eq!(history.entries[1].name, "b.mp4");
    }

    #[test]
    fn entry_splits_the_path_into_name_and_folder() {
        let mut history = History::default();
        history.remember(Path::new("/dl/Ролик [id].mp4"));

        let entry = &history.entries[0];
        assert_eq!(entry.name, "Ролик [id].mp4");
        assert_eq!(entry.dir.as_deref(), Some(Path::new("/dl")));
        assert!(!entry.dir_display.is_empty());
    }

    /// От yt-dlp приходят абсолютные пути, но UI не должен зависеть от их
    /// формы: у имени без папки `parent()` возвращает не `None`, а `Some("")`,
    /// и открывать такую «папку» нельзя — кнопки в этой строке не будет.
    #[test]
    fn entry_without_a_folder_has_nothing_to_open() {
        let mut history = History::default();
        history.remember(Path::new("file.mp4"));

        let entry = &history.entries[0];
        assert_eq!(entry.name, "file.mp4");
        assert_eq!(entry.dir, None);
        assert!(entry.dir_display.is_empty());
    }
}
