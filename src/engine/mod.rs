//! Движок загрузки.
//!
//! Про UI не знает ничего: наружу торчит канал `Event` и колбэк-будильник.
//! Благодаря этому движок можно прицепить к CLI или к тестам, не трогая код.

pub mod binaries;
mod cut;
pub mod hardware;
pub mod metadata;
pub mod monitor;
pub mod power;
pub mod settings;
pub mod setup;
pub mod sha256;
pub mod thumbnail;
mod tree;
pub mod ytdlp;

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::model::{
    CookieSource, DownloadId, Event, MediaInfo, NO_DOWNLOAD, Request, SectionPlan, SubtitlePlan,
    human_duration,
};

pub use binaries::{Tools, discover};

/// Есть ли на машине ffmpeg.
///
/// Отдельный вопрос вместо `discover()` нужен UI: без ffmpeg галочки «вшить»
/// не сработают, и сказать об этом надо **до** нажатия «Скачать», а не после
/// впустую скачанного ролика. Возвращаем `bool`, а не путь: слою UI знать
/// имена бинарников и каталоги поиска незачем.
pub fn has_ffmpeg() -> bool {
    binaries::locate(binaries::FFMPEG_NAME).is_some()
}

/// Чем «Отмена» останавливает загрузку. Общее у ручки и у потока движка.
///
/// Половин три, и все обязательны. Убить процесс можно, только когда он есть,
/// а первые секунды после «Скачать» его нет: идёт `probe`, потом обложка, потом
/// сам запуск. Всё это время слот пуст, и одного слота хватало ровно на то,
/// чтобы отмена не отменяла ничего: окно закрывалось, а файл спокойно
/// докачивался. Флаг отвечает на другой вопрос — «просили ли отменить», —
/// и потому виден движку до появления процесса. Приём тот же, что у установки
/// инструментов (`setup::Handle`), там он проверен.
///
/// Третья половина — [`tree::Group`]: слот держит **один** процесс, тот, что
/// запустил сам Savio, а качает при обрезке его внук. Убийство одного процесса
/// оставляло дерево работать (дефект 25), и группа отвечает на третий вопрос —
/// «кого ещё он за собой привёл».
#[derive(Clone, Default)]
struct Control {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    group: Arc<tree::Group>,
}

impl Control {
    /// Просили ли отменить.
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Готовит команду к запуску под присмотром «Отмены».
    ///
    /// Звать обязательно перед каждым `spawn`, чей процесс потом уйдёт в
    /// `adopt`: на Unix своя группа процессов заказывается только при запуске.
    /// Пропуск не заметят ни сборка, ни `clippy`, ни тесты — просто «Отмена»
    /// снова достанет один процесс из трёх.
    fn arm(&self, cmd: &mut Command) {
        self.group.arm(cmd);
    }

    /// Отдаёт запущенный процесс под присмотр «Отмены».
    ///
    /// Возвращает `false`, если отменить успели, пока процесс запускался: тогда
    /// он тут же и убит, а звать его снова незачем.
    ///
    /// Проверка флага и запись в слот идут под одним замком намеренно, и это
    /// не перестраховка. Между проверкой перед запуском и этой строкой лежит
    /// сам `spawn` — на Windows это десятки миллисекунд, и отмена, попавшая
    /// ровно в них, не нашла бы в слоте ничего. Под общим замком третьего
    /// исхода нет: либо `cancel()` видит в слоте процесс и убивает его, либо
    /// мы видим её флаг и не начинаем.
    fn adopt(&self, mut child: Child) -> Result<bool, String> {
        // В группу процесс попадает раньше всех проверок и до замка. Отмена,
        // пришедшая в это самое мгновение, ничего не теряет: либо она увидит
        // процесс уже в группе и убьёт дерево сама, либо мы увидим её флаг
        // и убьём его сами — а группа к тому времени будет заполнена.
        self.group.adopt(&child);

        match self.child.lock() {
            Ok(mut slot) => {
                if !self.cancelled() {
                    *slot = Some(child);
                    return Ok(true);
                }
            }
            // Замок отравлен: в слот процесс не положить, а оставить его
            // работать нельзя тем более — «Отмена» до него уже не доберётся.
            Err(_) => {
                self.kill_tree(&mut child);
                return Err("Внутренняя ошибка синхронизации".into());
            }
        }
        // Замок здесь уже отпущен: `wait()` под ним заставил бы `cancel()`
        // ждать нас, а её задача — вернуть управление сразу.
        self.kill_tree(&mut child);
        Ok(false)
    }

    /// Убивает процесс вместе со всем, что он успел запустить, и дожидается
    /// его конца.
    ///
    /// Группу бьём первой: у убитого родителя потомки не умирают сами,
    /// а вот `wait()` по убитому родителю вернётся сразу.
    fn kill_tree(&self, child: &mut Child) {
        self.group.kill();
        kill_and_reap(child);
    }

    /// Освобождает слот и группу после того, как процесс дождались.
    ///
    /// Обе половины вместе, а не порознь: в тот же слот после `probe` ложится
    /// сам yt-dlp, а после yt-dlp — наш ffmpeg, и оставленные там следы
    /// прошлого процесса сбивали бы «Отмену» с толку.
    fn release(&self) {
        self.group.release();
        if let Ok(mut guard) = self.child.lock() {
            *guard = None;
        }
    }
}

/// Убивает процесс и дожидается его конца.
///
/// Ждать обязательно: `Child::drop` этого не делает, и на Unix убитый, но
/// не дождавшийся потомок остаётся зомби до конца жизни Savio.
///
/// Одного процесса тут хватает только там, где потомством он обзавестись
/// не успел, — то есть сразу после `spawn`. Всё остальное убивается вместе
/// с деревом (`Control::kill_tree`).
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Ручка запущенной работы: позволяет её остановить.
///
/// Одна на два дела — на загрузку и на фоновый запрос метаданных
/// (`start_probe`). Останавливаются они одинаково: пометить флаг и убить
/// процесс, — и отдельный тип отличался бы от этого только именем.
pub struct Handle {
    control: Control,
}

impl Handle {
    /// Просит движок остановиться и убивает процесс, если он уже есть.
    /// Событие `Failed` при этом не шлётся — отмену UI показывает сам,
    /// чтобы не выглядела как ошибка.
    pub fn cancel(&self) {
        // Флаг ставится ДО замка, и порядок этих двух строк — не вкусовщина:
        // под этим же замком движок решает, отдавать ли ему только что
        // запущенный процесс (см. `Control::adopt`). Пока пометка идёт раньше
        // захвата, промахнуться мимо друг друга мы не можем. Переставь строки
        // местами — и вернётся ровно то окно, ради которого флаг заводился,
        // причём не сломается при этом ни сборка, ни тесты.
        self.control.cancelled.store(true, Ordering::Relaxed);

        // Дерево убиваем до замка и вместо одного процесса. Слот держит того,
        // кого запустили мы, а качает при обрезке его внук: убийство одного
        // процесса оставляло ffmpeg и рабочий yt-dlp работать, и файл спокойно
        // дописывался после «Отменено» в окне (дефект 25).
        self.control.group.kill();

        // Свой процесс всё-таки добиваем отдельно: группы могло и не быть —
        // Windows вправе отказать в Job Object, а на Unix `spawn` мог ещё
        // не случиться. Тогда это единственное, что «Отмену» и делает.
        if let Ok(mut guard) = self.control.child.lock()
            && let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
    }
}

/// Запускает загрузку в отдельном потоке.
///
/// `notify` вызывается после каждого события — UI на нём делает repaint.
///
/// `id` движок не выдумывает, а получает снаружи и только проставляет в
/// события: очередь ведёт UI, и знать о ней движку незачем. Одиночной
/// загрузке (CLI, тест) годится любое ненулевое число.
///
/// `known` — ответ `probe`, который у вызывающего уже есть: UI спрашивает
/// метаданные ещё при вставке ссылки (`start_probe`), и спрашивать их второй
/// раз значило бы сходить на сайт дважды за одним и тем же — а лишние запросы
/// на один ролик это прямая дорога к «Sign in to confirm you're not a bot».
/// Вместе с ответом у него есть и обложка, поэтому при `Some` движок не тянет
/// ни того ни другого. `None` — обычный путь (CLI, тесты, ссылка из очереди,
/// о которой не спрашивали): движок спросит сам, как и раньше.
pub fn start(
    id: DownloadId,
    request: Request,
    out_dir: PathBuf,
    known: Option<MediaInfo>,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) -> Result<Handle, String> {
    let tools = discover()?;

    // Предупреждения копим, а шлём одним сообщением. Причина в UI:
    // `Event::Warning` живёт там в единственном поле, и второе сообщение
    // молча затёрло бы первое — то есть про одну из двух бед человек не
    // узнал бы вовсе. Случаются они независимо: файл cookies мог пропасть
    // и у того, у кого нет ffmpeg.
    let mut warnings: Vec<String> = Vec::new();

    if tools.ffmpeg.is_none() {
        let _ = tx.send(Event::Log(
            "ffmpeg не найден — склейка видео со звуком и конвертация в MP3 работать не будут."
                .into(),
        ));

        // Про несделанное говорим отдельно и громче журнала: журнал свёрнут
        // и очищается перед каждой загрузкой, а человек поставил галочку
        // (или набрал границы фрагмента) и вправе узнать, что они не
        // сработали. Сами ключи в командную строку не попадут (см.
        // `download_args`): вшивание сорвало бы загрузку на постобработке,
        // а обрезка оборвала бы её ещё до начала.
        //
        // Предупреждение одно на оба случая, хотя случая два: перечислять
        // потерянное списком не нужно, фраза покрывает обе беды разом.
        let warning = match (request.section.any(), request.options.any()) {
            (true, true) => Some(
                "Без ffmpeg не выйдет ни вырезать фрагмент, ни вшить метаданные, \
                 обложку и субтитры: ролик скачается целиком и без них.",
            ),
            (true, false) => Some(
                "Вырезать фрагмент без ffmpeg нельзя — ролик скачается целиком.",
            ),
            (false, true) => Some(
                "Вшить метаданные, обложку и субтитры без ffmpeg нельзя — файл \
                 сохранится без них.",
            ),
            (false, false) => None,
        };
        if let Some(warning) = warning {
            warnings.push(format!(
                "{warning} Перезапустите Savio: при старте он сам попробует \
                 скачать ffmpeg ещё раз."
            ));
        }
    }

    if let Some(warning) = cookie_file_trouble(request.cookies, request.cookie_file.as_deref()) {
        warnings.push(warning);
    }

    if !warnings.is_empty() {
        let _ = tx.send(Event::Warning(warnings.join("\n\n")));
    }

    // ffprobe yt-dlp ищет **сам** — рядом с тем ffmpeg, что назван в
    // `--ffmpeg-location`; отдельного ключа под него нет. Если пара разъехалась
    // по разным папкам (системный ffmpeg в PATH, а докачанный нами ffprobe —
    // в каталоге данных), yt-dlp наш ffprobe не увидит и HLS-поток не починит.
    // Ошибки при этом не будет никакой: пропадёт лишь редкая ветка работы,
    // ровно тот молчаливый отказ, о котором Правило 6. Сказать об этом может
    // только журнал — на загрузку целиком это не влияет, и поднимать из-за
    // этого баннер значило бы пугать там, где почти всегда всё в порядке.
    if let (Some(ffmpeg), Some(ffprobe)) = (&tools.ffmpeg, &tools.ffprobe)
        && ffmpeg.parent() != ffprobe.parent()
    {
        let _ = tx.send(Event::Log(format!(
            "ffprobe лежит не рядом с ffmpeg ({} и {}) — yt-dlp его не увидит.",
            ffmpeg.display(),
            ffprobe.display()
        )));
    }

    let control = Control::default();
    let handle = Handle {
        control: control.clone(),
    };

    std::thread::spawn(move || {
        let job = Job {
            id,
            request: &request,
            known,
            out_dir: &out_dir,
        };
        let result = run(job, &tools, &tx, &notify, &control);
        if let Err(err) = result {
            // Отменённая загрузка кончается ненулевым кодом убитого процесса,
            // то есть выглядит отсюда точно как сорвавшаяся. Ошибкой она не
            // является, и слать `Failed` нельзя: отмену UI показывает сам,
            // словом «Отменено» (Правило 2). Раньше это спасал только UI —
            // он роняет приёмник вместе с отменой, — но движок обязан
            // оставаться пригодным для CLI и тестов без единой правки, а там
            // ронять нечего, и запоздалое «Ошибка» досталось бы человеку.
            if control.cancelled() {
                return;
            }
            let _ = tx.send(Event::Failed { id, message: err });
            notify();
        }
    });

    Ok(handle)
}

/// Что не так с выбранным файлом cookies, словами для баннера.
/// `None` — файл на месте и годен либо вход берут не из файла.
///
/// Проверять приходится нам, потому что yt-dlp не проверяет. **Пропавший
/// файл он не замечает вовсе**: проверено вживую (2026.07.04) — код возврата
/// 0, ролик скачивается, cookies не применяются, и в выводе об этом ни слова
/// даже без `--quiet` и `--no-warnings`. Молчание тут худшее из возможного:
/// человек, у которого файл переименовали, получил бы «видео с возрастным
/// ограничением» и пошёл бы чинить не то.
///
/// Закрытый на запись файл, наоборот, беда громкая, но приходит она в самый
/// неудачный момент. После работы yt-dlp **дописывает в этот файл свежие
/// cookies**, и на файле с атрибутом «Только чтение» он падает питоновским
/// трейсбеком уже поверх скачанного ролика (проверено вживую). Сказанное
/// заранее даёт отменить загрузку, пока она ничего не стоила; сказанное
/// потом — только объяснить (см. примету `cookies.py` в `FAILURE_HINTS`).
///
/// Имя файла, а не путь: полный путь и так виден строкой под списком, а
/// баннер от него растянулся бы на три строки.
fn cookie_file_trouble(cookies: CookieSource, file: Option<&Path>) -> Option<String> {
    if cookies != CookieSource::File {
        return None;
    }

    let Some(file) = file else {
        return Some(
            "Вход просят из файла, а сам файл не выбран — ролик скачается без \
             входа в аккаунт. Выберите файл под списком «Вход на сайт» или \
             верните в нём пункт «Не использовать»."
                .to_owned(),
        );
    };

    let name = file
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("cookies"))
        .to_string_lossy();

    match std::fs::metadata(file) {
        Err(_) => Some(format!(
            "Файл cookies «{name}» не найден — ролик скачается без входа в \
             аккаунт, и закрытый сайт его, скорее всего, не отдаст. Файл \
             переименовали, перенесли или удалили: выберите его заново под \
             списком «Вход на сайт»."
        )),
        // Права на запись спрашиваем у самого файла. На Unix это ещё не вся
        // правда (у чужого файла с правами 644 бит записи есть, а нам он всё
        // равно не по зубам), но случай с атрибутом «Только чтение» — тот,
        // который встречается, — закрывает.
        Ok(meta) if meta.permissions().readonly() => Some(format!(
            "Файл cookies «{name}» закрыт для записи, а yt-dlp дописывает \
             в него свежие cookies после загрузки — и оборвётся ошибкой, \
             когда ролик уже будет скачан. Снимите с файла «Только чтение» \
             или скопируйте его в обычную папку."
        )),
        Ok(_) => None,
    }
}

/// Что качаем: всё, что пришло снаружи, одной посылкой.
///
/// Собрано в структуру не для красоты. Восемь отдельных аргументов у `run`
/// clippy уже не пропускает, а два соседних `&Path` в таком списке недолго
/// и переставить местами — компилятор этого не заметит.
struct Job<'a> {
    id: DownloadId,
    request: &'a Request,
    /// Готовый ответ `probe` от вызывающего — см. `known` у [`start`].
    known: Option<MediaInfo>,
    out_dir: &'a Path,
}

fn run(
    job: Job<'_>,
    tools: &Tools,
    tx: &Sender<Event>,
    notify: &(impl Fn() + Send + 'static),
    control: &Control,
) -> Result<(), String> {
    let Job {
        id,
        request,
        known,
        out_dir,
    } = job;

    let _ = tx.send(Event::Stage("Читаю ссылку…".into()));
    notify();

    // Что просить по субтитрам: до `probe` этого знать неоткуда — ни языка
    // ролика, ни того, выложил ли автор свои субтитры, — а после уже поздно
    // спрашивать человека. Поэтому решаем сами (см. `SubtitlePlan`). Провал
    // `probe` оставляет слепой план: ключа языка не будет, и поведение
    // окажется ровно тем, что было до появления выбора.
    let mut subs = SubtitlePlan::blind(request.options.auto_subs);

    // Обложку тянем только тогда, когда спрашивали сами: с готовым ответом
    // она у вызывающего уже есть (см. `known` в `start`), и второй заход за
    // той же картинкой — это секунды задержки перед началом загрузки впустую.
    let cover_wanted = known.is_none();

    // Длительность ролика нужна и после разбора метаданных: по ней выбирается
    // путь к фрагменту (`Section::plan`). `None` — `probe` не ответил, и это
    // законный исход: тогда путь останется прежним.
    let mut duration: Option<f64> = None;

    // Метаданные тянем отдельным быстрым вызовом, чтобы показать название
    // ещё до старта загрузки. Если не вышло — не страшно, идём дальше.
    if let Some(info) = known.or_else(|| probe(request, tools, control)) {
        // Адрес, длительность и план по субтитрам забираем до отправки:
        // `Info` уходит в UI вместе со структурой.
        let cover = info.thumbnail_url.clone();
        duration = info.duration_secs;
        subs = info.subtitle_plan(&request.sub_lang, request.options.auto_subs);
        // Провал отправки означает, что приёмник закрыт, то есть загрузку уже
        // отменили. Проверяем это именно здесь и именно ради обложки: запрос
        // за ней стоит **перед** запуском yt-dlp и занимает до нескольких
        // секунд. Всё это время «Отмена» убивать ещё нечего, и без проверки
        // нажатие на неё оборачивалось бы ожиданием чужого сервера впустую.
        let listening = tx.send(Event::Info(info)).is_ok();
        notify();

        // Начало фрагмента за пределами ролика — единственная ошибка в
        // диапазоне, которую нельзя поймать до запуска: длительность мы
        // узнаём только отсюда. Ошибкой её не считает никто — ни yt-dlp,
        // ни ffmpeg: проверено вживую, у 19-секундного ролика диапазон
        // 100–200 дал код возврата 0 и файл с восемью секундами от начала.
        // Сказать об этом можем только мы, и сказать надо: человек получит
        // не тот кусок и без предупреждения решит, что обрезка не работает.
        //
        // Спрашиваем при живом ffmpeg: без него ключа обрезки в командной
        // строке нет вовсе, и своё предупреждение там уже ушло (см. `start`).
        if let Some(start) = request.section.start
            && let Some(duration) = duration
            && tools.ffmpeg.is_some()
            && duration > 0.0
            && start as f64 >= duration
        {
            let _ = tx.send(Event::Warning(format!(
                "Начало фрагмента ({}) лежит за концом ролика ({}) — вырезать \
                 оттуда нечего, и файл получится не тот, что вы ждёте. \
                 Поправьте границы и запустите снова.",
                human_duration(start),
                human_duration(duration as u64)
            )));
            notify();
        }

        // Обложку тянем после `Info`, а не вместо него: название должно
        // появиться сразу, не дожидаясь картинки.
        if cover_wanted && listening {
            send_cover(cover, tx, notify);
        }
    }

    // Отмена, нажатая в первые секунды. Всё, что было выше (`probe`, обложка),
    // идёт до появления процесса, а убивать «Отмена» умеет только процесс —
    // потому и спрашиваем флаг сами. Уходим молча, без `Failed`: UI обязан
    // показать «Отменено», а не ошибку (Правило 2).
    //
    // Проверка стоит до запуска, а не после: yt-dlp, убитый через мгновение
    // после старта, успевает оставить в папке `.part`-огрызок.
    if control.cancelled() {
        return Ok(());
    }

    // Каким путём брать фрагмент. Без ffmpeg вопроса нет вовсе: резать нечем,
    // ключа обрезки в командной строке не будет, и ролик приедет целиком —
    // про это в `start` уже сказано словами.
    let plan = match tools.ffmpeg {
        Some(_) => request.section.plan(duration),
        None => SectionPlan::Whole,
    };
    if plan == SectionPlan::CutAfter {
        // В журнал, а не баннером: беды здесь нет, а объяснение «почему в
        // папке на минуту появился целый ролик» пригодится. Человеку про
        // выбранный путь говорит оговорка под полями фрагмента.
        let _ = tx.send(Event::Log(
            "Фрагмент занимает больше половины ролика: качаем целиком быстрым \
             путём и вырежем кусок сами."
                .into(),
        ));
    }

    let args = ytdlp::download_args(request, out_dir, tools, &subs, plan);

    // Второй признак того, что UI про нас забыл, — закрытый приёмник, и он
    // не заменяет флаг, а дополняет: флаг отвечает «просили ли отменить»,
    // приёмник — «слушает ли кто-нибудь». Совпадают они не всегда: канал
    // роняет и закрытие окна. Уйти по этому признаку так же важно: для
    // очереди запуск после забвения опаснее, чем для одиночной загрузки —
    // следующий её элемент уже пошёл, и мы получили бы два yt-dlp разом,
    // вопреки правилу «строго по одному». Своего события не нужно —
    // достаточно посмотреть на исход отправки той строки, что и так уходит
    // в журнал.
    // Строку собирает `log_args`, а не `join`: путь к файлу cookies — это
    // каталоги и имя пользователя, а журнал человек копирует кнопкой и
    // вкладывает в сообщение о проблеме.
    if tx
        .send(Event::Log(format!("yt-dlp {}", ytdlp::log_args(&args))))
        .is_err()
    {
        return Ok(());
    }

    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    ytdlp::hide_console(&mut cmd);
    control.arm(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Не удалось запустить yt-dlp: {e}"))?;

    let stdout = child.stdout.take().ok_or("Нет stdout у yt-dlp")?;
    let stderr = child.stderr.take().ok_or("Нет stderr у yt-dlp")?;

    // Отмена могла прийти, пока процесс запускался. Тогда он уже убит,
    // и дальше идти незачем — снова молча, без `Failed`.
    if !control.adopt(child)? {
        return Ok(());
    }

    // stderr читаем отдельным потоком: под --quiet туда уходит прогресс
    // постобработки, а при падении — текст ошибки, который нужен целиком.
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let stderr_thread = {
        let errors = Arc::clone(&errors);
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                if let ytdlp::Line::Stage(stage) = ytdlp::parse_line(&line) {
                    let _ = tx.send(Event::Stage(stage));
                    continue;
                }
                if let Ok(mut guard) = errors.lock() {
                    guard.push(line.clone());
                }
                let _ = tx.send(Event::Log(line));
            }
        })
    };

    let mut final_path: Option<PathBuf> = None;
    let _ = tx.send(Event::Stage("Загрузка…".into()));
    notify();

    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        match ytdlp::parse_line(&line) {
            // Номер загрузки ставим здесь, а не в `parse_line`: тот разбирает
            // одну строку вывода и про очередь ничего не знает — тем он и
            // остаётся чистой функцией, которую легко покрыть тестами.
            ytdlp::Line::Progress(mut p) => {
                p.download_id = id;
                let _ = tx.send(Event::Progress(p));
            }
            ytdlp::Line::Stage(stage) => {
                let _ = tx.send(Event::Stage(stage));
            }
            ytdlp::Line::Done(path) => {
                final_path = Some(path);
            }
            ytdlp::Line::Other(text) if !text.is_empty() => {
                let _ = tx.send(Event::Log(text));
            }
            ytdlp::Line::Other(_) => continue,
        }
        notify();
    }

    let status = {
        let mut guard = control
            .child
            .lock()
            .map_err(|_| "Внутренняя ошибка синхронизации".to_string())?;
        match guard.as_mut() {
            Some(child) => child.wait().map_err(|e| format!("Сбой ожидания: {e}"))?,
            None => return Err("Процесс потерян".into()),
        }
    };
    let _ = stderr_thread.join();
    control.release();

    if status.success() {
        match final_path {
            Some(path) => {
                // Вторая стадия — своя обрезка. До неё дело доходит только по
                // плану `CutAfter`: ролик уже лежит целиком, осталось вырезать
                // кусок и подменить им файл.
                if plan == SectionPlan::CutAfter
                    && let Some(ffmpeg) = &tools.ffmpeg
                {
                    let _ = tx.send(Event::Stage("Вырезаю фрагмент…".into()));
                    notify();

                    match cut::cut(ffmpeg, &path, request.section, control, tx) {
                        cut::Outcome::Done => {}
                        // Молча, без `Failed`: «Отменено» UI показывает сам
                        // (Правило 2), а на диске после отмены не осталось
                        // ни фрагмента, ни ролика.
                        cut::Outcome::Cancelled => return Ok(()),
                        // Ролик скачан и лежит на диске — отдать его вместе
                        // с объяснением честнее, чем показать «Ошибку» поверх
                        // готового файла. Ровно та беда, из-за которой ключи
                        // вшивания живут под проверкой на ffmpeg.
                        // Про повторную попытку сказать надо здесь же:
                        // ролик лежит под тем самым именем, под каким его
                        // сохранил бы удавшийся фрагмент, а yt-dlp по
                        // умолчанию готовый файл не перезаписывает. То есть
                        // второе нажатие «Скачать» кончится не новой
                        // попыткой, а «Готово (файл уже существовал)» —
                        // и человек решит, что обрезка сломана насовсем.
                        cut::Outcome::Failed(why) => {
                            let _ = tx.send(Event::Warning(format!(
                                "Вырезать фрагмент не вышло — ролик сохранён целиком.\n\n{why}\n\n\
                                 Чтобы попробовать ещё раз, уберите или переименуйте сохранённый \
                                 файл: пока он лежит на месте, загрузка считается сделанной и \
                                 повторяться не будет."
                            )));
                            notify();
                        }
                    }
                }

                let _ = tx.send(Event::Done { id, path });
                notify();
                Ok(())
            }
            // Успех без пути означает, что файл уже был на диске
            // и yt-dlp пропустил стадию after_move.
            None => {
                let _ = tx.send(Event::Stage("Готово (файл уже существовал)".into()));
                notify();
                Ok(())
            }
        }
    } else {
        let tail = errors
            .lock()
            .map(|g| {
                g.iter()
                    .rev()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        Err(ytdlp::explain_failure(
            status.code().unwrap_or(-1),
            &tail,
            request.cookies,
        ))
    }
}

/// Что делаем с метаданными выбранного файла.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetaTask {
    Read,
    Clean,
}

/// Запускает работу с метаданными локального файла в отдельном потоке.
///
/// Ручки, как у загрузки, здесь нет намеренно: обе операции — это чтение
/// заголовков и копирование файла, они укладываются в доли секунды даже на
/// сотне мегабайт. Отменять нечего, а кнопка «Отмена», которая не успевает
/// нажаться, только запутывала бы.
///
/// Отдельный канал `Event` на вызов, а не общий с загрузкой: пользователь
/// вправе чистить метаданные, пока идёт скачивание, и события двух задач
/// не должны попадать в один приёмник.
pub fn start_metadata(
    path: PathBuf,
    task: MetaTask,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let result = match task {
            MetaTask::Read => {
                let _ = tx.send(Event::Stage("Читаю метаданные…".into()));
                notify();
                // ffprobe нужен только для MP3 и только ради битрейта с
                // длительностью. Его отсутствие — не повод отказать в работе:
                // изображения разбираются без единой внешней программы.
                let ffprobe = binaries::locate(binaries::FFPROBE_NAME);
                metadata::read(&path, ffprobe.as_deref()).map(Event::Tags)
            }
            MetaTask::Clean => {
                let _ = tx.send(Event::Stage("Удаляю метаданные…".into()));
                notify();
                metadata::strip(&path).map(Event::Cleaned)
            }
        };

        // Номер здесь `NO_DOWNLOAD`: к очереди метаданные отношения не имеют,
        // да и приёмник у них свой. Раскрывать это замыканием, а не коротким
        // `unwrap_or_else(Event::Failed)`, приходится потому, что вариант
        // с именованными полями конструктором-функцией не бывает.
        let _ = tx.send(result.unwrap_or_else(|message| Event::Failed {
            id: NO_DOWNLOAD,
            message,
        }));
        notify();
    });
}

/// Спрашивает метаданные ссылки, ни к какой загрузке не привязываясь.
///
/// Нужен предпросмотру: название и обложку человек должен увидеть, как только
/// вставил ссылку, — то есть до нажатия «Скачать», а не после. Ответ уходит
/// теми же `Info` и `Thumbnail`, что и у загрузки, так что разбирать его UI
/// умеет с первого дня.
///
/// Отдельный канал `Event` на вызов, а не общий с загрузкой, — по той же
/// причине, что и у метаданных файла: спрашивать про новую ссылку можно и
/// во время скачивания, и события двух задач не должны попадать в один сток.
///
/// **Ни одна неудача здесь ничем не оборачивается**: ни `Failed`, ни строки
/// в журнале. Предпросмотр — украшение, и отсутствие yt-dlp, незнакомый сайт
/// или молчащий сервер значат ровно одно: карточка останется пустой, а
/// «Скачать» по-прежнему можно нажать — вдруг сайт ответит ему.
pub fn start_probe(
    request: Request,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) -> Handle {
    let control = Control::default();
    let handle = Handle {
        control: control.clone(),
    };

    std::thread::spawn(move || {
        let Ok(tools) = discover() else {
            return;
        };
        let Some(info) = probe(&request, &tools, &control) else {
            return;
        };

        let cover = info.thumbnail_url.clone();
        // Закрытый приёмник означает, что спрашивают уже про другую ссылку:
        // на смену цели UI роняет канал. Тянуть после этого ещё и картинку —
        // несколько секунд чужого времени впустую, а показать её всё равно
        // будет некому.
        if tx.send(Event::Info(info)).is_err() {
            return;
        }
        notify();

        if control.cancelled() {
            return;
        }
        send_cover(cover, &tx, &notify);
    });

    handle
}

/// Тянет обложку по адресу из ответа `probe` и отправляет её в канал.
///
/// Общее у загрузки и у предпросмотра — картинка и там и там одна и та же.
/// Любая неудача здесь — строка в журнале, и только. Ни `Failed`, ни даже
/// `Warning`: превью — украшение, а баннер во весь экран из-за мёртвой ссылки
/// на картинку выглядел бы поломкой загрузки, которой не произошло.
fn send_cover(url: Option<String>, tx: &Sender<Event>, notify: &impl Fn()) {
    let Some(url) = url else {
        return;
    };
    match thumbnail::fetch(&url) {
        Ok(cover) => {
            let _ = tx.send(Event::Thumbnail(cover));
            notify();
        }
        Err(err) => {
            let _ = tx.send(Event::Log(format!("Обложка не загрузилась: {err}")));
        }
    }
}

/// Быстрый запрос метаданных. Ошибки глушим: это украшение, а не необходимость.
///
/// Разбор ответа (в том числе списка доступных высот) идёт здесь, на потоке
/// движка, а не в UI: `-J` у длинного плейлиста весит мегабайты, и разбирать
/// его в кадре отрисовки нельзя.
///
/// Берём весь `Request`, а не одну ссылку: спрашивать надо тем же, чем потом
/// качаем, — вместе с cookies. Иначе у закрытого ролика этот вызов молча
/// провалится, и человек будет смотреть на пустую карточку у загрузки,
/// которая на самом деле идёт.
///
/// Процесс запускаем и отдаём в слот, а не зовём короткий `Command::output()`:
/// `-J` у медленного сайта идёт секундами, и всё это время бросить его было
/// бы нечем. Для загрузки это то самое окно, в которое «Отмена» не отменяла
/// ничего (дефект 14), а для предпросмотра — накопившиеся фоновые процессы
/// по ссылкам, которых в поле давно нет.
fn probe(request: &Request, tools: &Tools, control: &Control) -> Option<MediaInfo> {
    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(ytdlp::probe_args(
        &request.url,
        request.cookies,
        request.cookie_file.as_deref(),
    ))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    ytdlp::hide_console(&mut cmd);
    control.arm(&mut cmd);

    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;

    // Бросили, пока запускались, — `adopt` его уже убил, и ждать ответа
    // не от кого.
    if !control.adopt(child).ok()? {
        return None;
    }

    // Читаем байтами и разбираем `from_utf8_lossy`, а не `read_to_string`:
    // ответ приходит от чужой программы, и один битый байт в названии ролика
    // не повод остаться без карточки целиком.
    let mut json = Vec::new();
    let read = BufReader::new(stdout).read_to_end(&mut json);
    let status = wait_and_release(control)?;

    (read.is_ok() && status.success())
        .then(|| ytdlp::parse_media_info(&String::from_utf8_lossy(&json)))
}

/// Дожидается процесса из слота и освобождает слот вместе с группой.
///
/// Ждать обязательно (иначе зомби на Unix), освобождать — тоже: после `probe`
/// в тот же слот ложится сам yt-dlp, и оставленный там отработавший процесс
/// сбивал бы с толку «Отмену».
fn wait_and_release(control: &Control) -> Option<ExitStatus> {
    let status = {
        let mut guard = control.child.lock().ok()?;
        guard.as_mut()?.wait().ok()?
    };
    control.release();
    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CookieSource, DownloadOptions, Format, Quality, Section};
    use std::sync::mpsc::channel;

    /// Заведомо несуществующий бинарник: `probe` на нём отваливается мгновенно
    /// (никакой сети и никакого процесса), а попытка запустить загрузку сразу
    /// видна по ошибке. На этом и держатся оба теста ниже: до `spawn` дошли —
    /// значит `Err`, не дошли — значит `Ok`.
    fn tools() -> Tools {
        Tools {
            ytdlp: PathBuf::from("savio-заведомо-нет-такого-бинарника"),
            ffmpeg: None,
            ffprobe: None,
        }
    }

    /// Загрузка, которую `run` получает на вход. Ссылка и папка живут в
    /// статике, чтобы не разбираться со временем жизни ссылок в каждом тесте.
    fn job(known: Option<MediaInfo>) -> Job<'static> {
        static REQUEST: std::sync::OnceLock<Request> = std::sync::OnceLock::new();
        static OUT_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        Job {
            id: 1,
            request: REQUEST.get_or_init(request),
            known,
            out_dir: OUT_DIR.get_or_init(std::env::temp_dir),
        }
    }

    fn request() -> Request {
        Request {
            url: "https://example.invalid/watch?v=savio".into(),
            format: Format::Mp4,
            quality: Quality::default(),
            options: DownloadOptions::default(),
            section: Section::default(),
            cookies: CookieSource::None,
            cookie_file: None,
            sub_lang: crate::model::SubLang::default(),
        }
    }

    /// Отмена, нажатая до запуска процесса, обязана остановить загрузку —
    /// и остановить молча.
    ///
    /// Саму гонку тест поймать не может (она на то и гонка), но ловит её
    /// причину: до появления флага `run` в этом месте не спрашивал ничего
    /// и уходил запускать yt-dlp. Пара с тестом ниже: поодиночке любой из
    /// них прошёл бы и на сломанном флаге.
    #[test]
    fn cancel_before_spawn_stops_download() {
        let control = Control::default();
        control.cancelled.store(true, Ordering::Relaxed);

        let (tx, rx) = channel();
        let result = run(job(None), &tools(), &tx, &|| {}, &control);

        assert!(result.is_ok(), "отмена — не ошибка, а «Отменено»: {result:?}");
        drop(tx);
        assert!(
            !rx.iter().any(|e| matches!(e, Event::Failed { .. })),
            "при отмене UI показывает «Отменено», а не ошибку (Правило 2)"
        );
    }

    /// Обратная половина: без отмены тот же вызов доходит до запуска yt-dlp.
    #[test]
    fn without_cancel_run_reaches_spawn() {
        let control = Control::default();

        // Приёмник держим живым до конца: закрытый канал — второй признак
        // отмены, и на нём `run` вышел бы, не дойдя до запуска.
        let (tx, rx) = channel();
        let result = run(job(None), &tools(), &tx, &|| {}, &control);

        let err = result.expect_err("без отмены дело обязано дойти до yt-dlp");
        assert!(err.contains("yt-dlp"), "неожиданная ошибка: {err}");
        drop(rx);
    }

    /// Готовый ответ вызывающего избавляет от второго похода на сайт.
    ///
    /// Проверяется от противного: бинарника нет, то есть сам `probe` ответить
    /// не мог бы ничем, — а `Info` в канал всё равно уходит. Значит, взят он
    /// из `known`, и `-J` второй раз не запускался. Ни сборка, ни `clippy`
    /// такого не видят: лишний запрос ничего не ломает, он просто есть.
    #[test]
    fn a_known_answer_saves_the_second_trip_to_the_site() {
        let known = MediaInfo {
            title: Some("Название из предпросмотра".into()),
            ..MediaInfo::default()
        };

        let (tx, rx) = channel();
        let _ = run(
            job(Some(known)),
            &tools(),
            &tx,
            &|| {},
            &Control::default(),
        );
        drop(tx);

        let title = rx.iter().find_map(|event| match event {
            Event::Info(info) => info.title,
            _ => None,
        });
        assert_eq!(title.as_deref(), Some("Название из предпросмотра"));
    }

    /// Настоящая загрузка, отменённая в первую секунду: в папке не должно
    /// появиться ни файла, ни огрызка `.part`.
    ///
    /// Ровно тот сценарий, которым дефект 14 и нашли: окно показывало
    /// «Отменено», а файл продолжал качаться. Отмена приходит через 300 мс,
    /// то есть в середину `probe`, — в это окно процесса ещё нет, и убивать
    /// «Отмене» нечего. Ни сборка, ни `clippy`, ни обычные тесты этого не
    /// видят, проверяется только живьём.
    ///
    /// Приёмник тест держит живым намеренно, хотя UI его роняет вместе с
    /// отменой: закрытый канал — второй, независимый признак остановки, и на
    /// нём тест прошёл бы даже с выломанным флагом.
    ///
    /// В обычном прогоне отключён: нужны сеть и yt-dlp.
    /// Запуск вручную: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "требует доступа в сеть и установленного yt-dlp"]
    fn real_cancel_in_the_first_second_leaves_no_file() {
        let dir = std::env::temp_dir().join("savio-cancel-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("создать временный каталог");

        let mut request = request();
        request.url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into();

        let (tx, rx) = channel();
        let handle =
            start(1, request, dir.clone(), None, tx, || {}).expect("yt-dlp обязан быть найден");

        std::thread::sleep(std::time::Duration::from_millis(300));
        handle.cancel();

        // Ждём дольше, чем длится probe и первые секунды загрузки: огрызок
        // `.part` появляется почти сразу, и если он не появился за полминуты,
        // значит yt-dlp не запускался вовсе.
        std::thread::sleep(std::time::Duration::from_secs(30));

        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("читать временный каталог")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left.is_empty(),
            "после отмены в папке появились файлы: {left:?}"
        );
        assert!(
            !rx.try_iter().any(|e| matches!(e, Event::Failed { .. })),
            "отмена показывается «Отменено», а не ошибкой (Правило 2)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Живая проверка дефекта 25: после «Отмены» дерево процессов обязано
    /// умереть целиком, а не только тот процесс, который запустили мы.
    ///
    /// Признак живого дерева тут — **закрытие канала**, и он выбран не от
    /// бедности. Поток движка до конца сидит в `BufReader::lines()`, а труба
    /// закрывается, лишь когда её отпустят все — и обёртка PyInstaller, и
    /// рабочий yt-dlp, и ffmpeg. Пережил кто-то из них отмену — поток жив,
    /// `Sender` жив, канал открыт. Так что «канал закрылся» и значит «дерева
    /// больше нет», причём проверяется это ровно тем механизмом, на котором
    /// дефект и держался.
    ///
    /// Мерить размером файла нельзя: пока `.part` открыт, каталожная запись
    /// Windows показывает 0 байт. Считать процессы `Get-Process yt-dlp` тоже
    /// нельзя — их два даже у исправной загрузки (обёртка и рабочий).
    ///
    /// **Срок после «Отмены» короткий, и это главное в тесте.** Убитое дерево
    /// отпускает трубу мгновенно, а живое держит её до конца загрузки — вот
    /// его-то и надо сделать заведомо долгим. Первая попытка брала фрагмент
    /// 5–15 секунд, и он успевал скачаться раньше срока: тест проходил
    /// с выломанной починкой, то есть не проверял ничего. Отсюда и качество
    /// по умолчанию (`Best` — у этого ролика 4K), и фрагмент в полторы минуты.
    ///
    /// Замер 2026-08-22 (yt-dlp 2026.07.04, Windows 11): с починкой канал
    /// закрывается за 4–6 миллисекунд во всех трёх случаях. С выломанной —
    /// «целиком» и «длинный фрагмент» не закрываются и за три секунды, а вот
    /// «короткий фрагмент» закрылся за 163 мс, то есть **сегодня он дефект
    /// уже не ловит**: тот самый случай, с которого дефект начинали, за месяц
    /// у yt-dlp изменился. Оставлен всё равно — он бесплатен, а измениться
    /// может и обратно (Правило 6 про стареющие замеры).
    ///
    /// В обычном прогоне отключены: нужны сеть, yt-dlp и ffmpeg.
    /// Запуск вручную: `cargo test -- --ignored --nocapture`.
    fn live_cancel_kills_the_tree(section: Section, what: &str) {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("savio-tree-{what}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("создать временный каталог");

        let mut request = request();
        request.url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into();
        request.section = section;

        let (tx, rx) = channel();
        let handle =
            start(1, request, dir.clone(), None, tx, || {}).expect("yt-dlp обязан быть найден");

        // Ждём настоящей загрузки: пока не пришёл первый `Progress`, дерева
        // ещё нет и убивать нечего — тест прошёл бы, ничего не проверив.
        let mut running = false;
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Event::Progress(_)) => {
                    running = true;
                    break;
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(running, "{what}: загрузка так и не пошла — проверять нечего");

        handle.cancel();
        let cancelled_at = Instant::now();

        // Три секунды — это «мгновенно» с большим запасом на медленную машину
        // и всё ещё несоизмеримо меньше, чем осталось качать 4K-ролику.
        const GRACE: Duration = Duration::from_secs(3);
        let mut closed_in = None;
        while cancelled_at.elapsed() < GRACE {
            match rx.recv_timeout(GRACE) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    closed_in = Some(cancelled_at.elapsed());
                    break;
                }
            }
        }

        // Число печатаем всегда: замер чужой программы стареет быстрее кода
        // (Правило 6), и следующему пригодится не «прошло», а «за сколько».
        println!("{what}: канал закрылся через {closed_in:?}");
        assert!(
            closed_in.is_some(),
            "{what}: спустя {GRACE:?} после «Отмены» кто-то из дерева всё ещё \
             держит трубу — то есть качает"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Загрузка без фрагмента: дерево мельче, но `Progress` после «Отмены»
    /// шли ещё восемнадцать секунд (замер 2026-07-31).
    #[test]
    #[ignore = "требует доступа в сеть и установленного yt-dlp"]
    fn real_cancel_stops_a_plain_download() {
        live_cancel_kills_the_tree(Section::default(), "целиком");
    }

    /// Короткий фрагмент: путь `--download-sections`, и качает при нём ffmpeg
    /// внуком. Тот случай, на котором дефект нашли.
    ///
    /// «Короткий» здесь про долю, а не про секунды: полторы минуты из
    /// трёх с половиной — это меньше половины, то есть путь `Stream`.
    /// Короче делать нельзя, иначе фрагмент успеет скачаться (см. `GRACE`).
    #[test]
    #[ignore = "требует доступа в сеть, yt-dlp и ffmpeg"]
    fn real_cancel_stops_a_short_section() {
        live_cancel_kills_the_tree(
            Section {
                start: Some(5),
                end: Some(100),
            },
            "короткий-фрагмент",
        );
    }

    /// Длинный фрагмент: с задачи 26 это другое дерево — ролик идёт обычной
    /// загрузкой, а режет его наш собственный ffmpeg уже после.
    #[test]
    #[ignore = "требует доступа в сеть, yt-dlp и ffmpeg"]
    fn real_cancel_stops_a_long_section() {
        live_cancel_kills_the_tree(
            Section {
                start: Some(0),
                end: Some(150),
            },
            "длинный-фрагмент",
        );
    }

    /// Команда, которая запускает ещё одну и ждёт её конца, — то самое дерево,
    /// на котором «Отмена» и спотыкалась (`yt-dlp` → `yt-dlp` → `ffmpeg`).
    ///
    /// Обе оболочки есть на своих системах всегда, скачивать для теста нечего.
    #[cfg(windows)]
    fn nested_process() -> Command {
        let mut cmd = Command::new("cmd");
        // `ping` по петле: тридцать секунд жизни и вывод в ту же трубу.
        cmd.args(["/c", "ping", "-n", "30", "127.0.0.1"]);
        cmd
    }

    #[cfg(unix)]
    fn nested_process() -> Command {
        let mut cmd = Command::new("/bin/sh");
        // Тут важен каждый знак. У `sh -c 'sleep 30'` оболочка подменяет собой
        // процесс (`exec`), никакого внука не появляется, и проверять было бы
        // нечего. Амперсанд же ставит `sleep` в потомки и, главное, делает это
        // **до** `echo`: первый байт в трубе значит, что внук уже родился.
        // А `wait` не даёт оболочке уйти раньше него.
        cmd.args(["-c", "sleep 30 & echo started; wait"]);
        cmd
    }

    /// «Отмена» обязана убить всё дерево, а не один процесс.
    ///
    /// Ровно дефект 25: `Child::kill()` бьёт по тому, кого запустили мы, —
    /// по обёртке PyInstaller, — а качает её внук, и после «Отменено» в окне
    /// файл спокойно дописывался.
    ///
    /// Проверяем по трубе, а не по списку процессов, и это не обход, а тот же
    /// механизм, которым дефект держался: дескриптор вывода достаётся и внуку,
    /// поэтому конец потока наступает лишь тогда, когда его отпустят **все**.
    /// Пережил внук отмену — `read_to_end` будет ждать его тридцать секунд,
    /// и ждать бы движок, сидящий в `BufReader::lines()`.
    ///
    /// Перечислять процессы средствами системы было бы и дольше, и хуже:
    /// у `Get-Process yt-dlp` два ответа даже на исправной загрузке.
    #[test]
    fn cancel_kills_the_whole_tree() {
        let control = Control::default();
        let handle = Handle {
            control: control.clone(),
        };

        let mut cmd = nested_process();
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        ytdlp::hide_console(&mut cmd);
        control.arm(&mut cmd);

        let mut child = cmd.spawn().expect("оболочка обязана быть в системе");
        let stdout = child.stdout.take().expect("труба заказана piped");
        assert!(
            control.adopt(child).expect("замок не отравлен"),
            "без отмены процесс обязан попасть в слот"
        );

        // Читающий поток говорит о двух событиях: о первых байтах и о конце
        // потока. Первые — признак того, что внук родился; конец — того, что
        // трубу отпустили все до единого.
        let (first_bytes, grew) = channel();
        let (done, closed) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = [0u8; 256];
            while let Ok(read) = reader.read(&mut buf) {
                if read == 0 {
                    break;
                }
                let _ = first_bytes.send(());
            }
            let _ = done.send(());
        });

        // Ждём внука по его же выводу, а не по часам. Со `sleep` тест был бы
        // честен ровно до первой загруженной машины: не успел внук родиться —
        // и «дерево умерло» стало бы неотличимо от «дерева не было».
        grew.recv_timeout(std::time::Duration::from_secs(15))
            .expect("от запущенного дерева не пришло ни байта — проверять нечего");

        handle.cancel();

        let tree_died = closed
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok();

        // Прибираем за собой в любом исходе: провалившийся тест не должен
        // оставлять после себя полминуты чужой работы.
        control.group.kill();
        if let Ok(mut slot) = control.child.lock()
            && let Some(child) = slot.as_mut()
        {
            kill_and_reap(child);
        }

        assert!(
            tree_died,
            "«Отмена» убила только свой процесс: потомок жив и держит трубу \
             открытой — ровно то, из-за чего загрузка шла после «Отменено»"
        );
    }

    /// Отмена уже идущей загрузки: процесс из слота убит, а флаг стоит —
    /// значит запускать что-то ещё после неё движок не станет.
    #[test]
    fn cancel_marks_the_flag() {
        let control = Control::default();
        let handle = Handle {
            control: control.clone(),
        };

        assert!(!control.cancelled());
        handle.cancel();
        assert!(control.cancelled());
    }
}
