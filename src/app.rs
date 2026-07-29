//! Состояние и отрисовка интерфейса.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::engine::settings;
use crate::engine::setup;
use crate::engine::{self, Handle, MetaTask, metadata};
use crate::model::{
    CookieSource, DownloadId, DownloadOptions, Event, Format, MediaInfo, Progress, Quality, Request,
    Section, SectionError, Tag, human_bytes, human_duration, human_speed, looks_like_url, meta_kind,
    parse_section,
};
use crate::theme;

const LOG_LIMIT: usize = 400;

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
    fn push(&mut self, request: Request, out_dir: PathBuf) -> Option<DownloadId> {
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
            title: request.url.clone(),
            detail: String::new(),
            error_line: String::new(),
            request,
            out_dir,
            status: QueueStatus::Waiting,
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

    /// Что запускать следующим: номер, запрос и папка.
    ///
    /// Отдаёт копии, а не ссылки: `engine::start` забирает `Request` во
    /// владение, а строка обязана остаться в списке — по ней рисуется
    /// состояние, и в неё же приходит исход.
    fn next_waiting(&self) -> Option<(DownloadId, Request, PathBuf)> {
        let item = self
            .items
            .iter()
            .find(|item| item.status == QueueStatus::Waiting)?;
        Some((item.id, item.request.clone(), item.out_dir.clone()))
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
                | Event::Versions(_) => {}
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
    /// Готовые строки версий для строки обслуживания — по одной на инструмент.
    ///
    /// Две, а не одна общая: длинная версия ffmpeg
    /// (`N-125365-g9a01c1cb6a-20260630`) в окне минимальной ширины не влезает,
    /// и в склеенной строке она вытеснила бы за кромку версию yt-dlp — то есть
    /// более нужную из двух. Каждая обрезается сама за себя.
    ytdlp_version_line: String,
    ffmpeg_version_line: String,
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
            versions_rx: None,
            meta_line: String::new(),
            progress_line: String::new(),
            done_path_display: String::new(),
            quality_note: String::new(),
            // Пустые до первого ответа: строка обслуживания просто не показывает
            // версий, пока их не спросили, — «неизвестно» там было бы враньём
            // на те доли секунды, что идёт опрос.
            ytdlp_version_line: String::new(),
            ffmpeg_version_line: String::new(),
            url_invalid: false,
            log_copied_at: None,
            tab: Tab::Download,
            meta: MetaPanel::new(),
            history: History::default(),
            queue: Queue::new(),
            maximize_pending: true,
            saver: settings::Saver::spawn(),
            gpu_errors: Arc::default(),
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
            section: self.section,
        };

        if self.queue.push(request, out_dir).is_none() {
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
        let Some((id, request, out_dir)) = self.queue.next_waiting() else {
            return;
        };

        let (tx, rx) = channel();
        let notify_ctx = ctx.clone();

        match engine::start(id, request, out_dir, tx, move || notify_ctx.request_repaint()) {
            Ok(handle) => {
                self.queue.set_status(id, QueueStatus::Running);
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
                Event::Log(line) => {
                    self.log.push(line);
                    if self.log.len() > LOG_LIMIT {
                        self.log.drain(..self.log.len() - LOG_LIMIT);
                    }
                }
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
                Event::Tags(_) | Event::Cleaned(_) | Event::Versions(_) => {}
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

        // Строки собираем здесь, на приёме события, а не в кадре отрисовки:
        // меняются они дважды за запуск, а `ui()` зовут 60 раз в секунду.
        self.ytdlp_version_line = version_line("yt-dlp", &versions.ytdlp);
        self.ffmpeg_version_line = version_line("ffmpeg", &versions.ffmpeg);
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
        self.drain_versions();
        self.drain_gpu_errors();

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
        // Очередь идёт под состоянием и над обслуживанием: она про текущую
        // работу, а «Обновить движок» — про то, за чем идут, когда работа
        // не пошла.
        self.queue_section(ui);
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
            self.cancel_setup(ctx);
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
                state.corner_radius = egui::CornerRadius::same(theme::RADIUS);
            }
            // Двойное ослабление не нужно: приглушённый жёлтый уже задан
            // явно, а поверх него прозрачность съела бы кнопку целиком.
            v.disabled_alpha = 1.0;

            ui.add_enabled(
                enabled,
                egui::Button::new(egui::RichText::new("Скачать").strong())
                    .min_size(egui::vec2(width, theme::CTA_HEIGHT)),
            )
            .on_disabled_hover_text(hint)
            .clicked()
        })
        .inner
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
            State::Queued => {
                ui.label(
                    egui::RichText::new(
                        "Ссылки ждут в очереди. Нажмите «Скачать» — они пойдут \
                         по одной, сверху вниз.",
                    )
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

    /// Список того, что стоит в очереди.
    ///
    /// Показывается, только когда очередь непуста: пустая карточка на экране
    /// человека, который очередью не пользуется, отнимала бы место у главного
    /// и объясняла бы то, о чём он не спрашивал.
    fn queue_section(&mut self, ui: &mut egui::Ui) {
        if self.queue.items.is_empty() {
            return;
        }

        // Что нажали, решаем после отрисовки: менять список, пока по нему
        // идёт цикл, нельзя, а откладывать решение до следующего кадра —
        // значит терять его при быстром щелчке.
        let mut remove: Option<DownloadId> = None;
        let mut clear = false;

        ui.add_space(16.0);
        egui::Frame::new()
            .fill(theme::BG_SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BORDER_SUBTLE))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                // Без этого карточка сжалась бы по ширине самой длинной
                // строки списка — то же, что и у карточки истории.
                ui.set_width(ui.available_width());

                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Очередь")
                            .small()
                            .color(theme::TEXT_SECONDARY),
                    );

                    // Кнопка прижата к правому краю, сводка — к ней:
                    // в окне шириной 520 сводка обрежется, а кнопка нет.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        clear = ui
                            .add(
                                egui::Button::new("Очистить")
                                    .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                            )
                            .on_hover_text(
                                "Список опустеет: уйдут и скачанные, и те, что ещё \
                                 ждут. Идущая загрузка не прервётся — её \
                                 останавливает «Отмена».",
                            )
                            .clicked();

                        // Подсказку с полной сводкой вешает сама обрезанная
                        // метка (`show_tooltip_when_elided`) — свой
                        // `on_hover_text` рядом дал бы вторую коробку
                        // с тем же текстом.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.queue.summary)
                                    .small()
                                    .color(theme::TEXT_MUTED),
                            )
                            .truncate(),
                        );
                    });
                });

                ui.add_space(10.0);

                // Строки лежат прямо в общей прокрутке, своей у списка нет —
                // ровно как у вкладки «История», и по той же причине: полоса
                // рядом с полосой только мешает. Здесь у этого есть и вторая,
                // более жёсткая причина. Вложенная вертикальная прокрутка
                // берёт высоту из `available_rect_before_wrap()`, а внутри
                // другой прокрутки это не «сколько влезет в окно», а «сколько
                // осталось от её видимой части». Карточка очереди лежит низко,
                // остаток к ней нулевой — и список схлопывается до
                // `min_scrolled_size`, то есть до 64 точек (scroll_area.rs:774).
                // Выходит окошко в одну строку с обрезанной подписью, и
                // `max_height` тут ни при чём. Проверено глазами; ни сборка,
                // ни `clippy`, ни тесты этого не видят.
                //
                // Плата — длинная страница при длинной очереди. Она честная:
                // человек, поставивший полсотни ссылок, их и хочет видеть,
                // а потолок в `QUEUE_LIMIT` держит длину конечной.
                for (index, item) in self.queue.items.iter().enumerate() {
                    if index > 0 {
                        ui.add_space(6.0);
                    }
                    if queue_row(ui, item) {
                        remove = Some(item.id);
                    }
                }

                ui.add_space(10.0);
                note(
                    ui,
                    "Ссылки качаются по одной, сверху вниз. Сорвавшаяся не \
                     останавливает остальные. На диск список не пишется и при \
                     закрытии Savio исчезает.",
                    theme::TEXT_MUTED,
                );
            });

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

    /// Обслуживание: версии инструментов и их обновление.
    ///
    /// Стоит внизу, рядом с журналом, а не у кнопки «Скачать», и намеренно:
    /// это то, за чем идут, когда что-то перестало работать, — соседство
    /// с журналом и версией в шапке тут уместнее, чем спор за внимание
    /// с главным действием экрана.
    ///
    /// Подписи на отдельных строках под кнопками, а не сбоку: в окне
    /// минимальной ширины (520) строка рядом с кнопкой не поместилась бы.
    fn maintenance_row(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(12.0);

        // Пока занят единственный канал событий — обновляться нечем: и
        // загрузка, и установка ходят через тот же `rx`.
        let enabled = !matches!(self.state, State::Running) && !self.setup.busy();

        let clicked = ui
            .add_enabled_ui(enabled, |ui| {
                ui.horizontal(|ui| {
                    let ytdlp = ui
                        .add(
                            egui::Button::new("Обновить движок")
                                .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                        )
                        .clicked();
                    let ffmpeg = ui
                        .add(
                            egui::Button::new("Обновить ffmpeg")
                                .min_size(egui::vec2(0.0, theme::CONTROL_HEIGHT)),
                        )
                        .clicked();
                    match (ytdlp, ffmpeg) {
                        (true, _) => Some(setup::Component::Ytdlp),
                        (_, true) => Some(setup::Component::Ffmpeg),
                        _ => None,
                    }
                })
                .inner
            })
            .inner;

        // Версии под кнопками, а не в шапке: там уже стоит версия самого Savio,
        // и три номера подряд в одной строке не различить глазами. Пусто до
        // первого ответа — опрос идёт в отдельном потоке и занимает мгновение.
        if !self.ytdlp_version_line.is_empty() {
            ui.add_space(8.0);
            for line in [&self.ytdlp_version_line, &self.ffmpeg_version_line] {
                // `truncate()`, а не перенос: версия ffmpeg у git-сборки — это
                // `N-125365-g9a01c1cb6a-20260630`, и в узком окне она заняла бы
                // две строки, растащив пару подписей по высоте. Обрезанную
                // метку egui сам показывает целиком по наведению, поэтому
                // своей подсказки здесь нет — вторая была бы дублем (Правило 4).
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(line).small().color(theme::TEXT_SECONDARY),
                    )
                    .truncate(),
                );
            }
        }

        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "Сайты меняются, и старый yt-dlp перестаёт их скачивать. \
                     Если ссылка вдруг не работает — обновите движок. \
                     ffmpeg обновляют редко, и стоит это дороже: свежая сборка \
                     качается целиком, больше сотни мегабайт.",
                )
                .small()
                .color(theme::TEXT_MUTED),
            )
            // Без явного переноса длинный абзац в узком окне уходит за кромку:
            // `ui.label` берёт там режим `Extend` и кладёт его в одну строку.
            .wrap(),
        );

        if let Some(what) = clicked {
            let ctx = ui.ctx().clone();
            self.start_update(what, &ctx);
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

/// Одна строка очереди. Возвращает `true`, если нажали «убрать».
///
/// Свободная функция, а не метод: строке нужен только сам элемент, и от
/// заимствования всего `SavioApp` внутри цикла по списку это избавляет.
fn queue_row(ui: &mut egui::Ui, item: &QueueItem) -> bool {
    let mut remove = false;

    egui::Frame::new()
        .fill(theme::BG_ELEVATED)
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_SMALL))
        .inner_margin(egui::Margin::symmetric(12, 10))
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
    use crate::model::NO_DOWNLOAD;

    fn request(url: &str, format: Format, quality: Quality) -> Request {
        Request {
            url: url.to_owned(),
            format,
            quality,
            options: DownloadOptions::default(),
            section: Section::default(),
            cookies: CookieSource::default(),
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
                        PathBuf::from("/dl")
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
            )
            .unwrap();
        let wrapped = queue
            .push(
                request("https://site/b", Format::Mp4, Quality::Best),
                PathBuf::from("/dl"),
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
        assert_eq!(queue.push(extra.clone(), PathBuf::from("/dl")), None);
        assert_eq!(queue.items.len(), QUEUE_LIMIT, "ожидающую не выбросили");

        // Первая скачалась — место освободилось, и уходит именно она.
        let oldest = queue.items[0].id;
        queue.set_status(oldest, QueueStatus::Done);
        assert!(!queue.full);

        assert!(queue.push(extra, PathBuf::from("/dl")).is_some());
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
    fn quiet_frame_takes_nothing() {
        let errors = GpuErrors::default();
        assert!(errors.take().is_empty());
    }
}
