//! Состояние и отрисовка интерфейса.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::engine::settings;
use crate::engine::setup;
use crate::engine::{self, Handle, MetaTask, metadata};
use crate::engine::monitor;
use crate::engine::power;
use crate::model::{
    BALANCED_PLAN, CheckStatus, CookieSource, DownloadId, DownloadOptions, Event, Format, GpuInfo,
    MediaInfo, Metric, PerfSample, PowerMode, PowerModes, PowerState, Progress, Quality, Request,
    Section, SectionError, SubLang, SystemReport, TRACE_LIMIT, Tag, Thumbnail, Trace, human_bytes,
    human_duration, human_speed, looks_like_url, meta_kind, parse_section,
};
use crate::theme;

const LOG_LIMIT: usize = 400;

/// Высота тела журнала в подвале.
const LOG_HEIGHT: f32 = 150.0;

/// Сколько сообщений от wgpu держим до того, как их разберёт кадр.
///
/// Потолок обязателен по Правилу 1, как `LOG_LIMIT` у журнала: пока окно
/// остаётся слишком большим, ошибка приходит на каждую попытку настроить
/// поверхность, то есть много раз в секунду и без конца.
const GPU_ERROR_LIMIT: usize = 4;

/// Ошибки wgpu, пойманные вместо падения процесса.
///
/// Клиентская часть окна — это текстура, и egui-wgpu просит у устройства ровно
/// 8192 пикселя по стороне жёстко зашитым числом, а поверхность настраивает
/// сырым размером окна, ничем его не ограничивая. Окно больше этого предела —
/// и `Surface::configure` отдаёт ошибку проверки, которую по умолчанию никто не
/// ловит: wgpu роняет процесс паникой. Программный размер окна приходит откуда
/// угодно — от оконного менеджера, удалённого рабочего стола, смены
/// разрешения, — и предотвратить его приложение не может: `max_inner_size`
/// система спрашивает только при перетаскивании рамки. Значит, остаётся
/// пережить: с этим обработчиком процесс остаётся жив, старая цепочка кадров
/// продолжает работать (wgpu-core выходит из `configure` до подмены
/// поверхности), а когда размер снова становится законным, рисование
/// восстанавливается само. Идущая загрузка при этом не теряется — ровно то,
/// ради чего всё и делается.
///
/// Проверено вживую: без обработчика окно с клиентом 504×8193 убивает процесс,
/// с ним — приложение живо и после возврата обычного размера рисует дальше.
#[derive(Default)]
struct GpuErrors {
    /// Взводится обработчиком, гасится кадром. Нужен, чтобы в обычном кадре
    /// (а ошибок не бывает почти никогда) дело не доходило до мьютекса:
    /// `ui()` зовут 60 раз в секунду.
    pending: AtomicBool,
    messages: Mutex<Vec<String>>,
}

impl GpuErrors {
    /// Зовётся из обработчика wgpu, то есть посреди чужого кода. Здесь нельзя
    /// ни паниковать, ни трогать состояние приложения — только отложить.
    fn push(&self, message: String) {
        let Ok(mut messages) = self.messages.lock() else {
            return;
        };
        // Пока окно остаётся большим, приходит одно и то же сообщение. Писать
        // его в журнал десятками одинаковых строк — значит вытеснить оттуда
        // всё остальное: у журнала свой предел, и он общий.
        if messages.last().is_some_and(|last| *last == message) {
            return;
        }
        if messages.len() < GPU_ERROR_LIMIT {
            messages.push(message);
            self.pending.store(true, Ordering::Release);
        }
    }

    /// Забирает накопленное. Пусто в подавляющем большинстве кадров, и тогда
    /// стоит одного атомарного чтения.
    fn take(&self) -> Vec<String> {
        if !self.pending.swap(false, Ordering::Acquire) {
            return Vec::new();
        }
        match self.messages.lock() {
            Ok(mut messages) => std::mem::take(&mut *messages),
            Err(_) => Vec::new(),
        }
    }
}

/// Приметы, по которым ошибка wgpu опознаётся как «окно больше, чем умеет
/// отрисовать видеокарта».
///
/// Подстроки английские и принадлежат wgpu — они могут смениться с новой
/// версией, и промах не поймает ни компилятор, ни тест (Правило 6). Поэтому
/// объяснение — не единственный исход: не узнали причину, значит показываем
/// сырой текст, а не молчим.
const GPU_TOO_LARGE_MARKS: [&str; 2] = [
    "maximum supported texture size",
    "max_texture_dimension_2d",
];

/// Строка для журнала по сообщению wgpu.
fn gpu_error_line(message: &str) -> String {
    if GPU_TOO_LARGE_MARKS.iter().any(|mark| message.contains(mark)) {
        "Окно оказалось больше, чем может отрисовать видеокарта (предел — 8192 \
         точки по стороне). Картинка временно не обновляется; уменьшите окно, \
         и рисование восстановится. Загрузка при этом не прервана."
            .to_owned()
    } else {
        format!("Ошибка отрисовки: {message}")
    }
}

/// Ширина превью обложки в точках.
///
/// 128 — примерно четверть того, что остаётся от окна минимальной ширины
/// (520 минус поля дают 480): рядом с картинкой обязано помещаться название,
/// а сама она стоит теперь в карточке управления, между полем ссылки и
/// переключателем формата, — то есть отнимает высоту у всего остального.
/// Движок уменьшает обложку до 480 настоящих точек, так что на экране
/// с двойной плотностью превью остаётся резким.
const PREVIEW_WIDTH: f32 = 128.0;

/// Потолок высоты превью.
///
/// Нужен из-за вертикальных роликов: обложка 9:16 при ширине 128 заняла бы
/// 227 точек — половину окна минимальной высоты (420) на одну картинку.
/// С потолком такая просто становится узкой, а не выдавливает поля ввода
/// в прокрутку. 72 — высота обычной обложки 16:9 при ширине 128.
const PREVIEW_MAX_HEIGHT: f32 = 72.0;

/// Сколько ждать после правки ссылки, прежде чем спрашивать сайт.
///
/// Окно ожидания обязательно. Без него запрос уходил бы на каждый набранный
/// символ: вставка из буфера — это одно изменение, а ссылка, набранная руками
/// или правленная по частям, — десятки, и сайт получил бы десятки запросов
/// об одном ролике. Прямая дорога к «Sign in to confirm you're not a bot»,
/// заработанная на ровном месте и ровно тем, что должно было помогать.
const PREVIEW_DEBOUNCE: f64 = 0.8;

/// Потолок высоты раскрытого списка источников входа.
///
/// Считается от числа источников, а не подобран на глаз: добавится браузер —
/// список подрастёт сам, и никто не будет гадать, почему последний пункт
/// уехал в прокрутку. 28 — строка списка (26 точек) плюс промежуток (2),
/// 12 — поля рамки меню сверху и снизу.
const COOKIE_LIST_HEIGHT: f32 = CookieSource::ALL.len() as f32 * 28.0 + 12.0;

/// Что написано на кнопке файла cookies, пока файла нет.
///
/// В окне до этой надписи не дойти: пункт «Из файла…» сам открывает диалог,
/// а отказ от диалога возвращает прежний выбор (см. `cookie_selector`). Но
/// кнопка обязана говорить хоть что-то, и приглашение выбрать файл — ровно
/// то, что она делает по нажатию.
const PICK_COOKIE_FILE: &str = "Выбрать файл…";


/// Потолок высоты раскрытого списка языков субтитров.
///
/// Здесь, в отличие от списка браузеров, вместить всё нельзя и пытаться:
/// список приходит от источника, и у YouTube в автоматических субтитрах
/// полторы сотни языков (проверено на живом ответе `-J`). Значение выбрано
/// от окна, а не от числа пунктов: 240 точек — чуть больше половины окна
/// минимальной высоты (420), так что раскрытый список не накрывает экран
/// целиком и остаётся видно, к чему он относится.
const SUBLANG_LIST_HEIGHT: f32 = 240.0;


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
    /// В очереди есть ссылки, но ничего не качается: их поставили и ещё не
    /// запустили. Отдельно от `Idle`, потому что экран говорит разное:
    /// «вставьте ссылку» и «нажмите „Скачать“ — очередь пойдёт» — это
    /// два разных следующих шага.
    Queued,
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
    /// Обновление одного инструмента по кнопке. Отдельно от `Installing` ради
    /// подписи в модалке: «Установка зависимостей» при обновлении сбивала бы
    /// с толку — пользователь ничего не устанавливал. И с указанием, чего
    /// именно: тексты у обновления yt-dlp и ffmpeg разные не для красоты —
    /// одно занимает секунды, другое качает больше сотни мегабайт.
    Updating(setup::Component),
    /// Установка не удалась. Приложение всё равно открывается: без `yt-dlp`
    /// пользователь увидит привычную подсказку, что делать дальше.
    Failed(String),
}

impl Setup {
    /// Идёт ли работа с внешними инструментами прямо сейчас. Пока идёт,
    /// показана модалка и занят единственный канал событий.
    fn busy(&self) -> bool {
        matches!(self, Setup::Installing | Setup::Updating(_))
    }
}

/// Какой экран показан.
///
/// Вкладки, а не один длинный экран: в окне минимального размера (520×420)
/// загрузка и работа с метаданными вместе уехали бы в прокрутку целиком.
///
/// Их три, а не пять, и это выбор макета. Пять коротких подписей в одной
/// дорожке кончались тем, что «Метаданные» вставали впритык и шестой вкладке
/// места уже не оставалось. Теперь «Система» и «Монитор» — это подвкладки
/// «Машины» (там и там речь об одной и той же машине, только в разрезе
/// «сейчас» и «состав»), а «История» переехала в правую колонку экрана
/// загрузки, к очереди: обе про одни и те же ссылки, только в разное время.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Download,
    Metadata,
    Machine,
}

/// Какая половина вкладки «Машина» показана.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MachineTab {
    /// Что происходит с машиной прямо сейчас — бывшая вкладка «Монитор».
    Now,
    /// Из чего машина состоит и в каком она состоянии — бывшая «Система».
    Spec,
}

/// Что показано в правой колонке экрана загрузки.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RailTab {
    Queue,
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

// ---------------------------------------------------------------------------
// Очередь загрузок
// ---------------------------------------------------------------------------

/// Сколько ссылок помещается в очередь.
///
/// Потолок обязателен по Правилу 1, как `LOG_LIMIT` у журнала и
/// `HISTORY_LIMIT` у истории: без него список рос бы всё время работы окна.
/// 50 — столько же, сколько помнит история, и заведомо больше, чем ставят
/// в очередь за один заход.
const QUEUE_LIMIT: usize = 50;

/// Что происходит с элементом очереди.
///
/// `Failed` несёт текст причины, а не голый признак. Причина в сроке жизни:
/// журнал очищается перед каждой следующей загрузкой, и к тому времени, как
/// человек вернётся к ушедшей в ночь очереди, от объяснения не осталось бы
/// ничего. А узнать, почему не скачалась третья ссылка из десяти, — ровно то,
/// зачем в этот список потом смотрят.
#[derive(Clone, PartialEq, Eq, Debug)]
enum QueueStatus {
    /// Ждёт своей очереди.
    Waiting,
    /// Качается прямо сейчас. Такой элемент в списке не больше одного —
    /// это и есть правило «строго по одному» из `start_next`.
    Running,
    Done,
    Failed(String),
    /// Снята человеком: нажали «Отмена», пока она качалась. Не ошибка
    /// (Правило 2), поэтому и слово, и цвет у неё спокойные.
    Cancelled,
}

impl QueueStatus {
    /// Отработал ли элемент — всё равно, чем кончилось.
    ///
    /// По этому признаку очередь освобождает место под новые ссылки:
    /// выбрасывать можно только то, что уже отработало.
    fn finished(&self) -> bool {
        matches!(
            self,
            QueueStatus::Done | QueueStatus::Failed(_) | QueueStatus::Cancelled
        )
    }

    /// Слово для строки списка. Статическое: в кадре отрисовки ничего
    /// не собирается и не выделяется.
    fn label(&self) -> &'static str {
        match self {
            QueueStatus::Waiting => "Ожидает",
            QueueStatus::Running => "Качается",
            QueueStatus::Done => "Готово",
            QueueStatus::Failed(_) => "Ошибка",
            QueueStatus::Cancelled => "Снято",
        }
    }

    /// Цвет точки и подписи состояния.
    ///
    /// Все пары проверены на `BG_ELEVATED` (заливка строки списка) по
    /// WCAG 2.1: `ACCENT` — 11.07:1, `STATE_SUCCESS` — 8.91:1,
    /// `STATE_ERROR` — 5.61:1, `TEXT_SECONDARY` — 7.83:1,
    /// `TEXT_MUTED` — 5.12:1. Порог 4.5:1 проходят все.
    ///
    /// Цветом одним состояние не передаётся: рядом с точкой всегда стоит
    /// слово из `label()`.
    fn color(&self) -> egui::Color32 {
        match self {
            QueueStatus::Waiting => theme::TEXT_MUTED,
            QueueStatus::Running => theme::ACCENT,
            QueueStatus::Done => theme::STATE_SUCCESS,
            QueueStatus::Failed(_) => theme::STATE_ERROR,
            QueueStatus::Cancelled => theme::TEXT_SECONDARY,
        }
    }
}

/// Одна ссылка в очереди.
///
/// Запрос и папку держим снимком, а не подсматриваем текущий выбор на экране:
/// пока очередь идёт, человек волен переключить формат под следующую ссылку,
/// и уже поставленное от этого меняться не должно. Иначе десяток ссылок,
/// поставленных как MP3, доехал бы до диска как MP4 — и заметить это можно
/// было бы, только открыв файлы.
struct QueueItem {
    id: DownloadId,
    request: Request,
    out_dir: PathBuf,
    /// Первая строка списка: название ролика, а пока оно не приехало от
    /// `probe` — сама ссылка.
    title: String,
    /// Вторая строка: «состояние · формат · качество». Собирается при смене
    /// состояния, а не в кадре отрисовки (Правило 1).
    detail: String,
    /// Причина отказа, разложенная в одну строку. Пустая — отказа не было.
    error_line: String,
    status: QueueStatus,
    /// Ответ `probe`, если предпросмотр успел его получить до того, как ссылку
    /// поставили в очередь.
    ///
    /// Едет вместе со ссылкой, а не берётся из предпросмотра в момент запуска,
    /// по той же причине, по какой снимком держится сам запрос: пока очередь
    /// идёт, в поле давно другая ссылка, и ответ там про неё.
    known: Option<MediaInfo>,
}

impl QueueItem {
    /// Пересобирает строки, зависящие от состояния. Зовётся при его смене —
    /// в кадре отрисовки здесь не собирается ничего (Правило 1).
    fn rebuild_strings(&mut self) {
        let format = self.request.format;
        self.detail.clear();
        self.detail.push_str(self.status.label());
        self.detail.push_str(" · ");
        self.detail.push_str(format.short());
        self.detail.push_str(" · ");
        self.detail
            .push_str(self.request.quality.label_with_unit(format));

        // Причину раскладываем в одну строку, и это не косметика:
        // `explain_failure` отдаёт текст с переносами, а метка с `truncate()`
        // показывает ровно первую строку. Выходило «Ошибка (код 1):…» —
        // подпись, которая не говорит ничего. Проверено глазами.
        self.error_line.clear();
        if let QueueStatus::Failed(message) = &self.status {
            for (index, word) in message.split_whitespace().enumerate() {
                if index > 0 {
                    self.error_line.push(' ');
                }
                self.error_line.push_str(word);
            }
        }
    }
}

/// Очередь загрузок за текущий запуск.
///
/// Отдельный тип, а не голый `Vec` в `SavioApp`, — по той же причине, что и
/// у `History`: потолок и сводка обязаны пересчитываться в одном месте.
/// Забыть про них в одной из точек изменения значило бы либо съесть память,
/// либо показать вчерашние цифры — и ни того ни другого не увидят ни сборка,
/// ни `clippy`.
///
/// Живёт только в памяти и только до закрытия окна, как и история.
struct Queue {
    /// Порядок здесь — порядок загрузки: сверху вниз, строго по одной.
    items: Vec<QueueItem>,
    /// Номер следующей загрузки. Растёт и никогда не переиспользуется:
    /// на этом держится вся развязка событий (см. `model::DownloadId`).
    next_id: DownloadId,
    /// «Идёт: 1 · В очереди: 3 · Готово: 5» — собирается при изменении
    /// очереди, а не в кадре отрисовки.
    summary: String,
    /// Мест больше нет, и освободить нечем. Считается там же, где сводка:
    /// перебирать полсотни строк 60 раз в секунду ради одного `bool` незачем.
    full: bool,
}

impl Queue {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            // С единицы, а не с нуля: ноль занят под `NO_DOWNLOAD`.
            next_id: 1,
            summary: String::new(),
            full: false,
        }
    }

    /// Ставит ссылку в конец очереди и отдаёт её номер.
    ///
    /// `None` — места нет: все `QUEUE_LIMIT` ссылок ещё ждут своего часа.
    ///
    /// `known` — ответ предпросмотра по этой ссылке, если он есть. С ним
    /// строка списка сразу называется роликом, а не адресом: до появления
    /// предпросмотра название приезжало только с началом загрузки, то есть
    /// у девятой ссылки — через час после того, как её поставили.
    fn push(
        &mut self,
        request: Request,
        out_dir: PathBuf,
        known: Option<MediaInfo>,
    ) -> Option<DownloadId> {
        if !self.make_room() {
            return None;
        }

        let id = self.next_id;
        // Четыре миллиарда ссылок за один запуск недостижимы, но `+= 1`
        // в отладочной сборке паникует на переполнении, а `wrapping_add` нет.
        // `max(1)` держит ноль занятым под `NO_DOWNLOAD`.
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let mut item = QueueItem {
            id,
            title: known
                .as_ref()
                .and_then(|info| info.title.clone())
                .unwrap_or_else(|| request.url.clone()),
            detail: String::new(),
            error_line: String::new(),
            request,
            out_dir,
            status: QueueStatus::Waiting,
            known,
        };
        item.rebuild_strings();

        self.items.push(item);
        self.rebuild_summary();
        Some(id)
    }

    /// Освобождает место под новую ссылку.
    ///
    /// Выбрасывает самую старую отработавшую строку, а не самую старую вообще:
    /// ожидающая ссылка — это невыполненная просьба, и молча терять её нельзя.
    /// Не нашлось ни одной отработавшей — очередь и правда полна, и сказать
    /// об этом надо словами, а не молчаливым отказом.
    fn make_room(&mut self) -> bool {
        if self.items.len() < QUEUE_LIMIT {
            return true;
        }
        match self.items.iter().position(|item| item.status.finished()) {
            Some(index) => {
                self.items.remove(index);
                true
            }
            None => false,
        }
    }

    /// Что запускать следующим: номер, запрос, папка и готовый ответ `probe`.
    ///
    /// Отдаёт копии, а не ссылки: `engine::start` забирает `Request` во
    /// владение, а строка обязана остаться в списке — по ней рисуется
    /// состояние, и в неё же приходит исход.
    fn next_waiting(&self) -> Option<(DownloadId, Request, PathBuf, Option<MediaInfo>)> {
        let item = self
            .items
            .iter()
            .find(|item| item.status == QueueStatus::Waiting)?;
        Some((
            item.id,
            item.request.clone(),
            item.out_dir.clone(),
            item.known.clone(),
        ))
    }

    fn has_waiting(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.status == QueueStatus::Waiting)
    }

    /// Номер идущей загрузки. Такая в списке не больше одной.
    fn running_id(&self) -> Option<DownloadId> {
        self.items
            .iter()
            .find(|item| item.status == QueueStatus::Running)
            .map(|item| item.id)
    }

    /// Запрос идущей загрузки. Нужен, чтобы понять, про ту ли ссылку приехали
    /// её метаданные, — см. `Preview::is_about`.
    fn running_request(&self) -> Option<&Request> {
        self.items
            .iter()
            .find(|item| item.status == QueueStatus::Running)
            .map(|item| &item.request)
    }

    /// Идёт ли прямо сейчас загрузка с таким номером.
    ///
    /// Здесь и живёт вся развязка событий: у снятой секунду назад загрузки
    /// процесс ещё дописывает свой вывод, и её `Failed` пометил бы ошибкой
    /// уже следующий элемент очереди. Событию установки (`NO_DOWNLOAD`)
    /// эта проверка отвечает «нет» — номера с нуля не начинаются.
    fn is_running(&self, id: DownloadId) -> bool {
        self.items
            .iter()
            .any(|item| item.id == id && item.status == QueueStatus::Running)
    }

    fn set_status(&mut self, id: DownloadId, status: QueueStatus) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.status = status;
            item.rebuild_strings();
        }
        self.rebuild_summary();
    }

    /// Заменяет ссылку в строке названием ролика.
    ///
    /// До `probe` названия нет, и в списке стоит сама ссылка. Как только оно
    /// приезжает — меняем: десяток ссылок с одного сайта различается тремя
    /// символами в конце, а названия — с первого взгляда.
    fn set_title(&mut self, id: DownloadId, title: &str) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.title.clear();
            item.title.push_str(title);
        }
    }

    /// Убирает строку из списка.
    ///
    /// Идущую загрузку не трогает: её процесс продолжил бы качать, а показать
    /// исход стало бы негде. Кнопки «убрать» у неё и нет — это страховка на
    /// случай, если она там однажды появится. Останавливают загрузку
    /// «Отменой», и это другое действие.
    fn remove(&mut self, id: DownloadId) {
        self.items
            .retain(|item| item.id != id || item.status == QueueStatus::Running);
        self.rebuild_summary();
    }

    /// Убирает всё, кроме идущей загрузки, — по кнопке «Очистить».
    fn clear(&mut self) {
        self.items
            .retain(|item| item.status == QueueStatus::Running);
        self.rebuild_summary();
    }

    fn rebuild_summary(&mut self) {
        use std::fmt::Write as _;

        let (mut running, mut waiting, mut done, mut failed, mut cancelled) = (0, 0, 0, 0, 0);
        for item in &self.items {
            // Разбор по вариантам, а не `_`: появится состояние — компилятор
            // потребует решить, куда его считать.
            match &item.status {
                QueueStatus::Running => running += 1,
                QueueStatus::Waiting => waiting += 1,
                QueueStatus::Done => done += 1,
                QueueStatus::Failed(_) => failed += 1,
                QueueStatus::Cancelled => cancelled += 1,
            }
        }

        // Полна очередь ровно тогда, когда мест нет и освободить нечем:
        // отработавшую строку `make_room` выбросит сам.
        self.full = self.items.len() >= QUEUE_LIMIT && done + failed + cancelled == 0;

        // Пары «слово: число», а не «3 ждут»: у русских числительных
        // окончание зависит от последней цифры, и «1 ждут» с «2 готово»
        // бросались бы в глаза. Двоеточие снимает вопрос вовсе.
        self.summary.clear();
        for (name, count) in [
            ("Идёт", running),
            ("В очереди", waiting),
            ("Готово", done),
            ("Ошибок", failed),
            ("Снято", cancelled),
        ] {
            if count == 0 {
                continue;
            }
            if !self.summary.is_empty() {
                self.summary.push_str(" · ");
            }
            let _ = write!(self.summary, "{name}: {count}");
        }
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
                // Номер загрузки здесь всегда `NO_DOWNLOAD` и никого
                // не интересует: канал у метаданных свой, разводить нечего.
                Event::Failed { message, .. } => {
                    self.outcome = Some((message, theme::STATE_ERROR));
                    self.busy = false;
                }
                // Остальные варианты рождаются только загрузкой и установкой,
                // а у них свой приёмник. Пустая ветка вместо `_` — чтобы
                // компилятор и дальше ловил здесь новые варианты `Event`.
                Event::Info(_)
                | Event::Thumbnail(_)
                | Event::Progress(_)
                | Event::Log(_)
                | Event::Done { .. }
                | Event::Ready
                | Event::Warning(_)
                | Event::Notice(_)
                | Event::Versions(_)
                | Event::SystemReport(_)
                | Event::Power(_)
                | Event::Perf(_) => {}
            }
        }

        if disconnected {
            self.rx = None;
            self.busy = false;
        }
    }
}

/// Состояние вкладки «Система».
///
/// Устроена как `MetaPanel`: свой приёмник на каждый запуск, работа в потоке,
/// готовый результат приезжает событием. Общего канала с загрузкой здесь быть
/// не может — опрашивать железо можно и посреди скачивания.
struct SystemPanel {
    /// Готовый снимок. `None` — ещё не спрашивали или спрашиваем прямо сейчас.
    report: Option<SystemReport>,
    busy: bool,
    /// Чем занят движок прямо сейчас.
    stage: String,
    /// Спрашивали ли хоть раз. Нужно, чтобы опрос пошёл сам при первом
    /// открытии вкладки: пустой экран с одной кнопкой «Проверить» — лишний
    /// шаг там, где ответ всё равно нужен всегда.
    asked: bool,
    /// Итог последнего сохранения отчёта в файл.
    saved: Option<(String, egui::Color32)>,
    rx: Option<Receiver<Event>>,
}

impl SystemPanel {
    fn new() -> Self {
        Self {
            report: None,
            busy: false,
            stage: String::new(),
            asked: false,
            saved: None,
            rx: None,
        }
    }

    fn start(&mut self, gpu: Option<GpuInfo>, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        engine::hardware::start(gpu, tx, move || notify_ctx.request_repaint());

        self.rx = Some(rx);
        self.busy = true;
        self.asked = true;
        self.stage = "Запуск…".to_owned();
        // Прошлый снимок убираем сразу: показывать вчерашние числа рядом
        // с надписью «опрашиваю» — прямой повод их перепутать.
        self.report = None;
        self.saved = None;
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
                Event::SystemReport(report) => {
                    self.report = Some(report);
                    self.busy = false;
                }
                // Опрос железа отказать целиком не умеет: не ответивший
                // источник приезжает пунктом со статусом «нет данных», а не
                // ошибкой на весь отчёт. Ветка всё равно выписана — на случай,
                // если это когда-нибудь изменится.
                Event::Failed { .. } => self.busy = false,
                // Остальное ходит по чужим каналам. Перечислено явно, а не
                // через `_`, чтобы компилятор и дальше требовал разбирать
                // новые варианты `Event` во всех приёмниках.
                Event::Info(_)
                | Event::Thumbnail(_)
                | Event::Progress(_)
                | Event::Log(_)
                | Event::Done { .. }
                | Event::Ready
                | Event::Warning(_)
                | Event::Notice(_)
                | Event::Tags(_)
                | Event::Cleaned(_)
                | Event::Versions(_)
                | Event::Power(_)
                | Event::Perf(_) => {}
            }
        }

        if disconnected {
            self.rx = None;
            self.busy = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Питание
// ---------------------------------------------------------------------------

/// Состояние карточки «Питание».
///
/// Устроена как `SystemPanel`: свой приёмник на каждый запуск, работа в
/// потоке, готовый ответ приезжает событием. Отличие в том, когда спрашивают.
/// Снимок железа берут по кнопке, а питание перечитывается само — при каждом
/// открытии половины «Сейчас» и после каждого переключения. Иначе кнопки
/// показывали бы вчерашнее положение: схему и режим меняют и мимо Savio,
/// из параметров Windows.
struct PowerPanel {
    state: PowerState,
    /// Спрашивали ли хоть раз. До первого ответа кнопок нет вовсе — рисовать
    /// переключатель, не зная его положения, значит выдумать положение.
    asked: bool,
    /// Идёт чтение или переключение: кнопки на это время выключены.
    busy: bool,
    /// Открыта ли половина «Сейчас». По изменению этого флага и идёт
    /// перечитывание.
    open: bool,
    /// Итог последнего переключения: текст и цвет.
    outcome: Option<(String, egui::Color32)>,
    /// Оговорка под рядом режимов. Собирается на приёме события, а не в кадре
    /// отрисовки: `format!` в `ui()` — это аллокация шестьдесят раз в секунду
    /// ради строки, которая меняется раз в минуту (Правило 1).
    hint: String,
    rx: Option<Receiver<Event>>,
}

impl PowerPanel {
    fn new() -> Self {
        Self {
            state: PowerState::default(),
            asked: false,
            busy: false,
            open: false,
            outcome: None,
            hint: String::new(),
            rx: None,
        }
    }

    /// Перечитывает состояние, когда половину «Сейчас» открыли.
    ///
    /// Именно по открытию, а не каждую секунду вместе с замерами монитора:
    /// чтение стоит запуска потока, а меняется питание раз в день. И не
    /// однажды за весь запуск: половину открывают ровно тогда, когда хотят
    /// увидеть, что с машиной сейчас.
    fn watch(&mut self, open: bool, ctx: &egui::Context) {
        if open && !self.open {
            self.start(ctx);
        }
        self.open = open;
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        engine::power::start(tx, move || notify_ctx.request_repaint());

        self.rx = Some(rx);
        self.busy = true;
        self.asked = true;
    }

    /// Просит систему переключиться и перечитать состояние.
    fn change(&mut self, change: power::Change, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        engine::power::start_change(change, tx, move || notify_ctx.request_repaint());

        self.rx = Some(rx);
        self.busy = true;
        self.asked = true;
        // Прошлый итог относился к прошлому нажатию: оставить его рядом
        // с новым — прямой повод их перепутать.
        self.outcome = None;
    }

    /// Принимает свежее состояние и пересобирает оговорку под ним.
    fn accept(&mut self, state: PowerState) {
        self.hint = power_hint(&state);
        self.state = state;
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
                Event::Power(state) => {
                    self.accept(state);
                    self.busy = false;
                }
                // Переключилось и проверено перечитыванием.
                Event::Notice(text) => self.outcome = Some((text, theme::STATE_SUCCESS)),
                // Система приняла просьбу, но работает по-прежнему. Не ошибка
                // и не успех — свой цвет (Правило 6).
                Event::Warning(text) => self.outcome = Some((text, theme::STATE_WARNING)),
                Event::Failed { message, .. } => {
                    self.outcome = Some((message, theme::STATE_ERROR));
                    self.busy = false;
                }
                // Остальное ходит по чужим каналам. Перечислено явно, а не
                // через `_`, чтобы компилятор и дальше требовал разбирать
                // новые варианты `Event` во всех приёмниках.
                Event::Info(_)
                | Event::Thumbnail(_)
                | Event::Stage(_)
                | Event::Progress(_)
                | Event::Log(_)
                | Event::Done { .. }
                | Event::Ready
                | Event::Tags(_)
                | Event::Cleaned(_)
                | Event::Versions(_)
                | Event::SystemReport(_)
                | Event::Perf(_) => {}
            }
        }

        if disconnected {
            self.rx = None;
            self.busy = false;
        }
    }
}

/// Оговорка под рядом режимов питания: чего ждать от нажатия.
///
/// Свободная функция, а не метод панели, потому что она чистая: из состояния
/// делается строка, и ничего больше. Так её и проверяют тесты — а проверять
/// её надо, потому что это единственное место, где Savio предупреждает
/// о молчаливом отказе Windows **до** нажатия, а не после.
fn power_hint(state: &PowerState) -> String {
    let PowerModes::Known { effective, ignored } = state.modes else {
        return String::new();
    };

    // Название сбалансированной схемы берём из списка, а не пишем своё: на
    // английской Windows она называется «Balanced», и подменять её название
    // значило бы отправить человека искать в системе то, чего там нет.
    let balanced = state
        .plan_name(BALANCED_PLAN)
        .unwrap_or("Сбалансированная");
    // Как назвать активную схему: по имени, если система его дала, и
    // обезличенно, если нет. Пустое место вместо названия читалось бы как
    // недорисованная строка, а выдуманное имя — как чужая схема.
    let active = match state.active_name() {
        Some(name) => format!("«{name}»"),
        None => "другая схема".to_owned(),
    };

    if let Some(stored) = ignored {
        return format!(
            "Windows запомнила режим «{}», но машина работает в другом: {}. \
             Режим питания применяется только при схеме «{balanced}», \
             а сейчас активна {active}.",
            stored.label(),
            effective.map_or("система его не назвала", PowerMode::label),
        );
    }

    if state.active.is_some_and(|id| id != BALANCED_PLAN) {
        return format!(
            "Сейчас активна {active}, а режим питания Windows применяет только \
             при «{balanced}»: выбор она запомнит, но машина будет работать \
             по-прежнему."
        );
    }

    String::new()
}

// ---------------------------------------------------------------------------
// Монитор производительности
// ---------------------------------------------------------------------------

/// Размер окна оверлея.
///
/// Фиксированный, и окно неизменяемое: содержимое у него — четыре строки
/// известной длины, и растягивать его некуда. Заодно это снимает целый класс
/// бед из дефекта 13: у окна, которому нельзя менять размер, не бывает
/// клиентской части шире 8192 пикселей.
///
/// Ширина подобрана по самой длинной строке — «Приём 1.2 МБ/с · Отдача
/// 128.0 КБ/с». На 230 точках она обрезалась многоточием, то есть оверлей
/// показывал половину того, ради чего его открыли. Высота — под четыре
/// строки и заголовок ровно с теми отступами, что заданы в `overlay_ui`:
/// со штатными отступами темы (строка виджета 32 точки) четвёртая строка
/// не влезала и молча пропадала.
const OVERLAY_SIZE: [f32; 2] = [300.0, 122.0];

/// Состояние вкладки «Монитор».
///
/// Устроена как `SystemPanel`: свой приёмник, работа в потоке, готовый
/// результат приезжает событием. Отличие одно, и оно определяет всё
/// остальное: снимок системы спрашивают однажды, а монитор — каждую секунду,
/// пока на него смотрят. Отсюда и ручка опроса, и явный останов.
struct MonitorPanel {
    /// Последний замер. `None` — опрос только начался, первого замера ещё нет.
    sample: Option<PerfSample>,
    /// История загрузки процессора и памяти — для графиков.
    cpu_trace: Trace,
    mem_trace: Trace,
    rx: Option<Receiver<Event>>,
    /// Чем остановить опрос. `Some` — поток работает.
    handle: Option<monitor::Handle>,

    /// Показывать ли окно поверх остальных.
    overlay: bool,
    /// Пропускать ли щелчки мыши сквозь оверлей.
    ///
    /// Отдельная галочка, а не всегда включённый режим: с пропуском оверлей
    /// нельзя ни передвинуть, ни закрыть его же кнопкой — щелчок уходит
    /// в окно под ним. Поэтому по умолчанию выключен, а сказано об этом
    /// рядом с галочкой.
    passthrough: bool,
    /// Оверлей попросили закрыть из него самого.
    ///
    /// Через общий флаг, а не напрямую: рисует оверлей замыкание, которому
    /// до полей панели не дотянуться (`show_viewport_deferred` требует
    /// `Send + Sync + 'static`).
    overlay_closing: Arc<AtomicBool>,
    /// То, что рисует оверлей. По той же причине — через общую ячейку.
    overlay_sample: Arc<Mutex<Option<PerfSample>>>,
    /// Кто такой оверлей для egui.
    ///
    /// Считается один раз и хранится полем, а не пересчитывается по имени
    /// в каждом обращении: `ViewportId::from_hash_of` — это хеш строки, а
    /// зовут его несколько раз за кадр. Константой его не сделать: функция
    /// не `const`, а `ViewportId(Id::NULL)`, которым это тянет записать, —
    /// это `ViewportId::ROOT`, то есть само главное окно.
    overlay_id: egui::ViewportId,
}

impl MonitorPanel {
    fn new() -> Self {
        Self {
            sample: None,
            cpu_trace: Trace::default(),
            mem_trace: Trace::default(),
            rx: None,
            handle: None,
            overlay: false,
            passthrough: false,
            overlay_closing: Arc::new(AtomicBool::new(false)),
            overlay_sample: Arc::new(Mutex::new(None)),
            overlay_id: egui::ViewportId::from_hash_of("savio-overlay"),
        }
    }

    /// Идёт ли опрос прямо сейчас.
    fn running(&self) -> bool {
        self.handle.is_some()
    }

    /// Включает и выключает опрос по тому, смотрят ли на него.
    ///
    /// Это и есть ответ на главный риск задачи: Savio в покое не тратит ни
    /// кадра, а секундный опрос будит окно раз в секунду до конца дня. Пока
    /// открыта вкладка или включён оверлей — есть кому смотреть; во всех
    /// прочих случаях поток обязан остановиться, иначе загрузчик греет
    /// ноутбук в фоне.
    fn set_running(&mut self, wanted: bool, ctx: &egui::Context) {
        if wanted == self.running() {
            return;
        }
        if wanted {
            self.start(ctx);
        } else {
            self.stop();
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        self.handle = Some(monitor::start(tx, move || notify_ctx.request_repaint()));
        self.rx = Some(rx);

        // Прошлые числа и графики убираем: между двумя включениями монитора
        // проходит сколько угодно времени, и склеенный с сегодняшним вчерашний
        // график показал бы провал, которого не было.
        self.sample = None;
        self.cpu_trace.clear();
        self.mem_trace.clear();
    }

    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.stop();
        }
        // Приёмник бросаем вместе с ручкой — как это делает `Preview::stop`.
        // Замер, снятый в последний миг перед остановкой, уже не про то,
        // что показано, и лечь в окно он не должен.
        self.rx = None;
    }

    /// Забирает замеры, приехавшие с прошлого кадра.
    fn drain(&mut self, ctx: &egui::Context) {
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
                Event::Perf(sample) => self.accept(sample, ctx),
                // Остальное ходит по чужим каналам. Перечислено явно, а не
                // через `_`, чтобы компилятор и дальше требовал разбирать
                // новые варианты `Event` во всех приёмниках.
                Event::Info(_)
                | Event::Thumbnail(_)
                | Event::Stage(_)
                | Event::Progress(_)
                | Event::Log(_)
                | Event::Done { .. }
                | Event::Failed { .. }
                | Event::Ready
                | Event::Warning(_)
                | Event::Notice(_)
                | Event::Tags(_)
                | Event::Cleaned(_)
                | Event::Versions(_)
                | Event::Power(_)
                | Event::SystemReport(_) => {}
            }
        }

        if disconnected {
            // Поток кончился сам — например, приёмник умер раньше ручки.
            // Держать мёртвый канал и мёртвую ручку незачем: следующий кадр
            // заведёт опрос заново, если на него всё ещё смотрят.
            self.rx = None;
            self.handle = None;
        }
    }

    /// Принимает свежий замер.
    fn accept(&mut self, sample: PerfSample, ctx: &egui::Context) {
        // В график кладём только то, что система вправду сказала: `None`
        // здесь означает «показания нет», и подставить на его месте ноль
        // значило бы нарисовать провал загрузки, которого не было.
        if let Some(cpu) = sample.cpu.percent {
            self.cpu_trace.push(cpu);
        }
        if let Some(mem) = sample.mem.percent {
            self.mem_trace.push(mem);
        }

        if let Ok(mut slot) = self.overlay_sample.lock() {
            *slot = Some(sample.clone());
        }
        if self.overlay {
            // Оверлею кадр надо просить отдельно: `request_repaint` из потока
            // опроса будит главное окно, а дочернее окно egui само по себе
            // за родителем не перерисовывается — числа в нём просто застыли
            // бы до первого движения мышью над ним.
            ctx.request_repaint_of(self.overlay_id);
        }

        self.sample = Some(sample);
    }

    /// Рисует оверлей, пока он включён.
    ///
    /// Зовётся каждый кадр: egui держит дочернее окно ровно до тех пор, пока
    /// его просят на каждом проходе. Замыкание при этом собирается заново —
    /// так требует API (`Fn + Send + Sync + 'static`), и дешевле этого здесь
    /// ничего нет: сами данные лежат в общей ячейке и не копируются.
    fn show_overlay(&mut self, ctx: &egui::Context) {
        // Закрыли изнутри — гасим галочку и забываем просьбу: иначе окно,
        // открытое заново, тут же закрылось бы старым флагом.
        if self.overlay_closing.swap(false, Ordering::Relaxed) {
            self.overlay = false;
        }
        if !self.overlay {
            return;
        }

        let builder = egui::ViewportBuilder::default()
            .with_title("Savio — монитор")
            .with_inner_size(OVERLAY_SIZE)
            // Оба предела равны размеру: окно без рамки всё равно нечем
            // тянуть, а верхний предел заодно закрывает дорогу дефекту 13.
            .with_min_inner_size(OVERLAY_SIZE)
            .with_max_inner_size(OVERLAY_SIZE)
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top()
            // В панели задач оверлею делать нечего: это не второе приложение,
            // а полоска поверх игры.
            .with_taskbar(false)
            // Забирать фокус нельзя ни в коем случае: в игре это потеря
            // управления, а в полноэкранной игре — ещё и сворачивание.
            .with_active(false)
            .with_mouse_passthrough(self.passthrough);

        let sample = Arc::clone(&self.overlay_sample);
        let closing = Arc::clone(&self.overlay_closing);
        ctx.show_viewport_deferred(self.overlay_id, builder, move |ui, class| {
            overlay_ui(ui, class, &sample, &closing);
        });
    }
}

/// Что показывать под полем ссылки.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum PreviewState {
    /// Спрашивать не о чем: поле пустое или в нём не ссылка.
    #[default]
    Idle,
    /// Ждём — окна дебаунса или самого ответа.
    Asking,
    /// Ответ пришёл.
    Ready,
    /// Спросили, а ответа не будет.
    Failed,
}

/// Сведения о ссылке из поля, полученные до нажатия «Скачать».
///
/// Ради этого превью и задумывалось: убедиться, что в буфере оказалась та
/// самая ссылка, надо **до** загрузки, а не посреди неё. Заодно до неё же
/// становятся известны доступные высоты кадра и список языков субтитров —
/// то, из чего человек выбирает прямо здесь, в карточке управления.
///
/// Свой приёмник и своя ручка, отдельно от загрузки: спрашивать про новую
/// ссылку можно и во время скачивания (`MetaPanel` устроен так же), и события
/// двух задач не должны попадать в один сток.
#[derive(Default)]
struct Preview {
    /// Ссылка, о которой спрашиваем или уже спросили. Пусто — спрашивать
    /// нечего.
    url: String,
    /// Cookies, которыми спрашивали. Часть цели, а не мелочь: тот же адрес
    /// с cookies и без них отдаёт разные ответы (у YouTube — вплоть до
    /// пустого списка дорожек), и смена браузера в списке обязана запрос
    /// перезапустить.
    cookies: CookieSource,
    /// Каким файлом cookies спрашивали. Такая же часть цели, как и сам
    /// источник: два разных файла — это два разных входа в аккаунт, и ответы
    /// по ним отличаются ровно так же, как ответ Firefox от ответа без
    /// cookies. Без этого поля смена файла оставила бы на экране карточку,
    /// собранную по прежнему входу.
    cookie_file: Option<PathBuf>,
    /// Когда истекает окно дебаунса, по часам egui. `None` — ждать нечего:
    /// либо уже спросили, либо спрашивать не о чем.
    due: Option<f64>,
    /// Приёмник ответа. Свой на каждый запуск, и в этом вся развязка
    /// поколений: бросив его вместе со сменой цели, мы разом перестаём
    /// слышать устаревший запрос. Без этого ответ по прежней ссылке,
    /// вернувшийся позже нового, положил бы в окно чужое название — беда
    /// тихая, её не видят ни компилятор, ни сборка.
    rx: Option<Receiver<Event>>,
    /// Чем бросить сам процесс. Без него `-J` по забытой ссылке дочитывает
    /// медленный сайт до конца, и на десятке правок подряд таких процессов
    /// накапливается десяток.
    handle: Option<Handle>,
    /// Что ответили. Отдаётся загрузке (см. `Queue::push`), чтобы та
    /// не ходила на сайт за тем же самым второй раз.
    info: Option<MediaInfo>,
    /// Обложка ролика, уже залитая в текстуру egui.
    ///
    /// Именно текстура, а не байты: заводится она один раз, на приёме
    /// `Event::Thumbnail`, и в кадре отрисовки остаётся только нарисовать.
    /// Освобождает текстуру egui сам, когда ручку заменяют или бросают.
    thumbnail: Option<egui::TextureHandle>,
    state: PreviewState,
}

impl Preview {
    /// Нацеливает предпросмотр на ссылку из поля.
    ///
    /// Возвращает `true`, если цель сменилась: прежний ответ больше не про то,
    /// что в поле, и всё, что из него собрано (название, оговорка про высоту,
    /// список языков), показывать уже нельзя.
    fn retarget(
        &mut self,
        url: &str,
        cookies: CookieSource,
        cookie_file: Option<&Path>,
        now: f64,
    ) -> bool {
        if self.url == url && self.cookies == cookies && self.cookie_file.as_deref() == cookie_file
        {
            return false;
        }

        self.stop();
        self.cookies = cookies;
        self.cookie_file = cookie_file.map(Path::to_path_buf);
        // Мусор из буфера обмена в yt-dlp не отправляем. Список поддерживаемых
        // сайтов принадлежит ему, и кнопку «непохожая» ссылка не блокирует, —
        // но каждый такой запуск это процесс и поход в сеть, а обрывок текста
        // из буфера не стоит ни того ни другого.
        if looks_like_url(url) {
            self.url.push_str(url);
            self.due = Some(now + PREVIEW_DEBOUNCE);
            self.state = PreviewState::Asking;
        }
        true
    }

    /// Спрашивает заново про ту же ссылку — когда изменилось не то, что
    /// в поле, а то, чем спрашивают.
    fn retry(&mut self, now: f64) {
        if self.url.is_empty() {
            return;
        }
        self.due = Some(now + PREVIEW_DEBOUNCE);
        self.state = PreviewState::Asking;
    }

    /// Бросает начатое и забывает ответ.
    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.cancel();
        }
        self.rx = None;
        self.url.clear();
        self.due = None;
        self.info = None;
        self.thumbnail = None;
        self.state = PreviewState::Idle;
    }

    /// Запоминает запущенный запрос.
    fn asked(&mut self, rx: Receiver<Event>, handle: Handle) {
        self.due = None;
        self.rx = Some(rx);
        self.handle = Some(handle);
    }

    /// Забирает то, что успел ответить движок.
    ///
    /// Сначала собираем, потом применяем — как в `drain_events`: заимствование
    /// приёмника иначе живёт во время правки остального состояния.
    fn take_events(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(rx) = &self.rx else {
            return events;
        };

        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => return events,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // Поток кончился — слушать больше нечего. Ответа при этом могло и не
        // быть вовсе: про отсутствие yt-dlp, незнакомый сайт и молчащий сервер
        // предпросмотр не говорит ничего (см. `engine::start_probe`), так что
        // единственный признак неудачи — закрытый канал без `Info`.
        self.rx = None;
        self.handle = None;
        if self.state == PreviewState::Asking
            && self.info.is_none()
            && !events.iter().any(|event| matches!(event, Event::Info(_)))
        {
            self.state = PreviewState::Failed;
        }
        events
    }

    /// Заводит текстуру под приехавшую обложку.
    ///
    /// Единственная заливка текстуры за весь запрос — и она здесь, на приёме
    /// события, а не в кадре. Разбор картинки уже сделал движок: сюда
    /// приезжает готовый RGBA.
    ///
    /// Размеры проверяем, хотя движок это уже сделал: между ним и нами канал,
    /// а `ColorImage` на несовпадении не возвращает ошибку, а паникует.
    /// Уронить окно из-за украшения нельзя, поэтому проверка на обеих сторонах.
    fn set_cover(&mut self, cover: &Thumbnail, ctx: &egui::Context) {
        if !cover.is_valid() {
            return;
        }
        self.thumbnail = Some(ctx.load_texture(
            "savio-cover",
            egui::ColorImage::from_rgba_unmultiplied([cover.width, cover.height], &cover.rgba),
            egui::TextureOptions::LINEAR,
        ));
    }

    /// Про ту же ли ссылку этот запрос.
    ///
    /// Сверяем и адрес, и вход в аккаунт целиком — источник вместе с файлом:
    /// ответ на один и тот же адрес с чужим входом — это другой ответ.
    fn is_about(&self, request: Option<&Request>) -> bool {
        request.is_some_and(|request| {
            request.url == self.url
                && request.cookies == self.cookies
                && request.cookie_file == self.cookie_file
        })
    }

    /// Ответ, годный для этой загрузки, — чтобы та не спрашивала сайт второй
    /// раз о том же самом.
    ///
    /// Копия, а не перенос: ссылку из поля после «В очередь» обычно не стирают,
    /// и карточка под ним обязана остаться на месте.
    fn answer_for(&self, request: &Request) -> Option<MediaInfo> {
        self.is_about(Some(request))
            .then(|| self.info.clone())
            .flatten()
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
    /// На каком языке нужны субтитры.
    ///
    /// Отдельно от `options` по той же причине, что и `cookies`: там всё
    /// `Copy`, а язык — строка. Между запусками не запоминается: язык
    /// относится к конкретному ролику, и вчерашний «немецкий», молча
    /// применённый к сегодняшней ссылке, дал бы не те субтитры.
    sub_lang: SubLang,
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
    /// Выбранный файл cookies. Осмыслен только при `CookieSource::File`,
    /// но переживает переключение списка: вернувшись к «Из файла…», человек
    /// не должен искать тот же файл заново.
    ///
    /// Между запусками не запоминается по той же причине, что и сам источник,
    /// и по своей вдобавок: путь к такому файлу — это не настройка, а след
    /// того, чем человек занимался (задача 24 реестра).
    cookie_file: Option<PathBuf>,
    /// Путь к файлу cookies строкой — для кнопки под списком. Полем, а не
    /// сборкой в кадре: `ui()` идёт 60 раз в секунду, а меняется это по
    /// выбору файла. Пока файла нет — приглашение выбрать его, а не пустая
    /// строка: пустая кнопка не сказала бы ничего.
    cookie_file_display: String,
    /// ffmpeg не нашёлся при последней проверке. Снимок с запуска (и с конца
    /// установки) — единственное, что можно спросить, не трогая диск в кадре.
    /// Нужен, чтобы предупредить о бесполезных галочках **до** нажатия
    /// «Скачать»; окончательное слово всё равно за движком в момент запуска.
    ffmpeg_missing: bool,
    out_dir: Option<PathBuf>,
    state: State,
    progress: Progress,
    stage: String,
    /// Что за ролик лежит по ссылке из поля. Наполняется фоновым запросом
    /// по вставке ссылки, а не загрузкой: см. [`Preview`].
    preview: Preview,
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
    /// Приёмник ответа о версиях инструментов.
    ///
    /// Отдельный от `rx`: опрос версий идёт при запуске, когда общий канал ещё
    /// может быть занят установкой недостающего, — попади ответ туда, его
    /// разбирали бы вместе с событиями установки.
    versions_rx: Option<Receiver<Event>>,

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
    /// Подпись выбранного языка субтитров — то, что написано на закрытом
    /// списке. Готовой строкой, а не поиском по дорожкам в кадре: их
    /// у YouTube полторы сотни.
    sub_lang_label: String,
    /// Оговорка под списком языков: субтитров, которые просят, у ролика нет.
    /// Пустая строка — всё в порядке или сказать пока нечего.
    subs_note: String,
    /// Что стоит в свёрнутых тонких настройках — одной строкой в их заголовке.
    ///
    /// Готовой строкой, а не сборкой в кадре: `ui()` зовут 60 раз в секунду,
    /// а меняется она от щелчка. И она обязательна: свёрнутая группа без
    /// сводки прячет включённую обрезку, а человек потом ищет, почему ролик
    /// скачался куском.
    advanced_summary: String,
    /// Версии инструментов одной строкой для подвала.
    ///
    /// Одна, а не две: подвал — это горизонтальная полоса, и вторая строка
    /// удвоила бы её высоту на всех вкладках сразу. yt-dlp в ней стоит
    /// первым намеренно. Обрезается конец строки, а версия ffmpeg у
    /// git-сборки — это `N-125365-g9a01c1cb6a-20260630`, то есть ровно то,
    /// чем в узком окне можно пожертвовать; полностью её всё равно
    /// показывает подсказка обрезанной метки.
    tools_line: String,
    /// Ссылка не похожа на ссылку. Только подсветка поля — кнопку не блокирует.
    url_invalid: bool,
    /// Когда журнал скопировали, по часам egui. Нужно только для подписи
    /// «Скопировано»: она живёт `COPIED_NOTICE_SECS` и гаснет сама.
    log_copied_at: Option<f64>,
    /// Показанная вкладка.
    tab: Tab,
    /// Половина вкладки «Машина».
    machine_tab: MachineTab,
    /// Что показано в правой колонке экрана загрузки.
    rail_tab: RailTab,
    /// Раскрыты ли тонкие настройки: фрагмент, вход на сайт, язык субтитров.
    ///
    /// Свёрнуты по умолчанию, и это главное, ради чего они собраны вместе:
    /// в развёрнутом виде они занимали половину карточки у всех, а нужны
    /// далеко не каждому. Сводка в заголовке говорит, что там сейчас стоит, —
    /// без неё свёрнутая группа прятала бы включённую обрезку.
    advanced: bool,
    /// Раскрыт ли журнал в подвале.
    log_open: bool,
    /// Состояние вкладки «Метаданные».
    meta: MetaPanel,
    /// Состояние вкладки «Система».
    system: SystemPanel,
    /// Состояние вкладки «Монитор» и оверлея.
    monitor: MonitorPanel,
    /// Состояние карточки «Питание» на половине «Сейчас».
    power: PowerPanel,
    /// Чем eframe рисует это окно.
    ///
    /// Снимается один раз при создании приложения с того же адаптера, что уже
    /// открыт для отрисовки, — второй открывать незачем, и стоит это ноль.
    /// `get_info()` собирает четыре строки, так что в кадре его звать нельзя
    /// (Правило 1); здесь он вызван ровно один раз за запуск. `None` —
    /// сборка без wgpu: не ошибка, просто пункта про видеокарту не будет.
    gpu: Option<GpuInfo>,
    /// Что скачано за этот запуск. Наполняется из `Event::Done`.
    history: History,
    /// Ссылки, поставленные в очередь. Идут строго по одной, сверху вниз.
    queue: Queue,
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

    /// Сюда обработчик wgpu складывает то, из-за чего процесс раньше падал.
    /// Разделяется с обработчиком, поэтому `Arc`, а не поле по значению.
    gpu_errors: Arc<GpuErrors>,

    /// Сетка фона: три тёплых пятна и вуаль поверх.
    ///
    /// Полем, а не переменной кадра: сетка зависит только от размера окна,
    /// и пересобирать её шестьдесят раз в секунду было бы ровно той лишней
    /// работой, которой не велит Правило 1.
    backdrop: theme::Backdrop,
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
            // Галочки вшивания восстанавливаются как есть, **не** оглядываясь
            // на то, нашёлся ли ffmpeg. Без него человек увидит под ними
            // жёлтую оговорку «Вшивать нечем» — и это правда: он просил вшить,
            // а вшить нечем. Погасить их было бы тише, но соврало бы дважды:
            // сегодня — про то, чего он не отменял, а в тот день, когда ffmpeg
            // доставится, — молчаливым отказом вшить то, что он выбрал.
            options: saved.options,
            sub_lang: SubLang::default(),
            section_start: String::new(),
            section_end: String::new(),
            section: Section::default(),
            section_error: None,
            cookies: CookieSource::default(),
            cookie_file: None,
            cookie_file_display: PICK_COOKIE_FILE.to_owned(),
            ffmpeg_missing: false,
            out_dir_display: display_dir(out_dir.as_deref()),
            out_dir,
            state: State::Idle,
            progress: Progress::default(),
            stage: String::new(),
            preview: Preview::default(),
            log: Vec::new(),
            rx: None,
            handle: None,
            setup_error: None,
            setup: Setup::Ready,
            warning: None,
            notice: None,
            setup_handle: None,
            versions_rx: None,
            meta_line: String::new(),
            progress_line: String::new(),
            done_path_display: String::new(),
            quality_note: String::new(),
            sub_lang_label: SubLang::ORIGINAL_LABEL.to_owned(),
            subs_note: String::new(),
            // Пустая до первого ответа: подвал просто не показывает версий,
            // пока их не спросили, — «неизвестно» там было бы враньём
            // на те доли секунды, что идёт опрос.
            tools_line: String::new(),
            advanced_summary: String::new(),
            url_invalid: false,
            log_copied_at: None,
            tab: Tab::Download,
            machine_tab: MachineTab::Now,
            rail_tab: RailTab::Queue,
            advanced: false,
            log_open: false,
            meta: MetaPanel::new(),
            system: SystemPanel::new(),
            monitor: MonitorPanel::new(),
            power: PowerPanel::new(),
            gpu: None,
            history: History::default(),
            queue: Queue::new(),
            maximize_pending: true,
            saver: settings::Saver::spawn(),
            gpu_errors: Arc::default(),
            backdrop: theme::Backdrop::default(),
        };

        app.ffmpeg_missing = !engine::has_ffmpeg();
        app.rebuild_advanced_summary();

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

        // Версии спрашиваем сразу — но в отдельном потоке: это запуск двух
        // чужих программ, и в конструкторе окна ему не место. До ответа строка
        // обслуживания просто без версий, и открытию окна опрос не мешает.
        app.refresh_versions(ctx);

        app
    }

    /// Ставит перехват ошибок wgpu вместо падения процесса — см. [`GpuErrors`].
    ///
    /// Отдельным вызовом, а не внутри `new`: устройство появляется только у
    /// рендерера wgpu, и его может не быть вовсе (сборка на glow). Тогда
    /// перехватывать нечего, и это не ошибка — приложение работает как прежде.
    pub fn catch_gpu_errors(&self, cc: &eframe::CreationContext<'_>) {
        let Some(state) = &cc.wgpu_render_state else {
            return;
        };
        let errors = Arc::clone(&self.gpu_errors);
        let ctx = cc.egui_ctx.clone();
        state.device.on_uncaptured_error(Arc::new(move |error| {
            errors.push(error.to_string());
            // Сообщение попадёт в журнал в ближайшем кадре, а сам кадр без
            // этой строки может и не наступить: окно в этот момент как раз
            // ничего не рисует.
            ctx.request_repaint();
        }));
    }

    /// Забирает сведения о видеокарте с адаптера, которым рисуется окно.
    ///
    /// Зовётся один раз при создании приложения, из того же места, что и
    /// `catch_gpu_errors`, и по той же причине: `RenderState` виден только
    /// здесь. Второй адаптер не открываем — этот уже готов, и данные с него
    /// стоят ноль. Отдельным методом, а не внутри `catch_gpu_errors`: тот
    /// про выживание процесса, этот про содержимое вкладки, и смешивать
    /// два несвязанных дела в одном имени незачем.
    ///
    /// Обновлять нечего: адаптер у окна один на всю жизнь процесса.
    pub fn read_gpu_info(&mut self, cc: &eframe::CreationContext<'_>) {
        // `None` — сборка на glow либо wgpu не поднялся. Не ошибка: пункта
        // про видеокарту в отчёте просто не будет.
        let Some(state) = &cc.wgpu_render_state else {
            return;
        };
        let info = state.adapter.get_info();

        // Пустые строки у `AdapterInfo` — обычное дело: `driver` и
        // `driver_info` приходят пустыми на Metal и на GL через ANGLE, и это
        // штатно. Пустоту превращаем в `None` здесь, у самого источника,
        // чтобы дальше по коду «нет данных» имело один-единственный вид.
        let text = |s: String| (!s.trim().is_empty()).then_some(s);

        self.gpu = text(info.name.clone()).map(|name| GpuInfo {
            name,
            kind: match info.device_type {
                eframe::wgpu::DeviceType::DiscreteGpu => "дискретная",
                eframe::wgpu::DeviceType::IntegratedGpu => "встроенная",
                eframe::wgpu::DeviceType::VirtualGpu => "виртуальная",
                // Программный растеризатор: карты нет вовсе или её драйвер
                // не подошёл. Сказать об этом стоит — рисование в этом
                // случае заметно медленнее.
                eframe::wgpu::DeviceType::Cpu => "программная отрисовка",
                eframe::wgpu::DeviceType::Other => "тип неизвестен",
            },
            // Ноль здесь — «идентификатор неизвестен», и звать по нему
            // разбор вендора незачем: он ответил бы «Unknown».
            vendor: (info.vendor != 0)
                .then(|| eframe::egui_wgpu::parse_vendor_id(info.vendor))
                .filter(|name| *name != "Unknown")
                .map(str::to_owned),
            // Имя драйвера и его версия лежат в разных полях, и порознь
            // каждое бесполезно: «NVIDIA» без числа не отличает свежий
            // драйвер от трёхлетнего. Склеиваем, но только непустые — на
            // Metal и на GL через ANGLE оба поля приходят пустыми, и это
            // штатно.
            driver: match (text(info.driver.clone()), text(info.driver_info.clone())) {
                (Some(name), Some(version)) => Some(format!("{name} {version}")),
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            },
            backend: info.backend.to_string(),
        });
    }

    /// Переносит пойманное в журнал. Зовётся из кадра, где это уже безопасно:
    /// сам обработчик работает посреди чужого кода.
    fn drain_gpu_errors(&mut self) {
        for message in self.gpu_errors.take() {
            let line = gpu_error_line(&message);
            // Повтор гасится здесь, по готовой строке, а не по сообщению
            // wgpu. Иначе в журнале двоится: про один и тот же предел
            // приходят два РАЗНЫХ сообщения (про поверхность и про
            // `set_viewport`), а человеку они говорят одно и то же. Заодно
            // это гасит повтор между кадрами: пока окно остаётся большим,
            // ошибка приходит заново в каждом.
            if self.log.last().is_some_and(|last| *last == line) {
                continue;
            }
            self.log.push(line);
            if self.log.len() > LOG_LIMIT {
                self.log.drain(..self.log.len() - LOG_LIMIT);
            }
        }
    }

    /// Вызывается, когда установка закончилась — успехом или нет.
    /// Инструменты после неё нужно искать заново: до установки их не было.
    fn finish_setup(&mut self, outcome: Setup, ctx: &egui::Context) {
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
        // Предпросмотр мог споткнуться ровно о то, чего до этой минуты не было:
        // без yt-dlp спрашивать сайт нечем, а молчит он об этом одинаково — и
        // про отсутствие инструмента, и про незнакомый сайт. Раз инструменты
        // появились, пробуем ещё раз: иначе ссылка, лежащая в поле с прошлой
        // попытки, останется без карточки до следующей правки текста.
        if self.preview.state == PreviewState::Failed {
            self.preview.retry(ctx.input(|i| i.time));
        }

        // Версии после установки или обновления другие — показанные устарели
        // ровно в этот момент. Спросить заново обязательно: иначе кнопка
        // «Обновить» отчитывалась бы об успехе, а строка над ней продолжала бы
        // показывать прежний номер, и обновление выглядело бы несработавшим.
        self.refresh_versions(ctx);
    }

    fn cancel_setup(&mut self, ctx: &egui::Context) {
        if let Some(handle) = &self.setup_handle {
            handle.cancel();
        }
        self.finish_setup(Setup::Ready, ctx);
    }

    /// Обновление инструмента по кнопке.
    ///
    /// Идёт по тому же каналу и в ту же модалку, что и установка при первом
    /// запуске: задача та же — скачать бинарник и показать прогресс, поэтому
    /// заводить второй механизм незачем.
    fn start_update(&mut self, what: setup::Component, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();

        // Прошлый исход убираем: иначе рядом со свежим результатом висела бы
        // причина позапрошлой неудачи и было бы не понять, к чему она.
        self.notice = None;
        self.warning = None;
        if matches!(self.setup, Setup::Failed(_)) {
            self.setup = Setup::Ready;
        }

        self.setup_handle = Some(setup::start_update(what, tx, move || {
            notify_ctx.request_repaint()
        }));
        self.rx = Some(rx);
        self.setup = Setup::Updating(what);
        self.progress = Progress::default();
        // Первая стадия у двух веток разная: у yt-dlp следом идёт запрос
        // выпуска, у ffmpeg — сразу загрузка, и «Проверяю версию…» висела бы
        // над полосой, которая на самом деле качает архив.
        self.stage = match what {
            setup::Component::Ytdlp => "Проверяю версию…",
            setup::Component::Ffmpeg => "Готовлюсь скачивать…",
        }
        .into();
        self.rebuild_progress_line();
    }

    /// Спрашивает версии установленных инструментов заново.
    ///
    /// Свой приёмник, а не общий `rx`: тот занят загрузкой или установкой, и
    /// ответ о версиях, пришедший в него, разбирался бы вместе с их событиями.
    /// Так же устроена вкладка метаданных — образец готовый.
    ///
    /// Зовётся при запуске и после каждой установки или обновления: ровно
    /// тогда версии и меняются. В кадре отрисовки — никогда.
    fn refresh_versions(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        setup::start_versions(tx, move || notify_ctx.request_repaint());
        self.versions_rx = Some(rx);
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
            // Флажки уезжают такими, какие есть, даже когда их не видно:
            // «Субтитры» при MP3 и «Можно автоматические» при снятых
            // субтитрах спрятаны, но значения своего не теряли. Обнулять
            // спрятанное нельзя — один запуск в режиме MP3 стёр бы настройку
            // насовсем, хотя человек к ней не прикасался.
            options: self.options,
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
    /// Проверки, общие для «Скачать» и «В очередь»: они относятся к ссылке
    /// в поле, а не к тому, что с ней собираются делать.
    fn url_is_ready(&self) -> bool {
        !self.url.trim().is_empty()
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

    /// Можно ли поставить ссылку из поля в конец очереди.
    ///
    /// В отличие от «Скачать», работает и во время загрузки: очередь затем
    /// и нужна, чтобы докладывать ссылки, пока идёт первая.
    fn can_enqueue(&self) -> bool {
        self.url_is_ready() && !self.queue.full
    }

    fn can_start(&self) -> bool {
        if matches!(self.state, State::Running) || self.setup_error.is_some() {
            return false;
        }
        // Ссылка в поле поедет в очередь первой — её проверки прежние.
        // Поле пустое — запускать имеет смысл только то, что уже стоит
        // в очереди: ровно так возвращаются к остатку после «Отмены».
        if self.url.trim().is_empty() {
            self.queue.has_waiting()
        } else {
            self.url_is_ready()
        }
    }

    /// Ставит ссылку из поля в конец очереди.
    ///
    /// Возвращает `false`, если ставить было нечего или места не нашлось.
    fn enqueue(&mut self) -> bool {
        if !self.can_enqueue() {
            return false;
        }
        let Some(out_dir) = self.out_dir.clone() else {
            return false;
        };

        let request = Request {
            url: self.url.trim().to_owned(),
            format: self.format,
            quality: self.quality,
            options: self.options,
            cookies: self.cookies,
            cookie_file: self.cookie_file.clone(),
            section: self.section,
            sub_lang: self.sub_lang.clone(),
        };

        // Ответ предпросмотра уезжает вместе со ссылкой: спрашивать сайт
        // второй раз о том же самом незачем — ни ради названия в списке,
        // ни ради обложки, которая и так уже показана.
        let known = self.preview.answer_for(&request);
        if self.queue.push(request, out_dir, known).is_none() {
            return false;
        }

        // Пока ничего не идёт, экран описывает не прошлую загрузку, а то,
        // что человек только что попросил. Во время загрузки состояние
        // не трогаем: там оно про неё.
        if !matches!(self.state, State::Running) {
            self.state = State::Queued;
        }
        true
    }

    /// Нажали «Скачать»: ссылка из поля уходит в конец очереди, и очередь
    /// идёт сверху вниз.
    ///
    /// Пустое поле при непустой очереди — это «продолжить»: так возвращаются
    /// к тому, что осталось после «Отмены». Поле при этом не очищается — до
    /// появления очереди «Скачать» его тоже не трогал, и повторить ту же
    /// ссылку по-прежнему можно вторым нажатием.
    fn start(&mut self, ctx: &egui::Context) {
        self.enqueue();
        self.start_next(ctx);
    }

    /// Запускает следующую ожидающую ссылку.
    ///
    /// Строго по одной: пока идёт загрузка, второго потока не заводим —
    /// десяток ссылок обернулся бы десятком yt-dlp разом, поделивших канал
    /// и полосу на всех. Очередь кончилась — просто ничего не делаем:
    /// на экране остаётся исход последней загрузки.
    fn start_next(&mut self, ctx: &egui::Context) {
        // `setup.busy()` в проверке не для полноты: установка занимает тот же
        // единственный `rx`, и запуск загрузки поверх неё отобрал бы у модалки
        // её же события.
        if matches!(self.state, State::Running) || self.setup.busy() {
            return;
        }
        let Some((id, request, out_dir, known)) = self.queue.next_waiting() else {
            return;
        };

        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();

        match engine::start(id, request, out_dir, known, tx, move || {
            notify_ctx.request_repaint()
        }) {
            Ok(handle) => {
                self.queue.set_status(id, QueueStatus::Running);
                self.rx = Some(rx);
                self.handle = Some(handle);
                self.state = State::Running;
                self.progress = Progress::default();
                self.stage = "Запуск…".into();
                self.log.clear();
                self.done_path_display.clear();
                self.rebuild_progress_line();
                // Карточку под полем ссылки здесь не трогаем, и это не
                // упущение: она описывает то, что лежит в поле, а не то, что
                // качается. Запуск ссылки из середины очереди поля не меняет —
                // и стирать по нему название с обложкой значило бы гасить
                // сведения о совсем другом ролике.
            }
            Err(err) => {
                // Не нашёлся yt-dlp — дальше по очереди идти незачем:
                // следующая ссылка споткнётся ровно о то же самое. Значит,
                // останавливаемся и говорим об этом один раз, а не полсотни.
                self.queue.set_status(id, QueueStatus::Failed(err.clone()));
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
        // Приёмник роняем сразу, и это не только уборка: у убитого процесса
        // поток движка живёт ещё секунду-другую и досылает свой `Failed`.
        // Закрытый канал глушит его, а заодно сообщает самому движку, что
        // UI про эту загрузку забыл (см. проверку перед запуском в `run`).
        self.rx = None;
        // Снятая — не проваленная: отмена показывается «Отменено», а не
        // ошибкой (Правило 2). Остальные ссылки остаются ждать: «Отмена»
        // останавливает очередь, а не стирает её, и «Скачать» продолжит
        // с того же места.
        if let Some(id) = self.queue.running_id() {
            self.queue.set_status(id, QueueStatus::Cancelled);
        }
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
        let Some(info) = &self.preview.info else {
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
            self.preview.info.as_ref().and_then(MediaInfo::max_height),
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

    /// Подпись выбранного языка субтитров и оговорка под списком.
    ///
    /// Обе строки собираются вместе намеренно: зависят они от одного и того
    /// же — от выбора человека и от ответа `probe`. Разъедься вызовы, и на
    /// списке был бы написан один язык, а оговорка говорила бы про другой.
    ///
    /// Зовётся только из обработчиков: смены формата, щелчка по галочке,
    /// выбора в списке и приезда `Event::Info`. В кадре отрисовки здесь
    /// собирать нечего — обе строки уже готовы.
    fn rebuild_subtitles(&mut self) {
        self.sub_lang_label.clear();
        match &self.sub_lang {
            SubLang::Original => self.sub_lang_label.push_str(SubLang::ORIGINAL_LABEL),
            SubLang::Code(code) => {
                // Подписи может не быть: список остался от прошлой ссылки,
                // а у нынешней такого языка нет. Показываем тогда сам код —
                // он и уедет в `--sub-langs`.
                let label = self
                    .preview
                    .info
                    .as_ref()
                    .and_then(|info| info.subtitle_label(code))
                    .unwrap_or(code);
                self.sub_lang_label.push_str(label);
            }
        }

        self.subs_note.clear();
        // Говорим только тогда, когда знаем наверняка: субтитры просят,
        // положить их есть куда, и `probe` уже ответил. Молчание честнее
        // догадки.
        if self.format != Format::Mp4 || !self.options.embed_subs {
            return;
        }
        let Some(info) = &self.preview.info else {
            return;
        };
        if let Some(note) = info.subtitle_note(&self.sub_lang, self.options.auto_subs) {
            self.subs_note = note;
        }
    }

    /// Сводка свёрнутых тонких настроек: что из них включено.
    ///
    /// Пишем только про то, что отличается от умолчания. Перечислять «фрагмент
    /// не задан · вход не используется» смысла нет: это состояние у почти всех
    /// и почти всегда, и в заголовке оно превратилось бы в шум, за которым
    /// перестанут замечать настоящую строку.
    ///
    /// Зовётся из обработчиков — правки полей фрагмента, выбора браузера,
    /// выбора языка, — а не из кадра: `ui()` идёт 60 раз в секунду.
    fn rebuild_advanced_summary(&mut self) {
        self.advanced_summary = advanced_summary(
            self.section_error.is_some(),
            self.section.any(),
            self.cookies.any(),
            &self.sub_lang,
        );
    }

    /// Короткая подпись состояния для плашки. Строки статические —
    /// в кадре отрисовки ничего не выделяется.
    fn status(&self) -> (&'static str, egui::Color32) {
        match self.state {
            State::Idle => ("Готов к работе", theme::TEXT_SECONDARY),
            State::Queued => ("В очереди", theme::TEXT_SECONDARY),
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
        // Текущая загрузка доработала — пора брать следующую из очереди.
        // Флагом, а не вызовом прямо из ветки: `start_next` подменяет `rx`,
        // и делать это посреди разбора уже вычитанной пачки — значит гадать,
        // к какой из двух загрузок относился её хвост.
        let mut advance = false;

        for event in events {
            match event {
                Event::Info(info) => {
                    // Название ролика — единственное, по чему строку очереди
                    // узнают глазами: десяток ссылок с одного сайта
                    // различается тремя символами в конце.
                    if let Some(title) = &info.title
                        && let Some(id) = self.queue.running_id()
                    {
                        self.queue.set_title(id, title);
                    }
                    // Карточка под полем ссылки — не про эту загрузку, и
                    // класть в неё что попало нельзя: качается обычно уже не
                    // то, что набрано в поле. Но если ссылка та же, а
                    // предпросмотр по ней ничего не добился (не было yt-dlp,
                    // сайт промолчал), то показать ответ загрузки — ровно то,
                    // что сделал бы он сам, повезло бы ему чуть больше.
                    // Обычно этой ветки не бывает вовсе: при живом
                    // предпросмотре движок и не спрашивает (см. `known`
                    // в `engine::start`).
                    if self.preview.info.is_none()
                        && self.preview.is_about(self.queue.running_request())
                    {
                        self.preview.info = Some(info);
                        self.preview.state = PreviewState::Ready;
                        meta_dirty = true;
                    }
                }
                Event::Thumbnail(cover) => {
                    // Та же оговорка, что и у `Info`: чужую обложку под полем
                    // ссылки показывать нельзя — ровно от этого превью
                    // и должно спасать.
                    if self.preview.thumbnail.is_none()
                        && self.preview.is_about(self.queue.running_request())
                    {
                        self.preview.set_cover(&cover, ctx);
                    }
                }
                Event::Stage(stage) => {
                    self.stage = stage;
                    progress_dirty = true;
                }
                Event::Progress(p) => {
                    // Чужой прогресс — это прогресс уже снятой загрузки:
                    // её процесс досылает строки и после `kill`. Показать
                    // его на месте текущей значило бы дёрнуть полосу назад.
                    //
                    // Во время установки номера нет вовсе (`NO_DOWNLOAD`),
                    // и разводить там нечего: канал занят ею одной.
                    if self.setup.busy() || self.queue.is_running(p.download_id) {
                        self.progress = p;
                        progress_dirty = true;
                    }
                }
                Event::Log(line) => self.push_log(line),
                Event::Done { id, path } => {
                    // Сверка номера обязательна: пока событие шло по каналу,
                    // загрузку могли снять, и «Готово» встало бы рядом
                    // с чужим путём.
                    if self.queue.is_running(id) {
                        self.stage = "Готово".into();
                        self.done_path_display = path.display().to_string();
                        // Единственное место, где пополняется история: другого
                        // признака «файл готов и лежит вот здесь» у UI нет.
                        // Успех без пути (`Event::Stage("Готово (файл уже
                        // существовал)")`) сюда не попадает — записывать нечего.
                        self.history.remember(&path);
                        self.queue.set_status(id, QueueStatus::Done);
                        self.state = State::Done(path);
                        self.handle = None;
                        advance = true;
                        progress_dirty = true;
                    }
                }
                Event::Failed { id, message } => {
                    // Один и тот же вариант обслуживает три задачи, поэтому
                    // разводим их по режиму и по номеру: во время установки
                    // это сбой установки, а не сорвавшаяся загрузка ролика.
                    if self.setup.busy() {
                        self.finish_setup(Setup::Failed(message), ctx);
                    } else if self.queue.is_running(id) {
                        self.stage = "Ошибка".into();
                        self.queue
                            .set_status(id, QueueStatus::Failed(message.clone()));
                        self.state = State::Failed(message);
                        self.handle = None;
                        // Одна мёртвая ссылка — не повод бросать девять живых:
                        // очередь идёт дальше. Ради этого её и ставят, уходя.
                        // Причина никуда не денется: она лежит в самой строке
                        // очереди, а не только в журнале, который вот-вот
                        // очистится под следующую загрузку.
                        advance = true;
                        progress_dirty = true;
                    }
                }
                Event::Ready => {
                    self.finish_setup(Setup::Ready, ctx);
                }
                Event::Warning(text) => {
                    self.warning = Some(text);
                }
                Event::Notice(text) => {
                    self.notice = Some(text);
                }
                // Метаданные и версии ходят по своим каналам — сюда эти
                // события попасть не могут. Ветка выписана явно, а не через
                // `_`, чтобы компилятор и дальше требовал разбирать новые
                // варианты `Event` во всех приёмниках.
                Event::Tags(_)
                | Event::Cleaned(_)
                | Event::Versions(_)
                | Event::SystemReport(_)
                | Event::Power(_)
                | Event::Perf(_) => {}
            }
        }

        if progress_dirty {
            self.rebuild_progress_line();
        }
        if meta_dirty {
            self.rebuild_meta_line();
            self.rebuild_quality_note();
            // Дорожки субтитров приезжают тем же `Event::Info`: до него
            // список языков пуст, а сказать «вшивать нечего» нечем.
            self.rebuild_subtitles();
        }

        if disconnected {
            self.rx = None;
            // Поток движка кончился, не сказав ни `Done`, ни `Failed`. Так
            // выглядит успех без пути: файл уже лежал на диске, и стадию
            // `after_move` yt-dlp пропустил — по Правилу 2 это НЕ ошибка.
            // Отметить его обязательно: иначе строка навсегда осталась бы
            // «качается», а вся очередь встала бы на ровном месте.
            if let Some(id) = self.queue.running_id() {
                self.queue.set_status(id, QueueStatus::Done);
                advance = true;
            }
            if matches!(self.state, State::Running) {
                self.state = State::Idle;
            }
        }

        // Строго после разбора пачки и после разрыва канала: `start_next`
        // заводит новый `rx`, и сделай мы это раньше — обрыв старого канала
        // обнулил бы только что заведённый.
        if advance {
            self.start_next(ctx);
        }
    }

    /// Ведёт фоновый запрос по ссылке из поля: забирает ответ и, когда
    /// истекло окно дебаунса, заводит следующий.
    ///
    /// Зовётся каждый кадр, но работы в обычном кадре здесь нет: пустой
    /// приёмник и сравнение двух чисел.
    fn tick_preview(&mut self, ctx: &egui::Context) {
        // Ответ разбираем первым: он мог приехать, пока шло окно дебаунса
        // следующего запроса.
        let mut ready = false;
        for event in self.preview.take_events() {
            match event {
                Event::Info(info) => {
                    self.preview.info = Some(info);
                    self.preview.state = PreviewState::Ready;
                    ready = true;
                }
                Event::Thumbnail(cover) => self.preview.set_cover(&cover, ctx),
                // Сюда попадает разве что «обложка не загрузилась»: больше
                // предпросмотр в журнал ничего не пишет, и молчит он даже
                // о собственной неудаче (см. `engine::start_probe`).
                Event::Log(line) => self.push_log(line),
                // Остальное по этому каналу не ходит. Ветка выписана явно,
                // а не через `_`, чтобы компилятор и дальше требовал
                // разбирать новые варианты `Event` во всех приёмниках.
                Event::Stage(_)
                | Event::Progress(_)
                | Event::Done { .. }
                | Event::Failed { .. }
                | Event::Ready
                | Event::Warning(_)
                | Event::Notice(_)
                | Event::Tags(_)
                | Event::Cleaned(_)
                | Event::Versions(_)
                | Event::SystemReport(_)
                | Event::Power(_)
                | Event::Perf(_) => {}
            }
        }

        if ready {
            self.rebuild_meta_line();
            self.rebuild_quality_note();
            // Дорожки субтитров приезжают тем же `Event::Info`: до него
            // список языков пуст, а сказать «вшивать нечего» нечем.
            self.rebuild_subtitles();
        }

        let Some(due) = self.preview.due else {
            return;
        };
        let now = ctx.input(|i| i.time);
        if now < due {
            // Кадр к сроку приходится просить: без ввода egui окно не
            // перерисовывает, и запрос ушёл бы не через `PREVIEW_DEBOUNCE`,
            // а при первом движении мыши — то есть, если ссылку вставили
            // и убрали руки, не ушёл бы вовсе.
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(due - now));
            return;
        }

        self.start_preview(ctx);
    }

    /// Спрашивает сайт о ссылке из поля.
    fn start_preview(&mut self, ctx: &egui::Context) {
        let request = Request {
            url: self.preview.url.clone(),
            cookies: self.preview.cookies,
            cookie_file: self.preview.cookie_file.clone(),
            // Остальное `probe` не спрашивает (см. `ytdlp::probe_args`), но
            // запрос — это запрос целиком, и половины его не бывает. Заодно
            // ровно эти поля уедут в загрузку, если нажмут «Скачать».
            format: self.format,
            quality: self.quality,
            options: self.options,
            section: self.section,
            sub_lang: self.sub_lang.clone(),
        };

        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();
        let handle = engine::start_probe(request, tx, move || notify_ctx.request_repaint());
        self.preview.asked(rx, handle);
    }

    /// Ссылку в поле правили — предпросмотр обязан догнать.
    ///
    /// Зовётся из обработчиков (правка поля, смена браузера в списке), а не
    /// из кадра: сравнивать строки шестьдесят раз в секунду незачем.
    fn retarget_preview(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if !self.preview.retarget(
            self.url.trim(),
            self.cookies,
            self.cookie_file.as_deref(),
            now,
        ) {
            return;
        }

        // Всё, что было собрано из прошлого ответа, теперь про другую ссылку.
        // Молча оставить это на экране — худший исход: «Выше 720p этот ролик
        // не отдают» под чужой ссылкой выглядит фактом о ней.
        self.meta_line.clear();
        self.quality_note.clear();
        self.rebuild_subtitles();
    }

    /// Добавляет строку в журнал, соблюдая его потолок.
    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > LOG_LIMIT {
            self.log.drain(..self.log.len() - LOG_LIMIT);
        }
    }

    /// Забирает ответ о версиях инструментов, если он подъехал.
    ///
    /// Приёмник бросаем сразу: ответ приходит ровно один, и держать канал
    /// дальше значило бы каждый кадр опрашивать заведомо мёртвую очередь.
    fn drain_versions(&mut self) {
        let Some(rx) = &self.versions_rx else {
            return;
        };

        let received = match rx.try_recv() {
            Ok(Event::Versions(versions)) => Some(versions),
            // Прочие варианты по этому каналу не ходят: отправитель один и
            // шлёт ровно `Versions`. Разбирать их незачем, а `_` вместо
            // явного `Ok(_)` спрятал бы от компилятора смену этого уговора.
            Ok(_) => None,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };

        self.versions_rx = None;
        let Some(versions) = received else {
            return;
        };

        // Строку собираем здесь, на приёме события, а не в кадре отрисовки:
        // меняется она дважды за запуск, а `ui()` зовут 60 раз в секунду.
        self.tools_line = format!(
            "{} · {}",
            version_line("yt-dlp", &versions.ytdlp),
            version_line("ffmpeg", &versions.ffmpeg)
        );
    }
}

/// Сводка тонких настроек: перечисление того, что в них включено.
///
/// Свободная и чистая функция, а не кусок метода: сводка — единственное, что
/// говорит о заданном фрагменте, пока группа свёрнута, и промах в ней стоит
/// человеку скачанного куска вместо ролика. Такое покрывается тестом.
///
/// Пишем только про отличия от умолчания. Перечислять «фрагмент не задан ·
/// вход не используется» смысла нет: это состояние у почти всех и почти
/// всегда, и в заголовке оно превратилось бы в шум, за которым перестанут
/// замечать настоящую строку.
fn advanced_summary(
    section_broken: bool,
    section_set: bool,
    cookies: bool,
    lang: &SubLang,
) -> String {
    let section = match (section_broken, section_set) {
        (true, _) => Some("фрагмент задан неверно".to_owned()),
        (false, true) => Some("фрагмент".to_owned()),
        (false, false) => None,
    };
    let cookies = cookies.then(|| "вход на сайт".to_owned());
    // Код языка, а не подпись: подпись бывает длинной («Русский
    // (автоматические)»), а места в заголовке ровно одна строка.
    let subs = match lang {
        SubLang::Code(code) => Some(format!("субтитры: {code}")),
        SubLang::Original => None,
    };

    let parts: Vec<String> = [section, cookies, subs].into_iter().flatten().collect();
    if parts.is_empty() {
        "фрагмент, вход на сайт, язык субтитров".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// Строка «инструмент — версия» для строки обслуживания.
///
/// «Не найден» и «версию узнать не вышло» говорят разное, и путать их нельзя:
/// в первом случае программы на машине нет и её надо ставить, во втором она
/// работает, просто печатает версию не так, как мы ожидали (см.
/// `setup::parse_version_line`). Сказать «не найден» про рабочую копию —
/// значит отправить человека решать несуществующую беду.
fn version_line(name: &str, version: &crate::model::ToolVersion) -> String {
    use crate::model::ToolVersion;
    match version {
        ToolVersion::Known(version) => format!("{name} — {version}"),
        ToolVersion::Unknown => format!("{name} — версия неизвестна"),
        ToolVersion::Missing => format!("{name} — не найден"),
    }
}

// ---------------------------------------------------------------------------
// Отрисовка
// ---------------------------------------------------------------------------

impl eframe::App for SavioApp {
    /// Фон окна до первой отрисовки — основа темы, иначе при запуске
    /// и ресайзе видна светлая вспышка. Пятна поверх неё кладёт уже кадр.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG_BASE.to_normalized_gamma_f32()
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
        self.tick_preview(ui.ctx());
        self.meta.drain();
        self.system.drain();
        self.monitor.drain(ui.ctx());
        self.power.drain();
        self.drain_versions();
        self.drain_gpu_errors();

        // Опрос железа стоит кадра в секунду — и стоит его, только пока есть
        // кому смотреть. Решение принимается здесь, а не во вкладке: вкладка
        // при закрытом мониторе не рисуется вовсе, и остановить опрос из неё
        // было бы некому.
        let now_open = self.tab == Tab::Machine && self.machine_tab == MachineTab::Now;
        let watched = now_open || self.monitor.overlay;
        self.monitor.set_running(watched, ui.ctx());
        // Питание перечитывается по открытию половины, а не по кадру: оно
        // меняется раз в день, но меняют его и мимо Savio. Место здесь, а не
        // во вкладке, по той же причине, что и у опроса: закрытая половина
        // не рисуется, и заметить её закрытие из неё самой некому.
        self.power.watch(now_open, ui.ctx());

        if self.maximize_pending {
            self.maximize_pending = false;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }

        // Фон кладём первым и прямо в корневой `Ui`, до всех панелей: egui
        // рисует фигуры в порядке добавления, и всё, что появится дальше,
        // ляжет поверх. Заливки у панелей при этом нет вовсе (`panel_fill`
        // прозрачный) — иначе сплошной цвет закрасил бы пятна.
        self.backdrop.paint(ui.painter(), ui.max_rect());

        // Шапка и подвал — панели, а не первая и последняя строки прокрутки:
        // они обязаны стоять на месте, пока содержимое едет. У панелей это
        // даром, а в прокрутке пришлось бы отмерять высоту руками.
        egui::Panel::top("savio-header")
            .resizable(false)
            .show_separator_line(false)
            .frame(theme::bar_frame())
            .show(ui, |ui| self.header(ui));

        egui::Panel::bottom("savio-footer")
            .resizable(false)
            .show_separator_line(false)
            .frame(theme::bar_frame())
            .show(ui, |ui| self.footer(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(20, 16)))
            .show(ui, |ui| {
                // Прокрутка нужна на минимальном размере окна: без неё
                // кнопка «Скачать» просто обрезалась бы нижней кромкой.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.tab {
                        Tab::Download => self.download_tab(ui),
                        Tab::Metadata => self.metadata_tab(ui),
                        Tab::Machine => self.machine_tab(ui),
                    });
            });

        // Оверлей — отдельное окно, и просить его надо на каждом проходе,
        // иначе egui его закроет. Место здесь, а не во вкладке: оверлей живёт
        // и при закрытой вкладке — ради этого он и нужен.
        self.monitor.show_overlay(ui.ctx());

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
    /// Шапка окна: имя, три раздела, версия.
    ///
    /// Разделов три, и запас по ширине снова есть: «Метаданные» — самая
    /// длинная подпись — в окне 520 больше ни с кем не спорит. Сегменты
    /// здесь по ширине текста, а не по равной доле: дорожка стоит посреди
    /// шапки, а не растянута на всё окно, и равные доли растащили бы её
    /// по ширине самого длинного слова.
    fn header(&mut self, ui: &mut egui::Ui) {
        // Порядок здесь — порядок на экране.
        const TABS: [(Tab, &str); 3] = [
            (Tab::Download, "Загрузка"),
            (Tab::Metadata, "Метаданные"),
            (Tab::Machine, "Машина"),
        ];

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;

            ui.label(
                egui::RichText::new("Savio")
                    .font(theme::display(21.0))
                    .color(theme::TEXT_PRIMARY),
            );
            // Акцентная точка — единственный «логотип», который нужен.
            let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.5, theme::ACCENT);

            // Что нажали, применяем после дорожки: внутри замыкания `self`
            // занят целиком, и присвоить поле оттуда нельзя.
            let mut picked = None;
            theme::track_frame().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (tab, label) in TABS {
                        if segment_button(ui, label, self.tab == tab, 0.0) {
                            picked = Some(tab);
                        }
                    }
                });
            });
            if let Some(tab) = picked {
                self.tab = tab;
            }

            // Версию прижимаем к правому краю: она нужна, когда выясняют,
            // почему что-то не работает, но в остальное время не должна
            // тянуть на себя внимание.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(VERSION)
                        .small()
                        .color(theme::TEXT_MUTED),
                );
            });
        });
    }

    /// Подвал: версии инструментов, их обновление и журнал.
    ///
    /// Внизу, а не рядом с кнопкой «Скачать», и намеренно: это то, за чем
    /// идут, когда что-то перестало работать, — соседство с журналом тут
    /// уместнее, чем спор за внимание с главным действием экрана. Стоит
    /// подвал на всех вкладках сразу: обновлять движок из «Метаданных»
    /// незачем, но и прятать его при переключении вкладки не за чем —
    /// полоса на месте, и это одно из того, что делает окно спокойным.
    fn footer(&mut self, ui: &mut egui::Ui) {
        // Пока занят единственный канал событий — обновляться нечем: и
        // загрузка, и установка ходят через тот же `rx`.
        let enabled = !matches!(self.state, State::Running) && !self.setup.busy();
        let mut update = None;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;

            // Точка зелёная, когда версии известны, и приглушённая, пока
            // опрос идёт. Цветом одним ничего не сказано — рядом текст.
            let known = !self.tools_line.is_empty();
            let (dot, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
            ui.painter().circle_filled(
                dot.center(),
                3.5,
                if known {
                    theme::STATE_SUCCESS
                } else {
                    theme::TEXT_MUTED
                },
            );

            ui.add_enabled_ui(enabled, |ui| {
                if pill_button(ui, "Обновить движок")
                    .on_hover_text(
                        "Сайты меняются, и старый yt-dlp перестаёт их скачивать. \
                         Если ссылка вдруг не работает — обновите движок.",
                    )
                    .clicked()
                {
                    update = Some(setup::Component::Ytdlp);
                }
                if pill_button(ui, "Обновить ffmpeg")
                    .on_hover_text(
                        "Свежая сборка ffmpeg качается целиком — больше сотни \
                         мегабайт. Обновлять его нужно редко.",
                    )
                    .clicked()
                {
                    update = Some(setup::Component::Ffmpeg);
                }
            });

            // Кнопку журнала кладём первой в раскладке справа налево, а
            // строку версий — следом: обрежется тогда строка, а не кнопка.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let has_log = !self.log.is_empty();
                let response = ui.add_enabled_ui(has_log, |ui| {
                    toggle_pill(ui, "Журнал", self.log_open)
                });
                if response.inner.clicked() {
                    self.log_open = !self.log_open;
                }
                if !has_log {
                    response.response.on_hover_text("Пока нечего показывать.");
                }

                // Подсказку с полной строкой вешает сама обрезанная метка
                // (`show_tooltip_when_elided`), поэтому своего
                // `on_hover_text` здесь нет — он дал бы вторую коробку
                // с тем же текстом (дефект 22).
                //
                // yt-dlp в строке первым не случайно: обрезается конец,
                // а версия ffmpeg у git-сборки — это
                // `N-125365-g9a01c1cb6a-20260630`, то есть ровно то, чем
                // в узком окне можно пожертвовать.
                //
                // Порог — не придирка. В окне минимальной ширины на строку
                // остаётся полсотни точек, и обрезка превращает её в «yt-d…»:
                // это уже не сведения, а мусор, который вдобавок отнимает
                // место у кнопок. Лучше не показывать ничего — версии всегда
                // есть в шапке отчёта и в журнале.
                const VERSIONS_MIN: f32 = 150.0;
                if ui.available_width() >= VERSIONS_MIN {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.tools_line)
                                .small()
                                .color(theme::TEXT_MUTED),
                        )
                        .truncate(),
                    );
                }
            });
        });

        if let Some(what) = update {
            let ctx = ui.ctx().clone();
            self.start_update(what, &ctx);
        }

        if self.log_open && !self.log.is_empty() {
            ui.add_space(10.0);
            self.log_section(ui);
        }
    }

    /// Вкладка загрузки: главная карточка слева, ход работы справа.
    ///
    /// Две колонки, а не одна длинная страница: очередь и история — это то,
    /// на что смотрят **во время** загрузки, и ради них прежде приходилось
    /// прокручивать экран мимо всей карточки настроек. Ниже
    /// [`theme::TWO_COLUMN_MIN`] колонки не помещаются рядом, и правая
    /// уходит под главную — иначе в окне 520 переключатель качества из
    /// шести ступеней вылез бы за кромку.
    fn download_tab(&mut self, ui: &mut egui::Ui) {
        // Баннеры идут над обеими колонками: они про экран целиком, а не
        // про настройки, и в узкой колонке их объяснения читались бы
        // по три слова в строке.
        self.download_banners(ui);

        const GAP: f32 = 18.0;
        if ui.available_width() < theme::TWO_COLUMN_MIN {
            self.download_main(ui);
            ui.add_space(GAP);
            self.download_rail(ui);
            return;
        }

        let total = ui.available_width();
        let rail = theme::RAIL_WIDTH;
        let main = total - rail - GAP;

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (width, which) in [(main, true), (rail, false)] {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        // Ширину задаём с обеих сторон: `allocate_ui_with_layout`
                        // двигает курсор не на запрошенный размер, а на тот,
                        // что занял потомок, — без нижней границы короткая
                        // колонка съехала бы к соседке, без верхней длинная
                        // подпись растянула бы её за кромку.
                        ui.set_min_width(width);
                        ui.set_max_width(width);
                        if which {
                            self.download_main(ui);
                        } else {
                            self.download_rail(ui);
                        }
                    },
                );
            }
        });
    }

    /// Сообщения, которые относятся ко всему экрану загрузки.
    fn download_banners(&mut self, ui: &mut egui::Ui) {
        // Причина неудавшейся установки идёт первой: она объясняет, почему
        // инструмента нет, а баннер ниже — что с этим делать.
        let messages = [
            match &self.setup {
                Setup::Failed(err) => Some((err.as_str(), theme::STATE_WARNING)),
                _ => None,
            },
            self.warning
                .as_deref()
                .map(|text| (text, theme::STATE_WARNING)),
            self.notice
                .as_deref()
                .map(|text| (text, theme::STATE_SUCCESS)),
            self.setup_error
                .as_deref()
                .map(|text| (text, theme::STATE_ERROR)),
        ];

        for (text, color) in messages.into_iter().flatten() {
            banner(ui, text, color);
            ui.add_space(12.0);
        }
    }

    /// Главная колонка: всё, что нужно решить до нажатия «Скачать».
    fn download_main(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            self.url_field(ui);
            // Превью идёт сразу под полем, а не в карточке хода работы
            // справа: оно про то, что собираются скачать, а не про то, что
            // качается. К моменту, когда очередь дойдёт до третьей ссылки,
            // в поле давно лежит пятая, и одна карточка на двоих врала бы
            // про обеих.
            self.preview_row(ui);

            ui.add_space(14.0);
            labelled_row(ui, "Формат", |ui| self.format_selector(ui));

            ui.add_space(12.0);
            // Подпись зависит от формата: у видео ступени — это высота
            // кадра, у звука — килобиты в секунду. Берём её у домена, а не
            // пишем здесь второй раз: две копии одной подписи разъезжаются.
            let quality_label = self.format.quality_label();
            labelled_row(ui, quality_label, |ui| self.quality_selector(ui));

            ui.add_space(12.0);
            labelled_row(ui, "Вшить", |ui| self.embed_options(ui));

            ui.add_space(14.0);
            self.advanced_group(ui);

            ui.add_space(14.0);
            self.folder_row(ui);
            ui.add_space(12.0);
            self.action_button(ui);
        });
    }

    /// Правая колонка: что происходит и что стоит следом.
    fn download_rail(&mut self, ui: &mut egui::Ui) {
        self.status_section(ui);
        ui.add_space(14.0);
        self.rail_list(ui);
    }

    /// Вкладка «Машина»: две половины одной темы.
    ///
    /// «Сейчас» и «Состав» — это прежние «Монитор» и «Система». Вместе,
    /// потому что речь об одной и той же машине: одно про то, чем она занята
    /// сию секунду, другое — из чего она собрана. Порознь они занимали две
    /// из пяти вкладок и вытесняли в прокрутку всё остальное.
    fn machine_tab(&mut self, ui: &mut egui::Ui) {
        const HALVES: [(MachineTab, &str); 2] =
            [(MachineTab::Now, "Сейчас"), (MachineTab::Spec, "Состав")];

        let mut picked = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            theme::track_frame().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (half, label) in HALVES {
                        if segment_button(ui, label, self.machine_tab == half, 0.0) {
                            picked = Some(half);
                        }
                    }
                });
            });

            // Плашка про опрос стоит рядом с переключателем, а не под
            // карточками: она объясняет ровно то, что человек включил,
            // перейдя на эту половину.
            if self.machine_tab == MachineTab::Now {
                soft_pill(
                    ui,
                    "Опрос идёт, пока открыт этот раздел",
                    theme::STATE_SUCCESS,
                    theme::SUCCESS_SOFT,
                );
            }
        });
        if let Some(half) = picked {
            self.machine_tab = half;
        }

        ui.add_space(14.0);
        match self.machine_tab {
            MachineTab::Now => self.monitor_tab(ui),
            MachineTab::Spec => self.system_tab(ui),
        }
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
        let (title, subtitle) = match self.setup {
            Setup::Updating(setup::Component::Ytdlp) => (
                "Обновление движка",
                "Savio скачивает свежий yt-dlp. Это занимает несколько секунд.",
            ),
            // Про объём говорим прямо: ffmpeg весит больше сотни мегабайт, и
            // молчаливое ожидание на медленном канале выглядит зависанием.
            Setup::Updating(setup::Component::Ffmpeg) => (
                "Обновление ffmpeg",
                "Savio скачивает свежую сборку ffmpeg целиком — это больше сотни \
                 мегабайт, на медленном интернете надолго.",
            ),
            _ => (
                "Установка зависимостей",
                "Savio догружает недостающие программы. \
                 Это нужно только при первом запуске — пожалуйста, подождите.",
            ),
        };

        let cancelled = egui::Modal::new(egui::Id::new("savio-setup"))
            .backdrop_color(theme::MODAL_BACKDROP)
            .frame(
                egui::Frame::new()
                    .fill(theme::MODAL_FILL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(theme::RADIUS_CARD))
                    .inner_margin(egui::Margin::same(24)),
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
                pill_button(ui, "Отменить").clicked()
            })
            .inner;

        if cancelled {
            self.cancel_setup(ctx);
        }
    }

    fn url_field(&mut self, ui: &mut egui::Ui) {
        let invalid = self.url_invalid;

        let response = ui
            .scope(|ui| {
                if invalid {
                    mark_invalid(ui);
                }

                // Поле выше остальных и без подписи над ним: это первое, к
                // чему тянется рука на экране, и подсказка внутри говорит
                // ровно то же, что сказала бы подпись.
                ui.add_sized(
                    [ui.available_width(), theme::FIELD_HEIGHT],
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("Вставьте ссылку: https://…")
                        .text_color(theme::TEXT_PRIMARY)
                        // Поля широкие: у «таблетки» текст обязан отступать
                        // от полукруглых торцов, иначе он в них упирается.
                        .margin(egui::Margin::symmetric(18, 8)),
                )
            })
            .inner;

        // Пересчитываем только при правке текста, а не каждый кадр.
        if response.changed() {
            let url = self.url.trim();
            self.url_invalid = !url.is_empty() && !looks_like_url(url);
            self.retarget_preview(ui.ctx());
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

    /// Что за ролик лежит по ссылке из поля.
    ///
    /// Всё готово заранее: название и обложка приехали событием, строка
    /// «автор · длительность» собрана в `rebuild_meta_line`. В кадре здесь
    /// не считается и не выделяется ничего.
    fn preview_row(&mut self, ui: &mut egui::Ui) {
        match self.preview.state {
            PreviewState::Idle => return,
            // Про ожидание говорим словами: пустое место под ссылкой человек
            // читает как «Savio ничего про неё не понял», а не как «идёт
            // запрос», и ждать перестаёт.
            PreviewState::Asking => {
                ui.add_space(8.0);
                note(ui, "Смотрю, что это за ролик…", theme::TEXT_MUTED);
                return;
            }
            // Не ошибка и баннера не заслуживает: сведения — украшение,
            // и «Скачать» после этого работает как ни в чём не бывало.
            PreviewState::Failed => {
                ui.add_space(8.0);
                note(
                    ui,
                    "Что это за ролик, выяснить не вышло: сайт не ответил или \
                     он незнаком yt-dlp. Скачать всё равно можно — попробуйте.",
                    theme::TEXT_MUTED,
                );
                return;
            }
            PreviewState::Ready => {}
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if let Some(cover) = &self.preview.thumbnail {
                ui.add(
                    egui::Image::new(cover)
                        // `max_size`, а не `max_width`: у вертикальных роликов
                        // ограничивать надо высоту (см. `PREVIEW_MAX_HEIGHT`),
                        // а пропорцию egui сохраняет сам.
                        .max_size(egui::vec2(PREVIEW_WIDTH, PREVIEW_MAX_HEIGHT))
                        .corner_radius(egui::CornerRadius::same(theme::RADIUS_INNER)),
                );
                ui.add_space(6.0);
            }

            // Текст кладём в свою вертикальную раскладку: в горизонтальной
            // egui берёт для подписей режим `Extend` и вытягивает строку любой
            // длины за кромку окна, а `truncate()` там ограничивать нечем.
            ui.vertical(|ui| {
                if let Some(title) = self
                    .preview
                    .info
                    .as_ref()
                    .and_then(|info| info.title.as_deref())
                {
                    // Своей подсказки нет намеренно: у обрезанной метки egui
                    // вешает её сам, и вторая встала бы под первой (Правило 4).
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(title)
                                // Полужирным настоящим, а не `strong()`: у egui
                                // нет оси насыщенности, и `strong()` меняет
                                // только цвет. Начертание живёт отдельным
                                // семейством — см. `theme::bold`.
                                .font(theme::bold(15.0))
                                .color(theme::TEXT_PRIMARY),
                        )
                        .truncate(),
                    );
                }
                if !self.meta_line.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.meta_line)
                                .small()
                                .color(theme::TEXT_SECONDARY),
                        )
                        .truncate(),
                    );
                }
            });
        });
    }

    fn format_selector(&mut self, ui: &mut egui::Ui) {
        theme::track_frame().show(ui, |ui| {
            ui.horizontal(|ui| {
                const GAP: f32 = 2.0;
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
            // Подписи сегментов качества и оговорки под ними зависят от
            // формата — пересобрать их надо здесь, а не в кадре отрисовки.
            // Субтитры в том же списке: в MP3 их класть некуда, и оговорка
            // про них при переключении на звук обязана исчезнуть.
            self.rebuild_quality_note();
            self.rebuild_subtitles();
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

        theme::track_frame().show(ui, |ui| {
                ui.horizontal(|ui| {
                    const GAP: f32 = 2.0;
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
            // Свёрнутая группа обязана сказать, что фрагмент задан, — иначе
            // обрезка становится невидимой настройкой.
            self.rebuild_advanced_summary();
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

    /// Что вшить в файл: три независимых переключателя-«чипа» и подчинённый
    /// четвёртый.
    ///
    /// Чипы, а не галочки, — так в макете, и на трёх пунктах это выигрыш:
    /// подписи русские и длинные, столбик из трёх галочек занимал четверть
    /// карточки, а чипы переносятся по ширине сами
    /// (`horizontal_wrapped`) и в широком окне встают одной строкой.
    /// Состояние при этом сказано не только цветом: у включённого чипа
    /// нарисована галочка — см. [`chip`].
    fn embed_options(&mut self, ui: &mut egui::Ui) {
        // Субтитры бывают только у видео: в MP3 их положить некуда. Чип
        // гасим, но причину говорим по наведению — молча выключенный элемент
        // выглядит поломкой, а не запретом.
        let subs_enabled = self.format == Format::Mp4;
        // Два флага, а не один, и разница не в экономии. `subs` пересобирает
        // подписи, и трогать их от «Метаданных» незачем; `any` запоминает
        // выбор, и вот его пропуск как раз ничем себя не выдаст — настройка
        // просто перестанет переживать перезапуск. Оба применяются **после**
        // отрисовки: `self` до конца замыкания занят.
        let mut subs_changed = false;
        let mut any_changed = false;

        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            any_changed |= chip(ui, &mut self.options.embed_metadata, "Метаданные", true)
                .on_hover_text("Название, автор и дата уедут в сам файл.")
                .changed();
            any_changed |= chip(ui, &mut self.options.embed_thumbnail, "Обложку", true)
                .on_hover_text("Картинка ролика станет обложкой файла.")
                .changed();
            subs_changed |= chip(ui, &mut self.options.embed_subs, "Субтитры", subs_enabled)
                .on_disabled_hover_text("Субтитры бывают только у видео — выберите MP4.")
                .changed();

            // Подчинённый чип появляется вместе с субтитрами, а не висит
            // выключенным рядом: без «Субтитров» он не значит ничего.
            if subs_enabled && self.options.embed_subs {
                subs_changed |= chip(
                    ui,
                    &mut self.options.auto_subs,
                    "Можно автоматические",
                    true,
                )
                .on_hover_text(
                    "Распознанные роботом субтитры лучше, чем никаких, но \
                     опечатки и слипшиеся слова там обычное дело.",
                )
                .changed();
            }
        });

        // Оговорка про ffmpeg — статическая строка: в кадре ничего не собирается.
        if self.ffmpeg_missing && self.options.any() {
            ui.add_space(8.0);
            note(
                ui,
                "Вшивать нечем: ffmpeg не найден. Файл скачается, но без \
                 метаданных, обложки и субтитров.",
                theme::STATE_WARNING,
            );
        }

        if subs_changed {
            self.rebuild_subtitles();
            // Язык субтитров показан в тонких настройках, и его строка
            // в их заголовке зависит от того, просят ли субтитры вообще.
            self.rebuild_advanced_summary();
        }

        if any_changed || subs_changed {
            self.remember();
        }
    }

    /// Тонкие настройки: фрагмент, вход на сайт, язык субтитров.
    ///
    /// Свёрнуты по умолчанию. Все трое нужны редко, а места занимали больше
    /// половины карточки: два поля времени с абзацем объяснения, список
    /// браузеров с абзацем и список из полутора сотен языков. Сводка в
    /// заголовке говорит, что из этого сейчас включено, — без неё свёрнутая
    /// группа прятала бы заданный фрагмент, и человек потом искал бы, почему
    /// ролик скачался куском.
    fn advanced_group(&mut self, ui: &mut egui::Ui) {
        let open = self.advanced;
        let mut toggled = false;

        egui::Frame::new()
            .fill(theme::CARD_INNER)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_INNER))
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                toggled = disclosure_row(ui, open, "Тонкие настройки", &self.advanced_summary);

                if !open {
                    return;
                }

                egui::Frame::new()
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 4,
                        bottom: 12,
                    })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());

                        field_label(ui, "Фрагмент");
                        self.section_row(ui);

                        ui.add_space(14.0);
                        field_label(ui, "Вход на сайт");
                        self.cookie_selector(ui);

                        // Список языков нужен, только если субтитры просят:
                        // в остальное время он не значит ничего.
                        if self.format == Format::Mp4 && self.options.embed_subs {
                            ui.add_space(14.0);
                            field_label(ui, "Язык субтитров");
                            if self.sub_lang_selector(ui) {
                                self.rebuild_subtitles();
                                self.rebuild_advanced_summary();
                            }
                        }
                    });
            });

        if toggled {
            self.advanced = !open;
        }
    }

    /// Выпадающий список языков субтитров.
    ///
    /// Возвращает `true`, когда выбор поменяли: пересобирать подпись и
    /// оговорку — работа обработчика, а не кадра отрисовки.
    ///
    /// Пункты берутся из ответа `probe`, то есть появляются через секунду
    /// после того, как в поле вставили ссылку (см. [`Preview`]). Пустого
    /// списка при этом не бывает — «Язык ролика» есть всегда, и он же
    /// значение по умолчанию, так что и до ответа выбор осмыслен.
    fn sub_lang_selector(&mut self, ui: &mut egui::Ui) -> bool {
        // Ширину берём до `ComboBox`, как и у списка браузеров: внутри он
        // заводит свою горизонтальную раскладку.
        let width = ui.available_width();
        let allow_auto = self.options.auto_subs;
        let mut changed = false;

        ui.scope(|ui| {
            let v = ui.visuals_mut();
            // То же оформление, что у списка браузеров: это такое же поле
            // ввода, и разъехаться им нельзя.
            for state in [&mut v.widgets.inactive, &mut v.widgets.open] {
                state.weak_bg_fill = theme::INPUT_FILL;
            }
            v.widgets.hovered.weak_bg_fill = theme::INPUT_FILL;

            // Поля берём по отдельности: внутри замыкания `sub_lang` нужен
            // изменяемым, а `info` — нет, и целиком `self` там занять нельзя.
            let info = self.preview.info.as_ref();
            let lang = &mut self.sub_lang;

            egui::ComboBox::from_id_salt("savio-sub-lang")
                .selected_text(self.sub_lang_label.as_str())
                .width(width)
                .height(SUBLANG_LIST_HEIGHT)
                .show_ui(ui, |ui| {
                    let spacing = ui.spacing_mut();
                    spacing.interact_size.y = 26.0;
                    spacing.button_padding.y = 3.0;
                    spacing.item_spacing.y = 2.0;

                    // Первым — то, что нужно почти всегда, и единственный
                    // пункт, который есть до первого `probe`.
                    let selected = matches!(lang, SubLang::Original);
                    if ui
                        .selectable_label(selected, SubLang::ORIGINAL_LABEL)
                        .clicked()
                        && !selected
                    {
                        *lang = SubLang::Original;
                        changed = true;
                    }

                    // Заголовок раздела ставим на смене вида дорожек.
                    // Список приходит уже разделённым: сначала авторские,
                    // потом автоматические, — так что перелом ровно один.
                    let mut group: Option<bool> = None;
                    let tracks = info.map(|info| info.subtitle_tracks(allow_auto));
                    for track in tracks.into_iter().flatten() {
                        if group != Some(track.auto) {
                            group = Some(track.auto);
                            ui.label(
                                egui::RichText::new(if track.auto {
                                    "Автоматические"
                                } else {
                                    "Свои"
                                })
                                .small()
                                .color(theme::TEXT_MUTED),
                            );
                        }

                        // `selectable_label`, а не `selectable_value`:
                        // последний берёт значение по значению, то есть
                        // требовал бы копии кода на каждый пункт в каждом
                        // кадре — а пунктов у YouTube полторы сотни.
                        let selected = matches!(lang, SubLang::Code(code) if *code == track.code);
                        if ui.selectable_label(selected, track.label.as_str()).clicked()
                            && !selected
                        {
                            *lang = SubLang::Code(track.code.clone());
                            changed = true;
                        }
                    }
                });
        });

        ui.add_space(6.0);
        if !self.subs_note.is_empty() {
            note(ui, &self.subs_note, theme::STATE_WARNING);
            ui.add_space(6.0);
        }
        // Обе строки статические, и обе нужны: про качество робота человек
        // должен узнать здесь, а не по готовому файлу.
        if allow_auto {
            note(
                ui,
                "Автоматические субтитры распознаёт робот: опечатки, слипшиеся \
                 слова и пропущенные знаки препинания там обычное дело. \
                 «Язык ролика» берёт распознанный оригинал, любой другой \
                 язык — машинный перевод с него, и он ещё хуже.",
                theme::TEXT_MUTED,
            );
        } else {
            note(
                ui,
                "Свои субтитры выкладывает автор ролика, и они точные — но \
                 есть далеко не у всех. Список языков заполняется сам, \
                 через секунду после того, как вы вставите ссылку.",
                theme::TEXT_MUTED,
            );
        }

        changed
    }

    /// Выпадающий список «откуда взять вход на сайт»: браузер или файл.
    ///
    /// Список, а не поле ввода: имена браузеров принадлежат yt-dlp, их список
    /// закрытый, и опечатка в нём обернулась бы английской руганью вместо
    /// загрузки.
    ///
    /// Оговорка под ним меняется вместе с выбором и в каждом случае статична —
    /// в кадре отрисовки здесь ничего не собирается.
    fn cookie_selector(&mut self, ui: &mut egui::Ui) {
        // Ширину берём до `ComboBox`: внутри он заводит свою горизонтальную
        // раскладку, и `available_width` там уже другая.
        let width = ui.available_width();
        // С каким входом спрашивали — часть вопроса: у YouTube ответ с cookies
        // и без них отличается вплоть до пустого списка дорожек. Сменили
        // браузер — прежнее превью уже не про этот запрос.
        let before = self.cookies;

        ui.scope(|ui| {
            let v = ui.visuals_mut();
            // Список — такое же поле ввода, как ссылка и дорожки
            // переключателей, поэтому и «утоплен» глубже карточки. Иначе на
            // заливке `BG_SURFACE` он держался бы на одной тонкой рамке.
            // `open` в списке обязателен: пока раскрыт список, egui рисует
            // кнопку именно этим состоянием, и без него она бы перекрашивалась
            // в момент нажатия.
            for state in [&mut v.widgets.inactive, &mut v.widgets.open] {
                state.weak_bg_fill = theme::INPUT_FILL;
            }
            v.widgets.hovered.weak_bg_fill = theme::INPUT_FILL;

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

        if self.cookies != before {
            // Пункт «Из файла…» без файла не значит ничего, поэтому диалог
            // открываем сразу, а не оставляем человека гадать, где выбрать
            // файл. Отказ от диалога возвращает прежний выбор: пустой «Из
            // файла…» выглядел бы включённым входом, а вход при нём не
            // передавался бы вовсе — ровно тот молчаливый отказ, которого
            // не должно быть.
            if self.cookies == CookieSource::File && self.cookie_file.is_none() {
                self.pick_cookie_file();
                if self.cookie_file.is_none() {
                    self.cookies = before;
                }
            }
            self.retarget_preview(ui.ctx());
            self.rebuild_advanced_summary();
        }

        if self.cookies == CookieSource::File {
            ui.add_space(6.0);
            self.cookie_file_row(ui);
        }

        ui.add_space(6.0);
        match self.cookies {
            CookieSource::None => note(
                ui,
                "Для возрастных, приватных и «подтвердите, что вы не робот» \
                 роликов: Savio возьмёт ваш вход на сайт из браузера или из \
                 файла. Обычные ссылки скачиваются и без этого.",
                theme::TEXT_MUTED,
            ),
            CookieSource::File => note(
                ui,
                "Нужен файл формата Netscape — такой выгружает расширение \
                 браузера вроде «Get cookies.txt». После загрузки yt-dlp \
                 допишет в него свежие cookies. И то же, что с браузером: \
                 у YouTube cookies чаще мешают — перестало скачиваться, \
                 верните «Не использовать».",
                theme::STATE_WARNING,
            ),
            _ => note(
                ui,
                "Закройте браузер перед загрузкой: пока он открыт, файл cookies \
                 занят и не читается. И учтите: у YouTube cookies чаще мешают — \
                 сайт отвечает пустым списком дорожек. Перестало скачиваться — \
                 верните «Не использовать».",
                theme::STATE_WARNING,
            ),
        }
    }

    /// Строка с выбранным файлом cookies: кнопка во всю ширину с самим путём.
    ///
    /// Устроена как `folder_row`, и по той же причине: пара «подпись + кнопка»
    /// в узкой колонке разъезжается, а путь длинный почти всегда. Полный путь
    /// с кнопки, как и там, не посмотреть — `show_tooltip_when_elided` есть
    /// только у `Label` (дефект 48 реестра).
    ///
    /// Отдельного вида «файл не выбран» здесь нет, и это не упущение: пункт
    /// «Из файла…» сам открывает диалог, а отказ от диалога возвращает
    /// прежний выбор (см. `cookie_selector`), так что при выбранном источнике
    /// файл есть всегда. На случай, если это когда-нибудь перестанет быть
    /// правдой, на кнопке стоит [`PICK_COOKIE_FILE`] — приглашение сделать
    /// ровно то, что она и делает.
    fn cookie_file_row(&mut self, ui: &mut egui::Ui) {
        let clicked = ui
            .add_sized(
                [ui.available_width(), theme::CONTROL_HEIGHT],
                egui::Button::new(
                    egui::RichText::new(&self.cookie_file_display)
                        .color(theme::TEXT_SECONDARY),
                )
                .truncate(),
            )
            .on_hover_text("Откуда взять вход на сайт. Нажмите, чтобы выбрать другой файл.")
            .clicked();

        if clicked {
            self.pick_cookie_file();
            // Файл — часть вопроса к сайту наравне со ссылкой: сменили
            // файл, значит прежний ответ уже не про этот вход.
            self.retarget_preview(ui.ctx());
        }
    }

    /// Спрашивает файл cookies. Отказ от диалога ничего не меняет: прежний
    /// выбор остаётся на месте.
    ///
    /// Диалог блокирует поток, то есть и кадр, — как и выбор папки
    /// сохранения. Это законно: пока открыт модальный диалог системы,
    /// рисовать окну всё равно нечего.
    fn pick_cookie_file(&mut self) {
        let Some(file) = rfd::FileDialog::new()
            // Фильтр по расширению, но не единственный: расширения у выгрузки
            // разные, и запереть человека в `*.txt` значило бы не дать
            // выбрать свой же файл.
            .add_filter("Файлы cookies", &["txt"])
            .add_filter("Все файлы", &["*"])
            .pick_file()
        else {
            return;
        };

        self.cookie_file_display = file.display().to_string();
        self.cookie_file = Some(file);
    }

    /// Куда класть готовые файлы: одна кнопка во всю ширину с самим путём.
    ///
    /// Путь на самой кнопке, а не подписью рядом: пары «кнопка + подпись»
    /// в узкой колонке разъезжаются, а путь длинный почти всегда. Длинный
    /// путь при этом обрезается, и целиком его сейчас взять негде:
    /// `show_tooltip_when_elided` есть только у `Label`, а на кнопке висит
    /// поясняющая подсказка (дефект 48 реестра).
    fn folder_row(&mut self, ui: &mut egui::Ui) {
        // Папки нет — это не оговорка, а помеха работе: «Скачать» без неё
        // выключена. Поэтому предупреждающий тон, а не приглушённый.
        let color = if self.out_dir.is_some() {
            theme::TEXT_SECONDARY
        } else {
            theme::STATE_WARNING
        };

        let clicked = ui
            .add_sized(
                [ui.available_width(), theme::CONTROL_HEIGHT],
                egui::Button::new(egui::RichText::new(&self.out_dir_display).color(color))
                    .truncate(),
            )
            .on_hover_text("Куда сохранять готовые файлы. Нажмите, чтобы выбрать другую папку.")
            .clicked();

        if clicked && let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.out_dir_display = display_dir(Some(&dir));
            self.out_dir = Some(dir);
            self.remember();
        }
    }

    /// Ряд действий: «В очередь» слева, «Скачать» (или «Отмена») справа.
    ///
    /// Две кнопки в строку, а не одна во всю ширину, как было до очереди:
    /// докладывать ссылки нужно и во время загрузки, а второй ряд удлинил бы
    /// и без того длинную карточку. Ширины неравные — главное действие экрана
    /// остаётся заметно крупнее.
    fn action_button(&mut self, ui: &mut egui::Ui) {
        // Подсказку выключенной кнопки выбираем по первой же причине, а не
        // по всем сразу: человеку нужно знать, что сделать сейчас.
        let add_hint = if self.queue.full {
            "Очередь заполнена: дождитесь, пока что-нибудь скачается."
        } else if self.url.trim().is_empty() {
            "Вставьте ссылку — она встанет в конец очереди."
        } else if self.section_error.is_some() {
            "Поправьте границы фрагмента."
        } else if self.out_dir.is_none() {
            "Сначала выберите папку сохранения."
        } else {
            "Сначала нужен yt-dlp."
        };
        let can_enqueue = self.can_enqueue();

        let mut enqueue_clicked = false;
        let mut primary_clicked = false;

        ui.horizontal(|ui| {
            const GAP: f32 = 10.0;
            ui.spacing_mut().item_spacing.x = GAP;
            // Треть — «В очередь», остаток — главному действию. Остаток,
            // а не вторая доля от деления: два округления до пиксельной
            // сетки не дотянули бы ряд до правого края.
            let secondary = ((ui.available_width() - GAP) / 3.0).max(90.0);

            enqueue_clicked = ui
                .add_enabled(
                    can_enqueue,
                    egui::Button::new("В очередь")
                        .min_size(egui::vec2(secondary, theme::CTA_HEIGHT)),
                )
                .on_hover_text(
                    "Ссылка встанет в конец очереди, а поле освободится под \
                     следующую. Качаются они по одной, сверху вниз.",
                )
                .on_disabled_hover_text(add_hint)
                .clicked();

            primary_clicked = self.primary_button(ui, ui.available_width());
        });

        if enqueue_clicked && self.enqueue() {
            // Поле освобождаем сразу: «В очередь» затем и нажимают, чтобы
            // вставить следующую ссылку. У «Скачать» этого нет — там поле
            // остаётся, как оно оставалось и до появления очереди.
            self.url.clear();
            self.url_invalid = false;
        }
        if primary_clicked {
            let ctx = ui.ctx().clone();
            if matches!(self.state, State::Running) {
                self.cancel();
            } else {
                self.start(&ctx);
            }
        }

        if self.queue.full {
            ui.add_space(6.0);
            note(
                ui,
                "В очереди больше некуда: полсотни ссылок ещё ждут. Как только \
                 хоть одна скачается, место освободится само.",
                theme::STATE_WARNING,
            );
        }
    }

    /// Главная кнопка ряда: «Отмена» во время загрузки, «Скачать» в остальное
    /// время. Возвращает `true`, когда её нажали.
    fn primary_button(&mut self, ui: &mut egui::Ui, width: f32) -> bool {
        if matches!(self.state, State::Running) {
            return ui
                .add_sized([width, theme::CTA_HEIGHT], egui::Button::new("Отмена"))
                .on_hover_text(
                    "Остановит идущую загрузку. Остальные ссылки останутся \
                     в очереди — «Скачать» продолжит с того же места.",
                )
                .clicked();
        }

        let enabled = self.can_start();
        // Подсказка выключенной кнопке нужна не меньше, чем соседней: до
        // очереди она молча гасла, и понять почему было неоткуда.
        let hint = if self.setup_error.is_some() {
            "Сначала нужен yt-dlp."
        } else if self.section_error.is_some() {
            "Поправьте границы фрагмента."
        } else if self.out_dir.is_none() {
            "Сначала выберите папку сохранения."
        } else {
            "Вставьте ссылку или поставьте что-нибудь в очередь."
        };

        ui.scope(|ui| {
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
                state.corner_radius = egui::CornerRadius::same(theme::RADIUS_PILL);
                state.expansion = 0.0;
            }
            // Двойное ослабление не нужно: приглушённый оранжевый уже задан
            // явно, а поверх него прозрачность съела бы кнопку целиком.
            v.disabled_alpha = 1.0;

            ui.add_enabled(
                enabled,
                egui::Button::new(
                    egui::RichText::new("Скачать")
                        .font(theme::display(17.0))
                        .color(theme::TEXT_ON_ACCENT),
                )
                .min_size(egui::vec2(width, theme::CTA_HEIGHT)),
            )
            .on_disabled_hover_text(hint)
            .clicked()
        })
        .inner
    }

    /// Карточка хода работы: что сейчас происходит с загрузкой.
    ///
    /// Название с обложкой отсюда переехали под поле ссылки: они про то,
    /// что собираются скачать, и известны ещё до нажатия «Скачать». Здесь
    /// остаётся только ход самой работы, а какой ролик качается — видно
    /// в строке очереди ниже, где название и так стоит и подсвечено.
    fn status_section(&mut self, ui: &mut egui::Ui) {
        let (label, color) = self.status();
        // Куда открывать папку, решаем после карточки: внутри замыкания
        // `self` занят целиком, а `open_dir` запускает процесс.
        let mut open_at: Option<PathBuf> = None;

        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 9.0;
                // Точка — подсказка глазу, а не носитель смысла: то же
                // состояние сказано словом рядом.
                let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.5, color);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .font(theme::display(17.0))
                            .color(theme::TEXT_PRIMARY),
                    )
                    .truncate(),
                );
            });

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
                        // Обрезаем, а не переносим: строка «стадия · проценты ·
                        // объём · скорость · остаток» в колонке шириной 340
                        // легла бы в две строки и дёргала бы высоту карточки
                        // на каждом кадре загрузки. Полную строку показывает
                        // подсказка самой обрезанной метки.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.progress_line)
                                    .small()
                                    .color(theme::TEXT_SECONDARY),
                            )
                            .truncate(),
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
                        );
                        ui.add_space(10.0);
                    }
                    if let Some(dir) = path.parent()
                        && pill_button(ui, "Открыть папку").clicked()
                    {
                        open_at = Some(dir.to_path_buf());
                    }
                }
                State::Failed(err) => banner(ui, err, theme::STATE_ERROR),
                State::Cancelled => note(ui, "Загрузка отменена.", theme::TEXT_SECONDARY),
                State::Queued => note(
                    ui,
                    "Ссылки ждут в очереди. Нажмите «Скачать» — они пойдут \
                     по одной, сверху вниз.",
                    theme::TEXT_SECONDARY,
                ),
                State::Idle => note(
                    ui,
                    "Вставьте ссылку и нажмите «Скачать».",
                    theme::TEXT_SECONDARY,
                ),
            }
        });

        if let Some(dir) = open_at {
            open_dir(&dir);
        }
    }

    /// Правая колонка снизу: очередь или история, на выбор.
    ///
    /// Вместе, а не двумя карточками подряд: обе про одни и те же ссылки,
    /// только в разное время, и одновременно нужна ровно одна из них.
    /// История сюда переехала из собственной вкладки — там она отнимала
    /// место в дорожке разделов, а смотрят в неё как раз во время загрузки.
    fn rail_list(&mut self, ui: &mut egui::Ui) {
        // Что нажали, решаем после отрисовки: менять список, пока по нему
        // идёт цикл, нельзя, а откладывать решение до следующего кадра —
        // значит терять его при быстром щелчке.
        let mut remove: Option<DownloadId> = None;
        let mut clear = false;
        let mut picked = None;
        let mut open_at: Option<PathBuf> = None;

        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                theme::track_frame().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (tab, label) in
                            [(RailTab::Queue, "Очередь"), (RailTab::History, "История")]
                        {
                            if segment_button(ui, label, self.rail_tab == tab, 0.0) {
                                picked = Some(tab);
                            }
                        }
                    });
                });

                // «Очистить» прижата к правому краю и есть только у очереди:
                // история за этот запуск — единственный след того, куда что
                // легло, и стирать её кнопкой рядом со списком опасно.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.rail_tab == RailTab::Queue && !self.queue.items.is_empty() {
                        clear = pill_button(ui, "Очистить")
                            .on_hover_text(
                                "Список опустеет: уйдут и скачанные, и те, что ещё \
                                 ждут. Идущая загрузка не прервётся — её \
                                 останавливает «Отмена».",
                            )
                            .clicked();
                    }
                });
            });

            ui.add_space(12.0);

            match self.rail_tab {
                RailTab::Queue => remove = self.queue_list(ui),
                RailTab::History => open_at = self.history_list(ui),
            }
        });

        if let Some(tab) = picked {
            self.rail_tab = tab;
        }
        if let Some(dir) = open_at {
            open_dir(&dir);
        }

        let emptied = remove.is_some() || clear;
        if let Some(id) = remove {
            self.queue.remove(id);
        }
        if clear {
            self.queue.clear();
        }

        // Экран не должен пережить очередь. Убрали последнее ожидающее — и
        // «В очереди» на плашке становится враньём, а совет под ней («нажмите
        // „Скачать“ — они пойдут по одной») указывает на кнопку, которую
        // `can_start()` к этому моменту уже погасил: ждать-то нечего. Сам
        // список при этом с экрана исчезает, так что человек читает про
        // очередь, которой не видит. Ни сборка, ни `clippy`, ни тесты этого
        // не ловят — только глаза.
        if emptied && matches!(self.state, State::Queued) && !self.queue.has_waiting() {
            self.state = State::Idle;
        }
    }

    /// Содержимое половины «Очередь». Возвращает строку, которую убрали.
    fn queue_list(&self, ui: &mut egui::Ui) -> Option<DownloadId> {
        if self.queue.items.is_empty() {
            note(
                ui,
                "Пока пусто. «В очередь» кладёт сюда ссылку из поля и \
                 освобождает поле под следующую.",
                theme::TEXT_MUTED,
            );
            return None;
        }

        let mut remove = None;

        // Подсказку с полной сводкой вешает сама обрезанная метка
        // (`show_tooltip_when_elided`) — свой `on_hover_text` рядом дал бы
        // вторую коробку с тем же текстом (дефект 22).
        ui.add(
            egui::Label::new(
                egui::RichText::new(&self.queue.summary)
                    .small()
                    .color(theme::TEXT_MUTED),
            )
            .truncate(),
        );
        ui.add_space(10.0);

        // Строки лежат прямо в общей прокрутке, своей у списка нет — и это
        // не косметика. Вложенная вертикальная прокрутка берёт высоту из
        // `available_rect_before_wrap()`, а внутри другой прокрутки это не
        // «сколько влезет в окно», а «сколько осталось от её видимой части».
        // Карточка лежит низко, остаток к ней нулевой — и список схлопнулся
        // бы до `min_scrolled_size`, то есть до 64 точек (дефект 27).
        for (index, item) in self.queue.items.iter().enumerate() {
            if index > 0 {
                ui.add_space(8.0);
            }
            if queue_row(ui, item) {
                remove = Some(item.id);
            }
        }

        ui.add_space(10.0);
        note(
            ui,
            "Качаются по одной, сверху вниз. Сорвавшаяся не останавливает \
             остальные. На диск список не пишется и при закрытии Savio \
             исчезает.",
            theme::TEXT_MUTED,
        );

        remove
    }

    /// Содержимое половины «История». Возвращает папку, которую попросили
    /// открыть.
    fn history_list(&self, ui: &mut egui::Ui) -> Option<PathBuf> {
        let Some((first, rest)) = self.history.entries.split_first() else {
            // Пустой экран без объяснения читается как поломка. Про то, что
            // список не переживает закрытие окна, говорим здесь же: иначе
            // после перезапуска пустая история выглядит потерянными данными.
            note(
                ui,
                "Пока пусто. Сюда попадёт всё, что вы скачаете за этот \
                 запуск, — с кнопкой, открывающей папку файла. На диск \
                 список не пишется и при закрытии Savio очищается.",
                theme::TEXT_MUTED,
            );
            return None;
        };

        let mut open_at = None;
        if let Some(dir) = self.history_card(ui, first) {
            open_at = Some(dir);
        }
        for entry in rest {
            ui.add_space(8.0);
            if let Some(dir) = self.history_card(ui, entry) {
                open_at = Some(dir);
            }
        }
        open_at
    }

    /// Тело журнала: кнопка «Скопировать» и сами строки.
    ///
    /// Живёт в подвале, а не в прокрутке содержимого, и это важно: вложенная
    /// прокрутка внутри другой схлопывается до 64 точек (дефект 27). Подвал —
    /// панель, своя прокрутка в нём законна.
    fn log_section(&mut self, ui: &mut egui::Ui) {
        self.log_copy_row(ui);
        ui.add_space(8.0);

        egui::Frame::new()
            .fill(theme::INPUT_FILL)
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_INNER))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Высота прибита с двух сторон, и это не перестраховка.
                // `ScrollArea` берёт высоту из `available_rect_before_wrap()`,
                // а внутри панели, которая сама растёт по содержимому, это
                // не «сколько влезет в окно», а «сколько панель заняла в
                // прошлом кадре». Пока журнал был закрыт, панель была
                // низкой — и прокрутка получала остаток от неё, панель от
                // этого не росла, и так по кругу: журнал застревал на
                // 75 точках вместо 150 и обрезался кромкой окна. Ровно тот
                // же механизм, что в дефекте 27, только там роль панели
                // играла внешняя прокрутка. `min_scrolled_height` разрывает
                // круг: панели приходится вырасти под него.
                egui::ScrollArea::vertical()
                    .max_height(LOG_HEIGHT)
                    .min_scrolled_height(LOG_HEIGHT)
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

            let copied = pill_button(ui, "Скопировать")
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
    /// Вкладка «Метаданные»: работа слева, объяснения справа.
    ///
    /// Две колонки по той же причине, что у загрузки: предупреждение о
    /// перезаписи и список поддерживаемых форматов — это то, что читают
    /// **до** нажатия «Удалить», и под самой кнопкой их не видно.
    fn metadata_tab(&mut self, ui: &mut egui::Ui) {
        const GAP: f32 = 18.0;
        if ui.available_width() < theme::TWO_COLUMN_MIN {
            self.metadata_main(ui);
            ui.add_space(GAP);
            metadata_rail(ui);
            return;
        }

        let total = ui.available_width();
        let rail = theme::RAIL_WIDTH;
        let main = total - rail - GAP;

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            ui.allocate_ui_with_layout(
                egui::vec2(main, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width(main);
                    ui.set_max_width(main);
                    self.metadata_main(ui);
                },
            );
            ui.allocate_ui_with_layout(
                egui::vec2(rail, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width(rail);
                    ui.set_max_width(rail);
                    metadata_rail(ui);
                },
            );
        });
    }

    /// Главная колонка вкладки: файл, кнопки и итог.
    fn metadata_main(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            ui.label(
                egui::RichText::new("Что файл рассказывает о вас")
                    .font(theme::display(21.0))
                    .color(theme::TEXT_PRIMARY),
            );
            ui.add_space(6.0);
            note(
                ui,
                "Модель камеры, дата съёмки, координаты места, автор, обложка \
                 альбома. Savio читает это и стирает, не пересжимая ни \
                 картинку, ни звук.",
                theme::TEXT_MUTED,
            );

            ui.add_space(16.0);
            self.meta_file_row(ui);

            if let Some(blocked) = &self.meta.blocked {
                ui.add_space(12.0);
                banner(ui, blocked, theme::STATE_WARNING);
            }

            ui.add_space(16.0);
            self.meta_buttons(ui);

            ui.add_space(14.0);
            self.meta_status(ui);
        });
    }

    /// Какой файл разбираем: одна кнопка во всю ширину с самим путём —
    /// как «Папка сохранения» на соседней вкладке. Одинаковые по смыслу
    /// пары должны выглядеть одинаково.
    fn meta_file_row(&mut self, ui: &mut egui::Ui) {
        let color = if self.meta.path.is_some() {
            theme::TEXT_SECONDARY
        } else {
            theme::TEXT_MUTED
        };

        let clicked = ui
            .add_enabled(
                !self.meta.busy,
                egui::Button::new(egui::RichText::new(&self.meta.path_display).color(color))
                    .truncate()
                    .min_size(egui::vec2(ui.available_width(), theme::FIELD_HEIGHT)),
            )
            .on_hover_text("Нажмите, чтобы выбрать MP3 или изображение.")
            .clicked();

        if clicked
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
                egui::Button::new("Прочитать").min_size(egui::vec2(width, theme::CTA_HEIGHT)),
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
                        state.corner_radius = egui::CornerRadius::same(theme::RADIUS_PILL);
                        state.expansion = 0.0;
                    }
                    v.disabled_alpha = 1.0;

                    ui.add_enabled(
                        clean_on,
                        egui::Button::new(
                            egui::RichText::new("Стереть всё")
                                .font(theme::display(17.0))
                                .color(theme::TEXT_ON_ACCENT),
                        )
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
                "Выберите MP3 или изображение. «Прочитать» покажет, что \
                 записано в файле, «Стереть всё» — уберёт теги, геометку \
                 и обложку, не трогая само содержимое.",
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
                    .fill(theme::MODAL_FILL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(theme::RADIUS_CARD))
                    .inner_margin(egui::Margin::same(24)),
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
                                    // станет заметно тяжелее. В 150 точек имя
                                    // тега не влезает почти никогда, так что
                                    // обрезается тут почти всё.
                                    //
                                    // Своего `on_hover_text` рядом быть не
                                    // должно: у `Label` включён по умолчанию
                                    // `show_tooltip_when_elided`, и обрезанная
                                    // метка сама вешает подсказку с полным
                                    // именем. Свой вызов её не заменяет, а
                                    // добавляет вторую — egui считает подсказки
                                    // на виджет и ставит их одна под другой,
                                    // выходит две коробки с одним и тем же
                                    // текстом (дефект 22).
                                    ui.add_sized(
                                        [150.0, ui.text_style_height(&egui::TextStyle::Body)],
                                        egui::Label::new(
                                            egui::RichText::new(&tag.name)
                                                .small()
                                                .color(theme::TEXT_MUTED),
                                        )
                                        .truncate(),
                                    );

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
                pill_button(ui, "Закрыть").clicked()
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
                    .fill(theme::MODAL_FILL)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
                    .corner_radius(egui::CornerRadius::same(theme::RADIUS_CARD))
                    .inner_margin(egui::Margin::same(24)),
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
    /// Половина «Состав»: что за машина и в каком она состоянии.
    ///
    /// Опрос запускается сам при первом открытии вкладки. Кнопка с пустым
    /// экраном была бы лишним шагом: сюда заходят ровно за ответом, и
    /// нажимать «Проверить», чтобы его увидеть, незачем.
    fn system_tab(&mut self, ui: &mut egui::Ui) {
        if !self.system.asked {
            let ctx = ui.ctx().clone();
            let gpu = self.gpu.clone();
            self.system.start(gpu, &ctx);
        }

        self.system_header(ui);
        ui.add_space(16.0);

        if self.system.busy {
            note(ui, &self.system.stage, theme::TEXT_SECONDARY);
            return;
        }

        let Some(report) = &self.system.report else {
            // Приёмник умер, а отчёт не приехал: поток сорвался, не отправив
            // ничего. Показать «в порядке» тут нельзя — мы ничего не узнали.
            note(
                ui,
                "Опрос не дал ответа. Попробуйте «Проверить снова».",
                theme::TEXT_MUTED,
            );
            return;
        };

        for check in &report.checks {
            check_card(ui, check);
            ui.add_space(8.0);
        }
    }

    /// Шапка вкладки: общий итог и две кнопки.
    fn system_header(&mut self, ui: &mut egui::Ui) {
        let mut again = false;
        let mut save = false;

        theme::card(ui, |ui| {
            // Итог — обычной строкой с переносом: он длинный, а в
            // горизонтальной раскладке egui положил бы его в одну строку
            // любой длины и срезал кромкой окна.
            match &self.system.report {
                Some(report) => note(ui, &report.headline(), theme::TEXT_SECONDARY),
                None => note(ui, "Сведения о железе этой машины.", theme::TEXT_SECONDARY),
            }

            ui.add_space(6.0);
            // Оговорка про то, чего в отчёте нет. Без неё «нет данных»
            // у половины пунктов выглядит поломкой Savio, а не отказом
            // системы: человеку неоткуда узнать, что температуры и SMART
            // без прав администратора недоступны в принципе.
            note(
                ui,
                "Показано то, что система отдаёт без прав администратора. \
                 Температуры, обороты вентиляторов и SMART накопителей \
                 сюда не входят: без элевации их нельзя прочитать честно, \
                 а показывать выдуманные значения хуже, чем не показывать \
                 ничего.",
                theme::TEXT_MUTED,
            );

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                again = ui
                    .add_enabled(!self.system.busy, pill("Проверить снова"))
                    .clicked();
                save = ui
                    .add_enabled(
                        !self.system.busy && self.system.report.is_some(),
                        pill("Сохранить отчёт…"),
                    )
                    .on_disabled_hover_text("Сначала дождитесь опроса.")
                    .clicked();
            });

            if let Some((text, color)) = &self.system.saved {
                ui.add_space(10.0);
                note(ui, text, *color);
            }
        });

        if again {
            let ctx = ui.ctx().clone();
            let gpu = self.gpu.clone();
            self.system.start(gpu, &ctx);
        }
        if save {
            self.save_report();
        }
    }

    /// Кладёт отчёт в файл.
    ///
    /// Запись идёт прямо здесь, в кадре, и это тот редкий случай, когда так
    /// можно: диалог выбора файла всё равно останавливает всё окно на время
    /// своего показа, а сам отчёт — несколько килобайт текста. Заводить ради
    /// него поток значило бы усложнить код там, где выигрыша нет.
    fn save_report(&mut self) {
        let Some(report) = &self.system.report else {
            return;
        };

        let Some(path) = rfd::FileDialog::new()
            .set_file_name("savio-система.txt")
            .add_filter("Текстовый файл", &["txt"])
            .save_file()
        else {
            // Диалог закрыли — это не отказ и не ошибка, говорить не о чем.
            return;
        };

        self.system.saved = Some(match std::fs::write(&path, report.to_text()) {
            Ok(()) => (
                format!("Отчёт сохранён: {}", path.display()),
                theme::STATE_SUCCESS,
            ),
            Err(err) => (
                format!("Не удалось сохранить отчёт: {err}"),
                theme::STATE_ERROR,
            ),
        });
    }

    /// Вкладка «Монитор»: что происходит с машиной прямо сейчас.
    fn monitor_tab(&mut self, ui: &mut egui::Ui) {
        self.monitor_header(ui);
        ui.add_space(14.0);

        // Питание — до показаний, а не после: это единственный орган
        // управления на всей половине, а показания под ним — ровно то, на
        // что он влияет. И до ожидания первого замера тоже: ждать секунду,
        // чтобы показать переключатель, незачем.
        self.power_card(ui);
        ui.add_space(14.0);

        let Some(sample) = &self.monitor.sample else {
            note(
                ui,
                "Замеряю… Первые числа появятся через секунду: загрузка — это \
                 разница между двумя замерами, и одной точки для неё мало.",
                theme::TEXT_SECONDARY,
            );
            return;
        };

        // Процессор и память рядом, когда есть место: это два одинаковых по
        // устройству показателя, и читать их проще парой, чем лестницей.
        if ui.available_width() >= theme::TWO_COLUMN_MIN {
            ui.columns(2, |columns| {
                metric_card(
                    &mut columns[0],
                    "Процессор",
                    &sample.cpu,
                    &self.monitor.cpu_trace,
                    theme::ACCENT,
                );
                metric_card(
                    &mut columns[1],
                    "Память",
                    &sample.mem,
                    &self.monitor.mem_trace,
                    theme::STATE_SUCCESS,
                );
            });
        } else {
            metric_card(
                ui,
                "Процессор",
                &sample.cpu,
                &self.monitor.cpu_trace,
                theme::ACCENT,
            );
            ui.add_space(12.0);
            metric_card(
                ui,
                "Память",
                &sample.mem,
                &self.monitor.mem_trace,
                theme::STATE_SUCCESS,
            );
        }
        ui.add_space(12.0);

        io_card(ui, sample, self.gpu.as_ref());
        ui.add_space(12.0);

        process_card(ui, &sample.procs);
    }

    /// Карточка «Питание»: чем машина питается сейчас и как это переключить.
    ///
    /// Нажатия не спрашивают подтверждения, и это решение: схема и режим
    /// питания меняются мгновенно и обратимо одним нажатием соседней кнопки.
    /// Переспрашивать Savio положено перед необратимым (так устроена чистка
    /// метаданных), а лишний вопрос там, где отменить можно тут же, только
    /// приучает жать «Да» не глядя.
    fn power_card(&mut self, ui: &mut egui::Ui) {
        let busy = self.power.busy;
        let mut refresh = false;
        let mut change = None;

        theme::card(ui, |ui| {
            // Ряду задаётся высота, и это не украшение вёрстки. `with_layout`
            // отдаёт потомку всю оставшуюся высоту карточки, а `Align::Center`
            // в горизонтальной раскладке «считает занятой» её целиком
            // (`Placer::advance_after_rects`: expand_to_include_rect(frame_rect)
            // «pretend we used whole frame»). Внутри прокрутки остаток — это
            // почти весь экран, поэтому строка заголовка уезжает в середину
            // карточки, а сама карточка вырастает во весь экран. Ровно так
            // сейчас и выглядят соседние `check_card` и `metric_card` —
            // это задача 42 реестра, и повторять её здесь незачем.
            // `ui.horizontal` этой беды лишён именно тем, что задаёт высоту
            // (`interact_size.y`).
            //
            // Кнопка при этом кладётся первой, справа налево: обрезаемый
            // заголовок иначе занял бы всю ширину и кнопка налезла бы на него.
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), theme::CONTROL_HEIGHT),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    refresh = ui
                        .add_enabled(!busy, pill("Обновить"))
                        .on_disabled_hover_text("Сначала дождитесь ответа системы.")
                        .clicked();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new("Питание")
                                    .font(theme::display(17.0))
                                    .color(theme::TEXT_PRIMARY),
                            )
                            .truncate(),
                        );
                    });
                },
            );

            if let Some(trouble) = &self.power.state.trouble {
                ui.add_space(6.0);
                note(ui, trouble, theme::TEXT_MUTED);
            }

            // Ответа ещё нет. Молчать здесь нельзя: пустая карточка с одним
            // заголовком выглядит поломкой, а не ожиданием.
            if self.power.state.is_blank() && self.power.state.trouble.is_none() {
                ui.add_space(6.0);
                note(ui, "Спрашиваю систему…", theme::TEXT_SECONDARY);
            }

            if !self.power.state.plans.is_empty() {
                ui.add_space(14.0);
                field_label(ui, "Схема электропитания");
                // Раскладка с переносом, а не дорожка сегментов: названий
                // бывает и шесть (вендорские схемы), длина у них любая, а
                // в окне шириной 520 даже три не встают в строку.
                //
                // Выключается ряд целиком, снаружи: `add_enabled_ui` на
                // каждой кнопке сломал бы перенос (см. `choice_pill`).
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        for plan in &self.power.state.plans {
                            let on = self.power.state.active == Some(plan.id);
                            // Нажатие на уже активную ничего не значит:
                            // просить систему переключиться на то, что и так
                            // работает, — бодрый отчёт о безделье.
                            if choice_pill(ui, &plan.name, on).clicked() && !on {
                                change = Some(power::Change::Plan(plan.id));
                            }
                        }
                    });
                });
            }

            if let PowerModes::Known { effective, .. } = self.power.state.modes {
                ui.add_space(14.0);
                field_label(ui, "Режим питания");
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        for mode in PowerMode::ALL {
                            // Четвёртый режим (ползунок Windows 10) рисуем,
                            // только если система в нём и есть: предложить
                            // пункт, которого нет в параметрах Windows 11,
                            // значит показать список, которого человек
                            // у себя не видел, а скрыть действующий —
                            // соврать о машине.
                            let on = effective == Some(mode);
                            if !mode.offered() && !on {
                                continue;
                            }
                            if choice_pill(ui, mode.label(), on).clicked() && !on {
                                change = Some(power::Change::Mode(mode));
                            }
                        }
                    });
                });

                // Ни одна кнопка не выбрана — это надо объяснить, иначе ряд
                // выглядит сломанным.
                if effective.is_none() {
                    ui.add_space(8.0);
                    note(
                        ui,
                        "Машина работает в режиме, которого Savio не знает, — \
                         поэтому ни одна кнопка не выбрана. Нажатие любой \
                         переключит машину в неё.",
                        theme::TEXT_MUTED,
                    );
                }

                if !self.power.hint.is_empty() {
                    ui.add_space(8.0);
                    note(ui, &self.power.hint, theme::STATE_WARNING);
                }
            }

            // Итог последнего нажатия — последней строкой карточки, у самых
            // кнопок: сказанное про переключение должно стоять там, где
            // переключали.
            if let Some((text, color)) = &self.power.outcome {
                ui.add_space(12.0);
                note(ui, text, *color);
            }
        });

        // Обе просьбы исполняем после карточки: внутри замыкания `self`
        // одолжен на чтение, и завести оттуда поток не выйдет.
        if refresh {
            let ctx = ui.ctx().clone();
            self.power.start(&ctx);
        }
        if let Some(change) = change {
            let ctx = ui.ctx().clone();
            self.power.change(change, &ctx);
        }
    }

    /// Шапка половины: чем монитор занят и как включить оверлей.
    fn monitor_header(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            note(
                ui,
                "Показания снимаются раз в секунду, пока открыта эта половина \
                 или включён оверлей. В остальное время Savio ничего не \
                 опрашивает и не тратит ни кадра.",
                theme::TEXT_SECONDARY,
            );

            ui.add_space(6.0);
            // Та же оговорка, что и в «Составе», и по той же причине: без неё
            // отсутствие видеокарты в списке выглядит недоделкой Savio,
            // а не отказом системы.
            note(
                ui,
                "Загрузки видеокарты здесь нет: система отдаёт её только \
                 через счётчики производительности, своих у каждой ОС и \
                 у каждого производителя, — а показывать выдуманное число \
                 хуже, чем не показывать ничего.",
                theme::TEXT_MUTED,
            );

            ui.add_space(14.0);
            checkbox(
                ui,
                &mut self.monitor.overlay,
                "Оверлей поверх других окон",
                true,
            );

            ui.add_space(6.0);
            let passthrough = checkbox(
                ui,
                &mut self.monitor.passthrough,
                "Пропускать щелчки мыши сквозь оверлей",
                self.monitor.overlay,
            );
            passthrough.on_disabled_hover_text("Сначала включите оверлей.");

            ui.add_space(10.0);
            note(
                ui,
                "Оверлей — обычное окно поверх остальных, и виден он только \
                 в оконных и безрамочных играх. В полноэкранном режиме его \
                 не будет: туда не пускают ни одно чужое окно. С пропуском \
                 щелчков оверлей нельзя ни передвинуть, ни закрыть его же \
                 кнопкой — только этой галочкой.",
                theme::TEXT_MUTED,
            );
        });
    }

    /// Одна строка истории.
    ///
    /// Карточка на каждую запись, а не одна на весь список: строки отделяются
    /// друг от друга сами, без разделителей, и список любой длины выглядит
    /// одинаково. Своей прокрутки здесь нет — вкладка целиком лежит в общей,
    /// и вложенная полоса рядом с внешней только мешала бы.
    fn history_card(&self, ui: &mut egui::Ui, entry: &HistoryEntry) -> Option<PathBuf> {
        let mut open_at = None;

        theme::inner_frame().show(ui, |ui| {
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
                        .font(theme::bold(14.0))
                        .color(theme::TEXT_PRIMARY),
                )
                .truncate(),
            );

            // Папки нет — значит, и открывать нечего: показываем только имя.
            let Some(dir) = &entry.dir else {
                return;
            };

            ui.add_space(8.0);
            // Раскладка справа налево: кнопка кладётся первой и занимает
            // ровно себя, а пути достаётся остаток строки. Слева направо
            // обрезаемая метка забирает всю ширину, и кнопка налезает на
            // неё — в колонке шириной 340 путь и «Открыть папку» вместе
            // не помещаются. Проверено глазами: текст уходил под кнопку.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 9.0;
                // Лежит ли файл на месте, не спрашиваем: это обращение
                // к диску, а `ui()` идёт 60 раз в секунду (Правило 1).
                // Папку могли переименовать или унести вместе с флешкой —
                // тогда об этом скажет проводник, и это честнее
                // выключенной без объяснения кнопки.
                if pill_button(ui, "Открыть папку").clicked() {
                    open_at = Some(dir.clone());
                }
                // Вложенная раскладка обязательна: в `right_to_left` метка
                // занимает ровно себя и прижимается к правому краю — без
                // неё путь сползал бы к кнопке, а слева оставалось бы
                // пустое место во всю ширину (та же грабля, что в
                // `process_row`).
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // Подсказку с полным путём, как и у имени выше, вешает
                    // сама обрезанная метка.
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&entry.dir_display)
                                .small()
                                .color(theme::TEXT_MUTED),
                        )
                        .truncate(),
                    );
                });
            });
        });

        open_at
    }
}

/// Правая колонка вкладки «Метаданные»: то, что читают до нажатия «Стереть».
///
/// Свободная функция, а не метод: ни одно из двух объяснений не зависит от
/// состояния приложения — обе карточки статические.
fn metadata_rail(ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 9.0;
            let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(dot.center(), 4.5, theme::STATE_WARNING);
            ui.add(
                egui::Label::new(
                    egui::RichText::new("Файл перезапишется")
                        .font(theme::display(17.0))
                        .color(theme::TEXT_PRIMARY),
                )
                .truncate(),
            );
        });
        ui.add_space(8.0);
        note(
            ui,
            "Копия рядом не создаётся, вернуть стёртое будет нельзя — поэтому \
             Savio переспросит. Пиксели и звуковые кадры при этом не \
             трогаются: вырезаются только служебные блоки, и на большом файле \
             это мгновенно.",
            theme::TEXT_SECONDARY,
        );
    });

    ui.add_space(14.0);

    theme::card(ui, |ui| {
        field_label(ui, "Что поддерживается");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(7.0, 7.0);
            for name in ["MP3", "JPG", "PNG", "WebP", "GIF"] {
                soft_pill(ui, name, theme::STATE_SUCCESS, theme::SUCCESS_SOFT);
            }
            soft_pill(
                ui,
                "TIFF — только чтение",
                theme::TEXT_MUTED,
                egui::Color32::TRANSPARENT,
            );
            soft_pill(
                ui,
                "видео — пока нет",
                theme::TEXT_MUTED,
                egui::Color32::TRANSPARENT,
            );
        });
    });
}

// ---------------------------------------------------------------------------
// Мелкие элементы
// ---------------------------------------------------------------------------

/// Один сегмент переключателя: выбранный — оранжевый, остальные прозрачные.
///
/// Цвета задаём через `visuals`, а не через `Button::fill`: последний,
/// по документации egui, отключает реакцию на наведение — кнопка выглядела бы
/// мёртвой. Одна функция на все дорожки сразу — разделы в шапке, формат,
/// качество, половины «Машины», очередь с историей: разъехавшись, одинаковые
/// на вид элементы смотрелись бы досадной небрежностью.
///
/// `width` — это **минимум**, а не потолок: egui не сжимает кнопку под
/// доступное место, а раздвигает раскладку. Ноль означает «по ширине текста»
/// и нужен там, где дорожка стоит посреди строки, а не растянута на всё окно.
fn segment_button(ui: &mut egui::Ui, label: &str, selected: bool, width: f32) -> bool {
    ui.scope(|ui| {
        // Поля сегмента урезаем против штатных 16: шесть ступеней качества
        // («2160p») в окне шириной 520 иначе вылезли бы за кромку.
        ui.spacing_mut().button_padding.x = 10.0;

        let v = ui.visuals_mut();
        let (rest, hover, press) = if selected {
            (theme::ACCENT, theme::ACCENT_HOVER, theme::ACCENT_ACTIVE)
        } else {
            (
                egui::Color32::TRANSPARENT,
                theme::CARD_INNER,
                theme::CARD_FILL,
            )
        };
        // Подпись выбранного сегмента тёмная — на оранжевом светлая даёт
        // 1.9:1. У невыбранного она приглушена в покое и светлеет под
        // курсором: это и есть отклик, заливки там почти нет.
        let (rest_text, hover_text) = if selected {
            (theme::TEXT_ON_ACCENT, theme::TEXT_ON_ACCENT)
        } else {
            (theme::TEXT_SECONDARY, theme::TEXT_PRIMARY)
        };

        for (state, fill, text) in [
            (&mut v.widgets.inactive, rest, rest_text),
            (&mut v.widgets.hovered, hover, hover_text),
            (&mut v.widgets.active, press, hover_text),
        ] {
            state.weak_bg_fill = fill;
            state.bg_stroke = egui::Stroke::NONE;
            state.fg_stroke = egui::Stroke::new(1.0, text);
            state.corner_radius = egui::CornerRadius::same(theme::RADIUS_PILL);
            // Сегмент не должен «распухать» — он зажат в дорожке.
            state.expansion = 0.0;
        }

        ui.add(egui::Button::new(label).min_size(egui::vec2(width, theme::SEGMENT_HEIGHT)))
            .clicked()
    })
    .inner
}

/// Заготовка вторичной кнопки: контурная «таблетка» штатной высоты.
///
/// Отдельно от [`pill_button`], потому что нужна и выключенной: у
/// `ui.add_enabled` первым аргументом идёт условие, а вторым — сам виджет.
fn pill(label: &str) -> egui::Button<'_> {
    egui::Button::new(label).min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT))
}

/// Вторичная кнопка.
fn pill_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(pill(label))
}

/// Кнопка-выключатель: нажатая заливается мягким акцентом.
///
/// Состояние сказано не только цветом — включённая ещё и обведена акцентной
/// границей, а рядом с ней всегда есть то, что она показывает.
fn toggle_pill(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    ui.scope(|ui| {
        if on {
            let v = ui.visuals_mut();
            for state in [
                &mut v.widgets.inactive,
                &mut v.widgets.hovered,
                &mut v.widgets.active,
            ] {
                state.weak_bg_fill = theme::ACCENT_SOFT;
                state.bg_stroke = egui::Stroke::new(1.0, theme::ACCENT);
                state.fg_stroke = egui::Stroke::new(1.0, theme::ACCENT_HOVER);
            }
        }
        pill_button(ui, label)
    })
    .inner
}

/// Неинтерактивная плашка: подпись на мягкой подложке.
///
/// Ею сказаны «MP3», «JPG» и «Опрос идёт» — то, что читают, но не нажимают.
///
/// Рисуется своими руками, а не `Frame` с меткой внутри, и это не вкусовщина.
/// Внутри `horizontal_wrapped` контейнер не знает, сколько места осталось
/// в строке, поэтому берёт ширину по своему содержимому — и плашка вылезает
/// за кромку карточки, утаскивая за собой саму карточку. Проверено глазами:
/// «TIFF — только чтение» так и уехал за правый край окна. Здесь ширина
/// известна заранее, из разложенного текста, и `horizontal_wrapped`
/// переносит плашку сам.
fn soft_pill(ui: &mut egui::Ui, text: &str, color: egui::Color32, fill: egui::Color32) {
    const PAD_X: f32 = 11.0;
    const HEIGHT: f32 = 24.0;

    let font = egui::TextStyle::Small.resolve(ui.style());
    // Раскладка текста у egui кэшируется по самой строке, так что повторный
    // вызов с той же подписью считает только хеш.
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        font,
        egui::Color32::PLACEHOLDER,
    );

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(galley.size().x + PAD_X * 2.0, HEIGHT),
        egui::Sense::hover(),
    );
    if !ui.is_rect_visible(rect) {
        return;
    }

    let painter = ui.painter();
    let stroke = if fill == egui::Color32::TRANSPARENT {
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE)
    } else {
        egui::Stroke::NONE
    };
    painter.rect(
        rect,
        egui::CornerRadius::same(theme::RADIUS_PILL),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.galley(
        egui::pos2(rect.left() + PAD_X, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

/// Переключатель-«чип»: галочка и подпись в одной «таблетке».
///
/// Своими руками, а не `Checkbox` в рамке, и рисуется здесь всё, включая
/// саму галочку. Причина у галочки та же, по которой в оверлее написано
/// «Закрыть», а не нарисован крестик: знака `✓` нет ни в одном из наших
/// шрифтов, и вместо него вышел бы пустой прямоугольник (Правило 4).
/// Две линии кистью надёжнее любого символа.
///
/// Галочка обязательна, а не украшение: без неё включённость чипа была бы
/// сказана одним цветом, а этого мало.
fn chip(ui: &mut egui::Ui, checked: &mut bool, label: &str, enabled: bool) -> egui::Response {
    const PAD: f32 = 14.0;
    const GAP: f32 = 8.0;
    const MARK: f32 = 14.0;

    ui.add_enabled_ui(enabled, |ui| {
        let font = egui::TextStyle::Button.resolve(ui.style());
        // Раскладка текста у egui кэшируется по самой строке, так что
        // повторный вызов с той же подписью считает только хеш.
        let galley =
            ui.painter()
                .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER);

        let size = egui::vec2(
            PAD * 2.0 + MARK + GAP + galley.size().x,
            theme::CONTROL_HEIGHT,
        );
        let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());

        if response.clicked() {
            *checked = !*checked;
            response.mark_changed();
        }

        if !ui.is_rect_visible(rect) {
            return response;
        }

        let on = *checked;
        let (fill, stroke, text_color) = match (on, response.hovered()) {
            (true, _) => (
                theme::SUCCESS_SOFT,
                theme::STATE_SUCCESS,
                theme::TEXT_PRIMARY,
            ),
            (false, true) => (theme::CARD_INNER, theme::BORDER_HOVER, theme::TEXT_PRIMARY),
            (false, false) => (
                egui::Color32::TRANSPARENT,
                theme::BORDER_STRONG,
                theme::TEXT_SECONDARY,
            ),
        };

        let painter = ui.painter();
        painter.rect(
            rect,
            egui::CornerRadius::same(theme::RADIUS_PILL),
            fill,
            egui::Stroke::new(1.0, stroke),
            egui::StrokeKind::Inside,
        );

        // Коробка галочки: пустая у выключенного чипа, с птичкой у включённого.
        let mark = egui::Rect::from_center_size(
            egui::pos2(rect.left() + PAD + MARK / 2.0, rect.center().y),
            egui::vec2(MARK, MARK),
        );
        painter.rect(
            mark,
            egui::CornerRadius::same(theme::RADIUS_TINY),
            egui::Color32::TRANSPARENT,
            egui::Stroke::new(1.4, if on { theme::STATE_SUCCESS } else { stroke }),
            egui::StrokeKind::Inside,
        );
        if on {
            let tick = egui::Stroke::new(1.8, theme::STATE_SUCCESS);
            let (l, t, w, h) = (mark.left(), mark.top(), mark.width(), mark.height());
            painter.line_segment(
                [
                    egui::pos2(l + w * 0.24, t + h * 0.52),
                    egui::pos2(l + w * 0.44, t + h * 0.72),
                ],
                tick,
            );
            painter.line_segment(
                [
                    egui::pos2(l + w * 0.44, t + h * 0.72),
                    egui::pos2(l + w * 0.78, t + h * 0.28),
                ],
                tick,
            );
        }

        painter.galley(
            egui::pos2(
                mark.right() + GAP,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            text_color,
        );

        response
    })
    .inner
}

/// Заголовок раскрывающейся группы: треугольник, название и сводка справа.
///
/// Возвращает `true`, когда по нему щёлкнули.
///
/// Треугольник рисуется кистью по той же причине, что и галочка в [`chip`]:
/// стрелок и треугольников в наших шрифтах нет. Подписи внутри намеренно
/// невыделяемые — иначе выделение текста съедало бы щелчок по строке.
fn disclosure_row(ui: &mut egui::Ui, open: bool, title: &str, summary: &str) -> bool {
    let inner = ui.horizontal(|ui| {
        ui.style_mut().interaction.selectable_labels = false;
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.add_space(8.0);

        let (mark, _) = ui.allocate_exact_size(egui::vec2(11.0, 11.0), egui::Sense::hover());
        let c = mark.center();
        let points = if open {
            // Вниз — группа раскрыта.
            vec![
                egui::pos2(c.x - 5.0, c.y - 2.5),
                egui::pos2(c.x + 5.0, c.y - 2.5),
                egui::pos2(c.x, c.y + 3.5),
            ]
        } else {
            // Вправо — группа свёрнута.
            vec![
                egui::pos2(c.x - 2.5, c.y - 5.0),
                egui::pos2(c.x - 2.5, c.y + 5.0),
                egui::pos2(c.x + 3.5, c.y),
            ]
        };
        ui.painter().add(egui::Shape::convex_polygon(
            points,
            theme::ACCENT,
            egui::Stroke::NONE,
        ));

        ui.add(
            egui::Label::new(egui::RichText::new(title).color(theme::TEXT_PRIMARY)).truncate(),
        );

        // Сводка прижата к правому краю и обрезается сама: подсказку с
        // полным текстом вешает обрезанная метка (дефект 22 — своя стала бы
        // второй коробкой).
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(summary)
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );
        });
    });

    let rect = inner.response.rect;
    ui.interact(rect, ui.id().with(title), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Ряд «подпись слева, элемент управления справа».
///
/// В узком окне подпись уезжает НАД элементом: колонка в 78 точек съела бы
/// пятую часть ширины, а переключателю качества из шести ступеней и без того
/// тесно. Порог — ширина, при которой ступени ещё помещаются в строку.
fn labelled_row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    // Ширина колонки подписи — под самую длинную из них, «Битрейт, кбит/с».
    // При 78 она обрезалась в «Битрейт, кб…», а обрезанная подпись у
    // переключателя — это ровно та частая беда, о которой Правило 2.
    const LABEL_WIDTH: f32 = 96.0;
    const GAP: f32 = 12.0;
    // Порог, ниже которого подпись уезжает НАД элементом. Считан от
    // переключателя качества: шесть ступеней («2160p» — самая широкая)
    // требуют около 360 точек, и колонка подписи с зазором отнимает ещё
    // 108. В окне минимальной ширины столько не набирается, и там подписи
    // встают сверху — иначе последняя ступень уехала бы за кромку.
    const INLINE_MIN: f32 = 470.0;

    if ui.available_width() < INLINE_MIN {
        field_label(ui, label);
        return add(ui);
    }

    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        let rest = (ui.available_width() - LABEL_WIDTH - GAP).max(120.0);

        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_WIDTH, theme::CONTROL_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                // `set_min_width` тут обязателен, хотя ширина уже запрошена
                // выше: `allocate_ui_with_layout` двигает курсор не на
                // запрошенный размер, а на тот, что занял потомок. Без него
                // короткая подпись («Формат») прижала бы элемент к себе,
                // а длинная («Качество») — нет, и колонки бы не вышло.
                ui.set_min_width(LABEL_WIDTH);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .small()
                            .color(theme::TEXT_MUTED),
                    )
                    .truncate(),
                );
            },
        );

        ui.allocate_ui_with_layout(
            egui::vec2(rest, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_width(rest);
                ui.set_max_width(rest);
                add(ui)
            },
        )
        .inner
    })
    .inner
}

/// Одна строка очереди. Возвращает `true`, если нажали «убрать».
///
/// Свободная функция, а не метод: строке нужен только сам элемент, и от
/// заимствования всего `SavioApp` внутри цикла по списку это избавляет.
fn queue_row(ui: &mut egui::Ui, item: &QueueItem) -> bool {
    let mut remove = false;

    theme::inner_frame()
        .show(ui, |ui| {
            // Иначе строка сжалась бы по ширине своего названия: у короткого
            // получилась бы узкая полоска посреди списка.
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                const GAP: f32 = 8.0;
                const BUTTON: f32 = 24.0;
                ui.spacing_mut().item_spacing.x = GAP;

                // Точка — подсказка глазу, а не носитель смысла: то же
                // состояние сказано словом строкой ниже.
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 4.0, item.status.color());

                // Убрать можно только то, что ещё не началось: у идущей
                // загрузки для этого есть «Отмена», а у отработавшей строка —
                // единственный след того, чем всё кончилось.
                let removable = item.status == QueueStatus::Waiting;

                // Место под кнопку отмеряем сами, а не кладём её первой
                // в раскладке справа налево: там короткое название прижалось
                // бы к правому краю, а начинаться строка обязана слева.
                // Отдать же название под `truncate()` без запаса нельзя —
                // оно займёт всю ширину и выдавит кнопку за кромку.
                let trailing = if removable { BUTTON + GAP } else { 0.0 };
                let width = (ui.available_width() - trailing).max(40.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(width, BUTTON),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        // `set_min_width` тут обязателен, хотя ширина уже
                        // запрошена выше: `allocate_ui_with_layout` двигает
                        // курсор не на запрошенный размер, а на тот, что занял
                        // потомок. Без него у короткого названия кнопка
                        // прилипала бы к нему вплотную посреди строки вместо
                        // правого края. Проверено глазами.
                        ui.set_min_width(width);
                        // Подсказку с полным названием вешает сама обрезанная
                        // метка — свой `on_hover_text` рядом её не заменил бы,
                        // а добавил вторую коробку с тем же текстом.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&item.title).color(theme::TEXT_PRIMARY),
                            )
                            .truncate(),
                        );
                    },
                );

                if removable {
                    remove = ui
                        .scope(|ui| {
                            // Поля кнопке урезаем, и это не косметика.
                            // `min_size` — только нижняя граница, а желаемую
                            // ширину `Button` считает как «текст плюс
                            // `button_padding.x` с двух сторон». При штатных
                            // 14 точках (theme.rs) крестик выходит 36.5 вместо
                            // отведённых ему 24, вылезает за `max_rect` строки
                            // и расширяет его — а следующая строка стартует уже
                            // от расширенной кромки и переполняет её снова.
                            // Перекос копится вниз по списку: к десятой строке
                            // это уже сотня точек за кромкой окна. Замерено на
                            // egui 0.35; ни сборка, ни `clippy`, ни тесты
                            // этого не видят.
                            ui.spacing_mut().button_padding.x = 6.0;
                            ui.add(egui::Button::new("×").min_size(egui::vec2(BUTTON, BUTTON)))
                                .on_hover_text("Убрать из очереди")
                                .clicked()
                        })
                        .inner;
                }
            });

            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(&item.detail)
                    .small()
                    .color(item.status.color()),
            );

            // Причина отказа — то, ради чего в этот список потом смотрят:
            // журнал к тому времени очищен следующей загрузкой. Обрезаем,
            // а не переносим: объяснения длинные, и десяток абзацев подряд
            // превратил бы список в стену текста. Полный текст показывает
            // та же штатная подсказка обрезанной метки.
            if !item.error_line.is_empty() {
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&item.error_line)
                            .small()
                            .color(theme::STATE_ERROR),
                    )
                    .truncate(),
                );
            }
        });

    remove
}

/// Одна галочка.
///
/// Вид задан здесь, а не у места вызова, и это не вкусовщина. Коробка
/// у флажка всего 16 точек в поперечнике, а общее скругление темы — 8:
/// при нём она скругляется почти в кружок, то есть выглядит радиокнопкой —
/// элементом с другим смыслом («одно из»), хотя галочки независимы. Пока
/// эта настройка стояла у карточки «Вшить в файл», следующая галочка
/// в другом месте окна получала именно кружок; ровно так и вышло с первой
/// галочкой монитора. Ни сборка, ни `clippy`, ни тесты этого не видят.
fn checkbox(
    ui: &mut egui::Ui,
    checked: &mut bool,
    label: &'static str,
    enabled: bool,
) -> egui::Response {
    ui.scope(|ui| {
        // Штатная строка виджета — 32 точки (высота поля ввода). Для галочки
        // это много: три подряд съели бы четверть окна минимальной высоты.
        ui.spacing_mut().interact_size.y = 24.0;

        let v = ui.visuals_mut();
        // `noninteractive` в списке не для полноты: выключенную галочку
        // egui рисует именно им, и без него она осталась бы с чужим
        // скруглением, то есть тем самым кружком-радиокнопкой.
        for state in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
        ] {
            // Галочку egui рисует цветом `fg_stroke` — тем же, каким красит
            // подпись рядом. Акцент нужен только самой галочке (в покое
            // коробка пуста, и без цвета выбранное не отличить от
            // невыбранного), поэтому подписи цвет задаётся отдельно, через
            // `RichText`: он перебивает цвет по умолчанию.
            state.fg_stroke = egui::Stroke::new(1.6, theme::ACCENT);
            state.corner_radius = egui::CornerRadius::same(theme::RADIUS_TINY);
            state.expansion = 0.0;
        }
        // Коробка «утоплена», как поле ввода и дорожка переключателя: на
        // заливке карточки она иначе держится на одной тонкой рамке.
        v.widgets.inactive.bg_fill = theme::INPUT_FILL;

        ui.add_enabled(
            enabled,
            egui::Checkbox::new(
                checked,
                egui::RichText::new(label).color(theme::TEXT_PRIMARY),
            ),
        )
    })
    .inner
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

/// Кнопка выбора «одно из»: подпись в таблетке, выбранная — с акцентом.
///
/// Рисуется своими руками, и причина не в оформлении, а в переносе строки.
/// Готовый [`toggle_pill`] заводит внутри `ui.scope`, а тот сообщает
/// раскладке занятое место **задним числом**, минуя `next_space` — то есть
/// мимо всей логики переноса. В `horizontal_wrapped` такая кнопка на новую
/// строку не уезжает: она вылезает за кромку карточки и тянет карточку за
/// собой. По той же причине выключать ряд надо целиком, снаружи, а не
/// каждую кнопку через `add_enabled_ui`: это тот же `scope`.
///
/// Проверено глазами в окне 520×420: третья таблетка режима обрезалась
/// кромкой окна, а подпись второй заворачивалась в две строки внутри самой
/// таблетки. Ни сборка, ни `clippy`, ни тесты этого не видят.
///
/// Здесь ширина известна заранее, из разложенного текста, и место просится
/// через `allocate_exact_size` — то есть через ту самую логику переноса
/// (так же устроен `chip`).
fn choice_pill(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    const PAD: f32 = 14.0;

    let font = egui::TextStyle::Button.resolve(ui.style());
    // Раскладка текста у egui кэшируется по самой строке, так что повторный
    // вызов с той же подписью считает только хеш.
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::PLACEHOLDER);

    let size = egui::vec2(PAD * 2.0 + galley.size().x, theme::CONTROL_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }

    // Выбранная сказана не одним цветом: у неё и заливка, и акцентная
    // кромка, и подпись другого тона. Невыбранная в покое почти прозрачна
    // и светлеет под курсором — это и есть отклик.
    let (fill, stroke, text) = match (on, response.hovered()) {
        (true, _) => (theme::ACCENT_SOFT, theme::ACCENT, theme::ACCENT_HOVER),
        (false, true) => (theme::CARD_INNER, theme::BORDER_HOVER, theme::TEXT_PRIMARY),
        (false, false) => (
            egui::Color32::TRANSPARENT,
            theme::BORDER_STRONG,
            theme::TEXT_SECONDARY,
        ),
    };

    let painter = ui.painter();
    painter.rect(
        rect,
        egui::CornerRadius::same(theme::RADIUS_PILL),
        fill,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.galley(
        egui::pos2(rect.left() + PAD, rect.center().y - galley.size().y / 2.0),
        galley,
        text,
    );

    response
}

/// Мелкая оговорка под элементом управления.
///
/// `wrap()` обязателен: без него длинная строка ушла бы за кромку окна
/// и растянула бы содержимое прокрутки — см. комментарий в `banner`.
fn note(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.add(egui::Label::new(egui::RichText::new(text).small().color(color)).wrap());
}

/// Цвет плашки статуса пункта отчёта.
///
/// «Нет данных» намеренно не красный и не зелёный, а приглушённый: это не
/// беда и не благополучие, а отсутствие сведений. Покрасить его зелёным
/// значило бы сказать «в порядке» про то, чего не смотрели, красным —
/// напугать штатным положением дел.
fn check_color(status: CheckStatus) -> egui::Color32 {
    match status {
        CheckStatus::Ok => theme::STATE_SUCCESS,
        CheckStatus::Warning => theme::STATE_WARNING,
        CheckStatus::Failed => theme::STATE_ERROR,
        CheckStatus::Unknown => theme::TEXT_MUTED,
    }
}

/// Карточка одного пункта отчёта.
fn check_card(ui: &mut egui::Ui, check: &crate::model::Check) {
    theme::card(ui, |ui| {
            // Плашка прижата к правому краю: так статусы всех карточек
            // стоят в одну колонку и читаются сверху вниз, не завися от
            // длины заголовка. Кладём её первой, справа налево: обрезаемый
            // заголовок иначе занял бы всю ширину, и плашка налезла бы
            // на него.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                status_pill(ui, check.status.label(), check_color(check.status));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&check.name)
                                .font(theme::display(17.0))
                                .color(theme::TEXT_PRIMARY),
                        )
                        .truncate(),
                    );
                });
            });

            ui.add_space(6.0);
            note(ui, &check.summary, theme::TEXT_SECONDARY);

            if !check.rows.is_empty() {
                ui.add_space(10.0);
                for row in &check.rows {
                    check_row(ui, row);
                }
            }

            if let Some(advice) = &check.advice {
                ui.add_space(10.0);
                banner(ui, advice, theme::STATE_WARNING);
            }
        });
}

/// Строка «подпись — значение» внутри карточки.
fn check_row(ui: &mut egui::Ui, row: &crate::model::CheckRow) {
    stat_row(ui, &row.label, row.value.as_deref());
}

/// Строка «подпись — значение» в колонку.
///
/// Отделена от `check_row` ради монитора: у него значения приезжают готовыми
/// строками внутри замера, и заворачивать их в `CheckRow` пришлось бы каждый
/// кадр — то есть заводить по аллокации на строку шестьдесят раз в секунду
/// ровно там, где Правило 1 этого и не велит. Вид у строк при этом обязан
/// остаться общим: две одинаковые на вид таблицы, разъехавшиеся по вёрстке,
/// выглядят небрежностью.
fn stat_row(ui: &mut egui::Ui, label: &str, value: Option<&str>) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        // Подпись занимает фиксированную долю ширины, чтобы значения
        // выстроились в колонку. Доля, а не число точек: в окне 520 и в
        // развёрнутом на два монитора нужны разные ширины, а колонка нужна
        // в обеих.
        let label_width = (ui.available_width() * 0.42).clamp(40.0, 200.0);
        ui.allocate_ui_with_layout(
            egui::vec2(label_width, 0.0),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                // `set_min_width` тут обязателен, хотя ширина уже запрошена
                // выше, и это ровно та же грабля, что в `queue_row`:
                // `allocate_ui_with_layout` двигает курсор не на запрошенный
                // размер, а на тот, что реально занял потомок. Без него
                // значения начинаются сразу за подписью, каждое на своём
                // месте, и обещанной колонки не получается — у «Ядро»
                // значение уезжает к левому краю, у «Точка монтирования»
                // почти к середине. Ни сборка, ни тесты этого не видят.
                ui.set_min_width(label_width);
                // Подсказку с полным текстом обрезанная метка вешает сама
                // (`show_tooltip_when_elided` включён по умолчанию). Свой
                // `on_hover_text` здесь добавил бы вторую коробку с тем же
                // текстом — это дефект 22, он уже случался.
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(label)
                            .small()
                            .color(theme::TEXT_MUTED),
                    )
                    .truncate(),
                );
            },
        );

        match value {
            // `wrap()` обязателен: в горизонтальной раскладке egui берёт
            // режим `Extend` и кладёт значение в одну строку любой длины —
            // длинное имя USB-устройства ушло бы за кромку окна.
            Some(value) => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(value)
                            .small()
                            .color(theme::TEXT_SECONDARY),
                    )
                    .wrap(),
                );
            }
            // Вот ради чего значение — `Option`. Прочерк, а не ноль и не
            // пустое место: пустая строка выглядит недорисованной, а ноль
            // читается как измеренная величина. Тон приглушённый — «нет
            // данных» не должно спорить за внимание с настоящими числами.
            None => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("—")
                            .small()
                            .color(theme::TEXT_MUTED),
                    )
                    .wrap(),
                )
                .on_hover_text("Система не сообщила это значение.");
            }
        }
    });
}

fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(theme::TEXT_MUTED));
    ui.add_space(6.0);
}

/// Плашка состояния: цветная точка плюс подпись тем же цветом.
/// Цветом одним статус не передаём — рядом всегда есть текст.
fn status_pill(ui: &mut egui::Ui, label: &str, color: egui::Color32) {
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_PILL))
        .inner_margin(egui::Margin::symmetric(11, 5))
        .show(ui, |ui| {
            // Раскладку задаём явно, а не `ui.horizontal`: тот, оказавшись
            // внутри уже горизонтальной раскладки, наследует её направление —
            // а плашку ставят как раз в `right_to_left`, чтобы прижать её
            // к правому краю карточки. Без этого точка и подпись менялись
            // местами, и одна и та же плашка выглядела по-разному в разных
            // местах окна. Проверено глазами: ни сборка, ни тесты этого
            // не видят.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let (dot, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.5, color);
                ui.add(
                    egui::Label::new(egui::RichText::new(label).small().color(color)).truncate(),
                );
            });
        });
}

// ---------------------------------------------------------------------------
// Монитор: карточки, графики, оверлей
// ---------------------------------------------------------------------------

/// Прочерк на месте отсутствующего значения.
///
/// Одной константой, а не литералом по месту: прочерк здесь — не украшение,
/// а способ сказать «нет данных», и он обязан выглядеть одинаково везде.
const DASH: &str = "—";

/// Карточка показателя: крупное число, график и подпись.
fn metric_card(
    ui: &mut egui::Ui,
    title: &str,
    metric: &Metric,
    trace: &Trace,
    color: egui::Color32,
) {
    theme::card(ui, |ui| {
            // Число прижато к правому краю: так проценты всех карточек
            // стоят в одну колонку и читаются сверху вниз. Кладём его
            // первым, справа налево, — обрезаемый заголовок иначе забрал бы
            // всю ширину и число ушло бы под него.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = metric.percent_text.as_deref().unwrap_or(DASH);
                ui.label(
                    egui::RichText::new(text)
                        .font(theme::display(30.0))
                        .color(color),
                );
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(title)
                                .small()
                                .color(theme::TEXT_MUTED),
                        )
                        .truncate(),
                    );
                });
            });

            ui.add_space(8.0);
            trace_plot(ui, trace, color);

            if let Some(detail) = &metric.detail {
                ui.add_space(6.0);
                note(ui, detail, theme::TEXT_SECONDARY);
            }
        });
}

/// Полоска-график: последние замеры, слева самый старый.
///
/// Шкала жёстко от нуля до ста, а не «по максимуму в окне». Автомасштаб
/// нарисовал бы у простаивающей машины ту же гору, что у загруженной, —
/// график, который врёт ровно в ту сторону, в какую на него смотрят.
fn trace_plot(ui: &mut egui::Ui, trace: &Trace, color: egui::Color32) {
    const HEIGHT: f32 = 56.0;

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(theme::RADIUS_INNER),
        theme::INPUT_FILL,
    );
    // Одна линия сетки, на половине шкалы. Пять линий на сорока четырёх
    // точках высоты слились бы в серый прямоугольник.
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

    // Одной точке рисовать нечего: линия начинается с отрезка.
    let filled = trace.iter().len();
    if filled < 2 {
        return;
    }

    // Шаг считаем по потолку буфера, а не по числу набранных точек: иначе
    // первые секунды график растягивался бы на всю ширину и «сжимался» по
    // мере наполнения — движение, которого на самом деле не было.
    let step = rect.width() / (TRACE_LIMIT - 1) as f32;
    let newest = filled - 1;

    // Одна ломаная, а не сто девятнадцать отрезков: `Shape::line` кладёт
    // в список отрисовки один объект, отдельные `line_segment` — по одному
    // на каждую пару точек.
    let points: Vec<egui::Pos2> = trace
        .iter()
        .enumerate()
        .map(|(i, value)| {
            egui::pos2(
                rect.right() - (newest - i) as f32 * step,
                rect.bottom() - (value / 100.0).clamp(0.0, 1.0) * rect.height(),
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5, color),
    ));
}

/// Карточка «Ввод-вывод»: сеть, диски и видеокарта.
///
/// Втроём в одной карточке, потому что у всех троих одна беда: показать
/// про них можно строку, а не график. Своя карточка на строку превратила бы
/// вкладку в лестницу из рамок.
fn io_card(ui: &mut egui::Ui, sample: &PerfSample, gpu: Option<&GpuInfo>) {
    theme::card(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new("Ввод-вывод")
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );

            ui.add_space(8.0);
            stat_row(ui, "Сеть", sample.net.as_deref());
            stat_row(ui, "Диски", sample.disk.as_deref());
            stat_row(ui, "Подкачка", sample.swap.detail.as_deref());
            // Видеокарта здесь только именем: загрузку у неё не спросить,
            // а имя уже снято с адаптера, которым eframe рисует окно.
            stat_row(ui, "Видеокарта", gpu.map(|gpu| gpu.name.as_str()));
        });
}

/// Карточка со списком процессов.
fn process_card(ui: &mut egui::Ui, procs: &[crate::model::ProcRow]) {
    theme::card(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new("Процессы")
                        .small()
                        .color(theme::TEXT_MUTED),
                )
                .truncate(),
            );

            ui.add_space(4.0);
            if procs.is_empty() {
                note(
                    ui,
                    "Список процессов система не отдала.",
                    theme::TEXT_MUTED,
                );
                return;
            }

            note(
                ui,
                "Сверху те, кто занимает процессор. Доля уже поделена на \
                 число ядер, поэтому сумма по списку не превышает ста.",
                theme::TEXT_MUTED,
            );
            ui.add_space(8.0);

            // Своей прокрутки здесь нет: вкладка целиком лежит в общей, а
            // вложенная полоса рядом с внешней схлопывает содержимое (это
            // дефект 27, он уже случался с журналом). Список короткий —
            // `PROC_LIMIT` строк, — и в общей прокрутке помещается весь.
            for row in procs {
                process_row(ui, row);
            }
        });
}

/// Одна строка списка процессов.
fn process_row(ui: &mut egui::Ui, row: &crate::model::ProcRow) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        // Раскладка справа налево: числа кладутся первыми и занимают
        // ровно себя, а имени достаётся остаток строки. Слева направо
        // длинное имя процесса вытолкнуло бы за кромку окна как раз то,
        // ради чего в список и смотрят.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&row.mem_text)
                        .small()
                        .color(theme::TEXT_SECONDARY),
                )
                .truncate(),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&row.cpu_text)
                        .small()
                        .color(theme::TEXT_PRIMARY),
                )
                .truncate(),
            );
            // Имени достаётся весь остаток строки, и вложенная раскладка
            // здесь обязательна: в `right_to_left` метка занимает ровно
            // себя и прижимается к правому краю — без неё вся строка
            // сползает вправо, а слева остаётся пустое поле во всю ширину
            // окна. Проверено глазами: ни сборка, ни тесты этого не видят.
            //
            // Подсказку с полным именем обрезанная метка вешает сама —
            // свой `on_hover_text` добавил бы вторую (дефект 22).
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&row.name)
                            .small()
                            .color(theme::TEXT_SECONDARY),
                    )
                    .truncate(),
                );
            });
        });
    });

    // Полоска под строкой: список читается глазами по ней, а не по числам.
    let (bar, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 3.0),
        egui::Sense::hover(),
    );
    let radius = egui::CornerRadius::same(2);
    let painter = ui.painter();
    painter.rect_filled(bar, radius, theme::INPUT_FILL);
    let filled = egui::Rect::from_min_size(
        bar.min,
        egui::vec2(bar.width() * (row.cpu / 100.0).clamp(0.0, 1.0), bar.height()),
    );
    painter.rect_filled(filled, radius, theme::ACCENT);
    ui.add_space(6.0);
}

/// Содержимое оверлея.
///
/// Свободная функция, а не метод: замыкание дочернего окна обязано быть
/// `Send + Sync + 'static` (так требует `show_viewport_deferred`), то есть
/// до полей `SavioApp` ему не дотянуться. Всё, что оно видит, приходит
/// через `Arc`.
///
/// Прозрачным окно намеренно не сделано, хотя просится. Подробности —
/// в CLAUDE.md, коротко: у wgpu прозрачность включается на весь `Painter`
/// сразу и берётся из настроек **главного** окна, а поверхность обычного
/// окна на Windows отдаёт единственный режим смешения — непрозрачный.
/// Запрос прозрачности собрался бы, прошёл бы clippy и тесты — и уехал бы
/// в `log::warn`, которого никто не увидит.
fn overlay_ui(
    ui: &mut egui::Ui,
    class: egui::ViewportClass,
    sample: &Mutex<Option<PerfSample>>,
    closing: &AtomicBool,
) {
    // Окно закрыли системой — Alt+F4 или «Закрыть» из панели задач. Гасить
    // галочку отсюда некому, поэтому передаём просьбу главному окну.
    if ui.ctx().input(|i| i.viewport().close_requested()) {
        closing.store(true, Ordering::Relaxed);
    }

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::MODAL_FILL)
                .stroke(egui::Stroke::new(1.0, theme::BORDER_STRONG))
                .inner_margin(egui::Margin::symmetric(12, 10)),
        )
        .show(ui, |ui| {
            // Отступы темы рассчитаны на окно, а не на полоску в треть его
            // ширины: штатная строка виджета — 32 точки, и четыре строки
            // с заголовком не поместились бы в оверлей никакой разумной
            // высоты. Правится здесь, а не в теме: там эти числа держат
            // раскладку главного окна.
            ui.spacing_mut().interact_size.y = 0.0;
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.spacing_mut().button_padding = egui::vec2(8.0, 2.0);

            // Выделение текста здесь выключено не ради вида, а ради
            // перетаскивания. Подписи у egui по умолчанию выделяемые, то есть
            // сами ловят нажатие и протаскивание, — и окно, схваченное за
            // строку «ЦП 24%», не двигалось никуда. Проверено вживую: за пустое
            // место оверлей тянулся, за любую надпись — нет, а надписи занимают
            // почти всю его площадь. Ни сборка, ни `clippy`, ни тесты этого
            // не видят: и там и там нажатие «сработало», просто на другом
            // виджете. Выделять в оверлее всё равно нечего — числа меняются
            // раз в секунду.
            ui.style_mut().interaction.selectable_labels = false;

            // Перетаскивание вешаем ПЕРВЫМ, до кнопки: у egui щелчок достаётся
            // тому, кого положили позже. Наоборот — и кнопка «Закрыть»
            // перестала бы нажиматься, а окно ездило бы по экрану от каждого
            // тычка в неё.
            //
            // Своего заголовка у окна нет (`with_decorations(false)`), так что
            // без этого оверлей нельзя было бы сдвинуть с того места, куда его
            // поставила система. Во встроенном окне (`EmbeddedWindow`, сборка
            // без поддержки нескольких окон) команда бессмысленна: там его
            // таскает сам egui за свою рамку.
            if class != egui::ViewportClass::EmbeddedWindow {
                let drag = ui.interact(
                    ui.max_rect(),
                    ui.id().with("savio-overlay-drag"),
                    egui::Sense::click_and_drag(),
                );
                // Спрашиваем «кнопка нажата на нас», а не `drag_started`, и это
                // не придирка: как только команда ушла, окно уводит система —
                // мышь захватывает её собственный цикл перетаскивания, и до
                // egui события больше не доходят. Порога сдвига, по которому
                // `drag_started` только и срабатывает, оно набрать не успевает,
                // так что оверлей просто не двигался. Проверено вживую: с
                // `drag_started` сдвиг ровно ноль. Так же устроен и штатный
                // пример egui с окном без рамки.
                if drag.is_pointer_button_down_on() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
            }

            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new("Savio")
                            .small()
                            .color(theme::TEXT_MUTED),
                    )
                    .truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Словом, а не крестиком: своих шрифтов Savio не
                    // подключает, а в тех, что кладёт eframe, знаки вроде
                    // `✕` рисуются пустым прямоугольником — ровно так уже
                    // вышло со стрелкой `→` (Правило 4).
                    let close =
                        ui.add(egui::Button::new(egui::RichText::new("Закрыть").small()));
                    if close.clicked() {
                        closing.store(true, Ordering::Relaxed);
                    }
                });
            });

            ui.add_space(6.0);

            let Ok(slot) = sample.lock() else {
                return;
            };
            let Some(sample) = slot.as_ref() else {
                note(ui, "Замеряю…", theme::TEXT_MUTED);
                return;
            };

            overlay_row(ui, "ЦП", sample.cpu.percent_text.as_deref());
            overlay_row(ui, "ОЗУ", sample.mem.percent_text.as_deref());
            overlay_row(ui, "Сеть", sample.net.as_deref());
            overlay_row(ui, "Диск", sample.disk.as_deref());
        });
}

/// Одна строка оверлея.
///
/// Своя, а не `stat_row`: там подпись занимает 42% ширины ради колонки из
/// длинных названий, а здесь подписи в три буквы и места всего 230 точек —
/// колонка съела бы половину окна.
fn overlay_row(ui: &mut egui::Ui, label: &str, value: Option<&str>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add(
            egui::Label::new(
                egui::RichText::new(label)
                    .small()
                    .color(theme::TEXT_MUTED),
            )
            .truncate(),
        );
        ui.add(
            egui::Label::new(
                egui::RichText::new(value.unwrap_or(DASH))
                    .font(theme::bold(12.5))
                    .color(theme::TEXT_PRIMARY),
            )
            .truncate(),
        );
    });
}

/// Сообщение об ошибке или предупреждение: цветная полоса слева, текст справа.
fn banner(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(theme::CARD_INNER)
        .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_INNER))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
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
    use crate::model::NO_DOWNLOAD;

    fn request(url: &str, format: Format, quality: Quality) -> Request {
        Request {
            url: url.to_owned(),
            format,
            quality,
            options: DownloadOptions::default(),
            section: Section::default(),
            cookies: CookieSource::default(),
            cookie_file: None,
            sub_lang: SubLang::default(),
        }
    }

    /// Очередь из `count` одинаковых ожидающих ссылок.
    fn queue_with(count: usize) -> Queue {
        let mut queue = Queue::new();
        for i in 0..count {
            let url = format!("https://site/{i}");
            assert!(
                queue
                    .push(
                        request(&url, Format::Mp4, Quality::Best),
                        PathBuf::from("/dl"),
                        None
                    )
                    .is_some(),
                "{url} не влезла в очередь"
            );
        }
        queue
    }

    fn ids(queue: &Queue) -> Vec<DownloadId> {
        queue.items.iter().map(|item| item.id).collect()
    }

    fn info(title: &str) -> MediaInfo {
        MediaInfo {
            title: Some(title.to_owned()),
            ..MediaInfo::default()
        }
    }

    /// Предпросмотр, уже спросивший про эту ссылку.
    ///
    /// Ручку движка тест завести не может — её отдаёт только
    /// `engine::start_probe`, — но для развязки поколений она и не нужна:
    /// всё решает приёмник, а ручка лишь убивает процесс.
    fn asking(url: &str) -> (Preview, std::sync::mpsc::Sender<Event>) {
        let mut preview = Preview::default();
        assert!(preview.retarget(url, CookieSource::None, None, 0.0), "{url}");

        let (tx, rx) = channel();
        preview.due = None;
        preview.rx = Some(rx);
        (preview, tx)
    }

    /// Ответ по прежней ссылке не должен попасть в окно.
    ///
    /// Это и есть та тихая беда, ради которой запрос живёт на своём канале:
    /// медленный сайт отвечает про первую ссылку позже, чем быстрый про
    /// вторую, и в карточке оказывается чужое название. Ни компилятор, ни
    /// сборка, ни глазной прогон такого не ловят — воспроизводится оно только
    /// на разной скорости ответов.
    #[test]
    fn an_answer_about_the_previous_link_never_reaches_the_screen() {
        let (mut preview, slow) = asking("https://site.ru/a");
        // Ответ по первой ссылке уже в пути, а в поле за это время оказалась
        // вторая.
        slow.send(Event::Info(info("Ролик по прежней ссылке")))
            .expect("канал ещё жив");
        assert!(preview.retarget("https://site.ru/b", CookieSource::None, None, 0.0));

        assert!(
            preview.take_events().is_empty(),
            "ответ по прежней ссылке обязан пропасть вместе с приёмником"
        );
        assert!(preview.info.is_none(), "и ничего после себя не оставить");
    }

    /// Обратная половина: ответ по той ссылке, что в поле, доезжает.
    /// Поодиночке любой из двух тестов прошёл бы и на предпросмотре,
    /// который не работает вовсе.
    #[test]
    fn an_answer_about_the_link_in_the_field_reaches_the_screen() {
        let (mut preview, tx) = asking("https://site.ru/a");
        tx.send(Event::Info(info("Тот самый ролик")))
            .expect("канал ещё жив");

        let events = preview.take_events();
        assert_eq!(events.len(), 1, "ответ обязан доехать: {events:?}");
    }

    /// Молчание — законный исход: про отсутствие yt-dlp и незнакомый сайт
    /// предпросмотр не говорит ничего, и единственный его признак —
    /// закрытый канал без `Info`. Спутать одно с другим нельзя: в первом
    /// случае под ссылкой надо объясниться, во втором — показать ролик.
    #[test]
    fn silence_and_an_answer_are_told_apart() {
        let (mut preview, tx) = asking("https://site.ru/a");
        drop(tx);
        assert!(preview.take_events().is_empty());
        assert_eq!(preview.state, PreviewState::Failed);

        let (mut preview, tx) = asking("https://site.ru/a");
        tx.send(Event::Info(info("Ролик"))).expect("канал ещё жив");
        drop(tx);
        assert_eq!(preview.take_events().len(), 1);
        assert_ne!(
            preview.state,
            PreviewState::Failed,
            "ответ пришёл — жаловаться не на что"
        );
    }

    /// Окно ожидания отсчитывается от последней правки, а не от первой:
    /// иначе ссылка, набранная руками, ушла бы в сеть недописанной.
    #[test]
    fn the_site_is_asked_only_after_a_pause() {
        let mut preview = Preview::default();

        assert!(preview.retarget("https://site.ru/a", CookieSource::None, None, 10.0));
        assert_eq!(preview.due, Some(10.0 + PREVIEW_DEBOUNCE));

        assert!(preview.retarget("https://site.ru/ab", CookieSource::None, None, 10.4));
        assert_eq!(preview.due, Some(10.4 + PREVIEW_DEBOUNCE));

        // Та же ссылка и тот же вход — спрашивать заново нечего: срок
        // не сдвигается, а начатое не бросается.
        assert!(!preview.retarget("https://site.ru/ab", CookieSource::None, None, 10.9));
        assert_eq!(preview.due, Some(10.4 + PREVIEW_DEBOUNCE));
    }

    /// Обрывок текста из буфера обмена в yt-dlp не отправляем: кнопку такая
    /// строка не блокирует, но процесс и поход в сеть стоят дороже догадки.
    #[test]
    fn junk_from_the_clipboard_is_not_worth_a_request() {
        let mut preview = Preview::default();
        for text in ["", "   ", "просто текст", "site.com/watch?v=a", "https://"] {
            preview.retarget(text, CookieSource::None, None, 0.0);
            assert_eq!(preview.due, None, "спросили про {text:?}");
            assert_eq!(preview.state, PreviewState::Idle, "на {text:?}");
        }
    }

    /// Готовый ответ переиспользуется загрузкой — но только тот же самый.
    /// Тот же адрес, спрошенный с чужим входом в аккаунт, у YouTube отдаёт
    /// другое вплоть до пустого списка дорожек.
    #[test]
    fn the_answer_is_reused_only_for_the_very_same_request() {
        let mut preview = Preview::default();
        preview.retarget("https://site.ru/a", CookieSource::None, None, 0.0);
        preview.info = Some(info("Ролик"));

        let mut same = request("https://site.ru/a", Format::Mp4, Quality::Best);
        assert!(preview.answer_for(&same).is_some());

        let other = request("https://site.ru/b", Format::Mp4, Quality::Best);
        assert!(preview.answer_for(&other).is_none(), "чужая ссылка");

        same.cookies = CookieSource::Firefox;
        assert!(preview.answer_for(&same).is_none(), "чужой вход в аккаунт");
    }

    /// Тот же вопрос про файл: два разных файла cookies — это два разных
    /// входа в аккаунт, и ответ по одному нельзя показывать под другим.
    /// Сам источник (`File`) при этом не меняется, так что без сверки путей
    /// подмена прошла бы незамеченной.
    #[test]
    fn a_different_cookie_file_is_a_different_question() {
        let mine = PathBuf::from("/home/me/cookies.txt");
        let other = PathBuf::from("/home/me/другой.txt");

        let mut preview = Preview::default();
        preview.retarget("https://site.ru/a", CookieSource::File, Some(&mine), 0.0);
        preview.info = Some(info("Ролик"));

        let mut request = request("https://site.ru/a", Format::Mp4, Quality::Best);
        request.cookies = CookieSource::File;
        request.cookie_file = Some(mine.clone());
        assert!(preview.answer_for(&request).is_some(), "свой же ответ");

        request.cookie_file = Some(other.clone());
        assert!(preview.answer_for(&request).is_none(), "ответ по чужому файлу");

        // И смена файла обязана перезапустить запрос — иначе на экране
        // осталось бы название, полученное прежним входом.
        assert!(
            preview.retarget("https://site.ru/a", CookieSource::File, Some(&other), 1.0),
            "смену файла не заметили"
        );
        assert!(
            !preview.retarget("https://site.ru/a", CookieSource::File, Some(&other), 2.0),
            "тот же файл — спрашивать заново нечего"
        );
    }

    /// Строка очереди называется роликом сразу, а не через час, когда до неё
    /// дойдёт очередь: название уже спрошено предпросмотром.
    #[test]
    fn a_queued_row_is_named_by_the_preview_right_away() {
        let mut queue = Queue::new();
        queue
            .push(
                request("https://site/watch?v=abc", Format::Mp4, Quality::Best),
                PathBuf::from("/dl"),
                Some(info("Ролик про кота")),
            )
            .expect("место в пустой очереди есть");

        assert_eq!(queue.items[0].title, "Ролик про кота");
    }

    /// Ноль занят под `NO_DOWNLOAD` — событие установки или разбора
    /// метаданных. Выдай очередь этот номер ссылке, и чужой `Failed`
    /// пометил бы её ошибкой.
    #[test]
    fn numbering_never_hands_out_the_reserved_zero() {
        let mut queue = Queue::new();
        assert_ne!(queue.next_id, NO_DOWNLOAD);

        // Четыре миллиарда ссылок за запуск недостижимы, но проверить обход
        // нуля дёшево, а `+= 1` на этом месте ещё и паникует в отладке.
        queue.next_id = u32::MAX;
        let last = queue
            .push(
                request("https://site/a", Format::Mp4, Quality::Best),
                PathBuf::from("/dl"),
                None,
            )
            .unwrap();
        let wrapped = queue
            .push(
                request("https://site/b", Format::Mp4, Quality::Best),
                PathBuf::from("/dl"),
                None,
            )
            .unwrap();

        assert_eq!(last, u32::MAX);
        assert_eq!(wrapped, 1, "после переполнения нумерация обходит ноль");
    }

    /// Потолок обязателен по Правилу 1, как и у журнала с историей. Но
    /// выбрасывать можно только отработавшее: ожидающая ссылка — это
    /// невыполненная просьба, и потерять её молча нельзя.
    #[test]
    fn queue_stays_bounded_by_dropping_what_already_finished() {
        let mut queue = queue_with(QUEUE_LIMIT);
        assert_eq!(queue.items.len(), QUEUE_LIMIT);
        assert!(queue.full, "мест нет и освободить нечем");

        let extra = request("https://site/extra", Format::Mp4, Quality::Best);
        assert_eq!(queue.push(extra.clone(), PathBuf::from("/dl"), None), None);
        assert_eq!(queue.items.len(), QUEUE_LIMIT, "ожидающую не выбросили");

        // Первая скачалась — место освободилось, и уходит именно она.
        let oldest = queue.items[0].id;
        queue.set_status(oldest, QueueStatus::Done);
        assert!(!queue.full);

        assert!(queue.push(extra, PathBuf::from("/dl"), None).is_some());
        assert_eq!(queue.items.len(), QUEUE_LIMIT);
        assert!(
            queue.items.iter().all(|item| item.id != oldest),
            "место обязана освободить отработавшая строка"
        );
    }

    /// Очередь идёт сверху вниз и не выдаёт дважды одно и то же: выдай она
    /// идущую ссылку второй раз — и тот же ролик качался бы в два потока.
    #[test]
    fn the_queue_goes_top_down_and_skips_what_already_ran() {
        let mut queue = queue_with(3);
        let id = ids(&queue);
        let next = |q: &Queue| q.next_waiting().map(|(id, ..)| id);

        assert_eq!(next(&queue), Some(id[0]));
        queue.set_status(id[0], QueueStatus::Done);
        assert_eq!(next(&queue), Some(id[1]));
        queue.set_status(id[1], QueueStatus::Running);
        assert_eq!(next(&queue), Some(id[2]));

        queue.set_status(id[2], QueueStatus::Failed("нет такой страницы".into()));
        assert_eq!(next(&queue), None);
        assert!(!queue.has_waiting());
    }

    /// Тот самый случай, ради которого номера и заведены: у снятой загрузки
    /// поток движка живёт ещё секунду и досылает свой исход. Достанься он
    /// следующему элементу — исправная загрузка показалась бы сорванной.
    #[test]
    fn a_late_event_of_a_dropped_download_belongs_to_no_one() {
        let mut queue = queue_with(2);
        let id = ids(&queue);

        queue.set_status(id[0], QueueStatus::Running);
        assert!(queue.is_running(id[0]));
        assert!(!queue.is_running(id[1]), "ожидающая ещё не идёт");

        queue.set_status(id[0], QueueStatus::Cancelled);
        queue.set_status(id[1], QueueStatus::Running);

        assert!(!queue.is_running(id[0]), "запоздалое событие снятой загрузки");
        assert!(queue.is_running(id[1]));
        // Установка и метаданные ходят без номера — их события в очередь
        // не попадают вовсе.
        assert!(!queue.is_running(NO_DOWNLOAD));
        assert_eq!(queue.running_id(), Some(id[1]));
    }

    /// Строка списка обязана говорить, что именно уедет на диск: настройки
    /// снимаются в момент постановки, и переключатели на экране к ней уже
    /// отношения не имеют.
    #[test]
    fn a_row_says_what_it_will_download_and_in_what_state() {
        let mut queue = Queue::new();
        let id = queue
            .push(
                request("https://site/a", Format::Mp3, Quality::P1080),
                PathBuf::from("/dl"),
                None,
            )
            .unwrap();

        let detail = queue.items[0].detail.clone();
        assert!(detail.starts_with("Ожидает"), "{detail}");
        // Единица обязательна: «MP3 · 192» читается как загадка.
        assert!(detail.contains("MP3 · 192 кбит/с"), "{detail}");

        queue.set_status(id, QueueStatus::Running);
        let detail = &queue.items[0].detail;
        assert!(detail.starts_with("Качается"), "{detail}");
        assert!(detail.contains("MP3 · 192 кбит/с"), "{detail}");
    }

    /// `explain_failure` отдаёт объяснение с переносами строк, а метка
    /// с `truncate()` показывает ровно первую из них: в списке выходило
    /// «Ошибка (код 1):…» — подпись, не говорящая ничего.
    #[test]
    fn the_reason_for_failure_is_flattened_into_one_line() {
        let mut queue = queue_with(1);
        let id = queue.items[0].id;
        assert!(queue.items[0].error_line.is_empty(), "отказа ещё не было");

        queue.set_status(
            id,
            QueueStatus::Failed("Ошибка (код 1):\nERROR: Unsupported URL:\n  https://site".into()),
        );

        let line = &queue.items[0].error_line;
        assert!(!line.contains('\n'), "перенос остался: {line}");
        assert_eq!(
            line,
            "Ошибка (код 1): ERROR: Unsupported URL: https://site"
        );

        // Ушёл отказ — ушла и строка: причина позапрошлой беды рядом
        // с готовым файлом читается как новая.
        queue.set_status(id, QueueStatus::Done);
        assert!(queue.items[0].error_line.is_empty());
    }

    /// До `probe` названия нет, и в строке стоит сама ссылка: пустая строка
    /// выглядела бы поломкой, а десяток ссылок с одного сайта различается
    /// тремя символами в конце.
    #[test]
    fn the_link_gives_way_to_the_title_when_it_arrives() {
        let mut queue = Queue::new();
        let id = queue
            .push(
                request("https://site/watch?v=abc", Format::Mp4, Quality::Best),
                PathBuf::from("/dl"),
                None,
            )
            .unwrap();

        assert_eq!(queue.items[0].title, "https://site/watch?v=abc");
        queue.set_title(id, "Ролик про кота");
        assert_eq!(queue.items[0].title, "Ролик про кота");
    }

    #[test]
    fn summary_counts_every_state_a_row_can_be_in() {
        let mut queue = queue_with(5);
        let id = ids(&queue);
        queue.set_status(id[0], QueueStatus::Running);
        queue.set_status(id[1], QueueStatus::Done);
        queue.set_status(id[2], QueueStatus::Failed("не вышло".into()));
        queue.set_status(id[3], QueueStatus::Cancelled);

        assert_eq!(
            queue.summary,
            "Идёт: 1 · В очереди: 1 · Готово: 1 · Ошибок: 1 · Снято: 1"
        );

        // Пустых пар в сводке быть не должно: «Ошибок: 0» рядом с готовым
        // выглядит как доклад о беде, которой не было.
        let empty = Queue::new();
        assert!(empty.summary.is_empty());
    }

    /// Идущую загрузку не выбрасывает ни «убрать», ни «Очистить»: процесс
    /// продолжил бы качать, а показать его исход стало бы негде.
    #[test]
    fn neither_removing_nor_clearing_drops_the_running_download() {
        let mut queue = queue_with(3);
        let id = ids(&queue);
        queue.set_status(id[1], QueueStatus::Running);

        queue.remove(id[1]);
        assert!(queue.items.iter().any(|item| item.id == id[1]));

        queue.remove(id[0]);
        assert!(queue.items.iter().all(|item| item.id != id[0]));

        queue.clear();
        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.items[0].id, id[1]);
        assert_eq!(queue.summary, "Идёт: 1");
    }

    /// Слова состояний человек читает в списке глазами: одинаковые или
    /// пустые превратили бы очередь в набор одинаковых строк.
    #[test]
    fn queue_states_explain_themselves_distinctly() {
        let all = [
            QueueStatus::Waiting,
            QueueStatus::Running,
            QueueStatus::Done,
            QueueStatus::Failed(String::new()),
            QueueStatus::Cancelled,
        ];

        let mut seen: Vec<&str> = Vec::new();
        for status in &all {
            let label = status.label();
            assert!(!label.trim().is_empty(), "{status:?}: пустая подпись");
            assert!(!seen.contains(&label), "{label}: подпись повторяется");
            seen.push(label);
        }

        // Освобождать место можно только за счёт отработавших.
        assert!(!QueueStatus::Waiting.finished());
        assert!(!QueueStatus::Running.finished());
        assert!(QueueStatus::Done.finished());
        assert!(QueueStatus::Failed(String::new()).finished());
        assert!(QueueStatus::Cancelled.finished());
    }

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

    /// Настоящее сообщение wgpu 29 — то самое, от которого раньше падал
    /// процесс. Проверено вживую на окне с клиентом 504×8193.
    const TOO_LARGE: &str = "Validation Error\n\nCaused by:\n  In Surface::configure\n    \
         `Surface` width and height must be within the maximum supported texture size. \
         Requested was (504, 8193), maximum extent for either dimension is 8192.";

    #[test]
    fn oversized_window_is_explained_in_russian() {
        let line = gpu_error_line(TOO_LARGE);
        assert!(line.contains("больше, чем может отрисовать"), "{line}");
        // Английский текст wgpu пользователю не показываем: он про текстуру,
        // а человек видит окно.
        assert!(!line.contains("Surface"), "{line}");
    }

    /// Приметы принадлежат wgpu и могут смениться с новой версией. Промах не
    /// поймает ни компилятор, ни сборка, поэтому запасной исход обязателен:
    /// потерять подсказку терпимо, потерять сообщение целиком — нет.
    #[test]
    fn unknown_gpu_error_keeps_its_own_text() {
        let line = gpu_error_line("Validation Error: something else entirely");
        assert!(line.starts_with("Ошибка отрисовки: "), "{line}");
        assert!(line.contains("something else entirely"), "{line}");
    }

    /// Пока окно остаётся большим, ошибка приходит без конца. Журнал у Savio
    /// один и ограничен, так что повтор туда пускать нельзя.
    #[test]
    fn repeated_gpu_errors_are_collapsed_and_capped() {
        let errors = GpuErrors::default();
        for _ in 0..100 {
            errors.push(TOO_LARGE.to_owned());
        }
        assert_eq!(errors.take().len(), 1);

        for i in 0..100 {
            errors.push(format!("ошибка {i}"));
        }
        assert_eq!(errors.take().len(), GPU_ERROR_LIMIT);
    }

    /// Про один и тот же предел wgpu присылает два РАЗНЫХ сообщения: про
    /// поверхность и про `set_viewport`. Человеку они говорят одно и то же,
    /// поэтому повторы в журнале гасятся по готовой строке, а не по тексту
    /// wgpu. Без этого запись двоилась — видно только глазами, ни сборка,
    /// ни clippy этого не ловят.
    #[test]
    fn different_wgpu_messages_about_one_limit_give_the_same_line() {
        let viewport = "Validation Error\n\nCaused by:\n  In a CommandEncoder, label = 'encoder'\n    \
             In a set_viewport command\n      Viewport size { w: 9984, h: 381 } greater than \
             device's requested `max_texture_dimension_2d` limit 8192, or less than zero";
        assert_eq!(gpu_error_line(TOO_LARGE), gpu_error_line(viewport));
    }

    /// Кадр без ошибок не должен ничего забирать: `ui()` зовут 60 раз
    /// в секунду, и пустой разбор обязан оставаться пустым.
    #[test]
    fn a_folded_group_admits_what_it_hides() {
        // Заданный фрагмент обязан быть виден в заголовке: свёрнутая группа —
        // единственное место, где о нём вообще можно узнать, а скачанный
        // кусок вместо ролика человек заметит уже в плеере.
        assert_eq!(
            advanced_summary(false, true, false, &SubLang::Original),
            "фрагмент"
        );
        assert_eq!(
            advanced_summary(true, false, false, &SubLang::Original),
            "фрагмент задан неверно"
        );
        assert_eq!(
            advanced_summary(false, false, true, &SubLang::Original),
            "вход на сайт"
        );
        assert_eq!(
            advanced_summary(false, false, false, &SubLang::Code("ru".to_owned())),
            "субтитры: ru"
        );
        assert_eq!(
            advanced_summary(false, true, true, &SubLang::Code("de".to_owned())),
            "фрагмент · вход на сайт · субтитры: de"
        );
    }

    #[test]
    fn an_untouched_group_lists_what_lies_inside() {
        // Ничего не включено — перечисляем содержимое, а не «ничего не
        // задано»: заголовок обязан объяснять, зачем группу вообще открывать.
        assert_eq!(
            advanced_summary(false, false, false, &SubLang::Original),
            "фрагмент, вход на сайт, язык субтитров"
        );
    }

    #[test]
    fn quiet_frame_takes_nothing() {
        let errors = GpuErrors::default();
        assert!(errors.take().is_empty());
    }

    // -----------------------------------------------------------------------
    // Питание
    // -----------------------------------------------------------------------

    fn plans() -> Vec<crate::model::PowerPlan> {
        vec![
            crate::model::PowerPlan {
                id: BALANCED_PLAN,
                name: "Сбалансированная".to_owned(),
            },
            crate::model::PowerPlan {
                id: crate::model::PlanId::from_parts(
                    0x8c5e_7fda,
                    0xe8bf,
                    0x4a96,
                    [0x9a, 0x85, 0xa6, 0xe2, 0x3a, 0x8c, 0x63, 0x5c],
                ),
                name: "Высокая производительность".to_owned(),
            },
        ]
    }

    fn high_performance() -> crate::model::PlanId {
        plans()[1].id
    }

    /// При сбалансированной схеме режим питания работает — предупреждать
    /// не о чем, и лишняя жёлтая строка только пугала бы.
    #[test]
    fn the_power_hint_stays_quiet_when_the_mode_really_applies() {
        let state = PowerState {
            plans: plans(),
            active: Some(BALANCED_PLAN),
            modes: PowerModes::Known {
                effective: Some(PowerMode::Max),
                ignored: None,
            },
            trouble: None,
        };
        assert_eq!(power_hint(&state), "");
    }

    /// Оговорка обязана появиться ДО нажатия: это единственное место, где
    /// Savio успевает предупредить о молчаливом отказе Windows.
    #[test]
    fn the_power_hint_warns_before_the_press_when_the_scheme_is_not_balanced() {
        let state = PowerState {
            plans: plans(),
            active: Some(high_performance()),
            modes: PowerModes::Known {
                effective: Some(PowerMode::Balanced),
                ignored: None,
            },
            trouble: None,
        };

        let hint = power_hint(&state);
        // Названа и та схема, что мешает, и та, что нужна: без первой
        // непонятно, что менять, без второй — на что.
        assert!(hint.contains("Высокая производительность"), "{hint}");
        assert!(hint.contains("Сбалансированная"), "{hint}");
    }

    /// Windows запомнила одно, а работает по-другому. Пока это не сказано
    /// словами, окно показывает выбранным режим, которого нет.
    #[test]
    fn the_power_hint_names_both_modes_when_the_stored_one_is_ignored() {
        let state = PowerState {
            plans: plans(),
            active: Some(high_performance()),
            modes: PowerModes::Known {
                effective: Some(PowerMode::Balanced),
                ignored: Some(PowerMode::Max),
            },
            trouble: None,
        };

        let hint = power_hint(&state);
        assert!(hint.contains(PowerMode::Max.label()), "{hint}");
        assert!(hint.contains(PowerMode::Balanced.label()), "{hint}");
    }

    /// Схему система назвать может и не суметь. Выдуманного имени в оговорке
    /// быть не должно — человек пойдёт искать его в параметрах Windows.
    #[test]
    fn the_power_hint_does_not_invent_a_scheme_name() {
        let state = PowerState {
            plans: Vec::new(),
            active: Some(high_performance()),
            modes: PowerModes::Known {
                effective: Some(PowerMode::Balanced),
                ignored: None,
            },
            trouble: None,
        };

        let hint = power_hint(&state);
        assert!(!hint.is_empty(), "предупредить всё равно надо");
        assert!(!hint.contains("Высокая производительность"), "{hint}");
    }

    /// Без режимов питания оговорка про них бессмысленна.
    #[test]
    fn the_power_hint_says_nothing_where_there_are_no_modes() {
        let state = PowerState {
            plans: plans(),
            active: Some(high_performance()),
            modes: PowerModes::Unsupported,
            trouble: Some("режимов нет".to_owned()),
        };
        assert_eq!(power_hint(&state), "");
    }
}
