//! Движок загрузки.
//!
//! Про UI не знает ничего: наружу торчит канал `Event` и колбэк-будильник.
//! Благодаря этому движок можно прицепить к CLI или к тестам, не трогая код.

pub mod binaries;
pub mod metadata;
pub mod settings;
pub mod setup;
pub mod sha256;
pub mod thumbnail;
pub mod ytdlp;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::model::{DownloadId, Event, NO_DOWNLOAD, Request, human_duration};

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
/// Половины две, и обе обязательны. Убить процесс можно, только когда он есть,
/// а первые секунды после «Скачать» его нет: идёт `probe`, потом обложка, потом
/// сам запуск. Всё это время слот пуст, и одного слота хватало ровно на то,
/// чтобы отмена не отменяла ничего: окно закрывалось, а файл спокойно
/// докачивался. Флаг отвечает на другой вопрос — «просили ли отменить», —
/// и потому виден движку до появления процесса. Приём тот же, что у установки
/// инструментов (`setup::Handle`), там он проверен.
#[derive(Clone, Default)]
struct Control {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl Control {
    /// Просили ли отменить.
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
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
                kill_and_reap(&mut child);
                return Err("Внутренняя ошибка синхронизации".into());
            }
        }
        // Замок здесь уже отпущен: `wait()` под ним заставил бы `cancel()`
        // ждать нас, а её задача — вернуть управление сразу.
        kill_and_reap(&mut child);
        Ok(false)
    }
}

/// Убивает процесс и дожидается его конца.
///
/// Ждать обязательно: `Child::drop` этого не делает, и на Unix убитый, но
/// не дождавшийся потомок остаётся зомби до конца жизни Savio.
fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Ручка запущенной загрузки: позволяет её отменить.
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
pub fn start(
    id: DownloadId,
    request: Request,
    out_dir: PathBuf,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) -> Result<Handle, String> {
    let tools = discover()?;
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
        // Предупреждение одно на оба случая, хотя случая два. Причина в UI:
        // `Event::Warning` живёт там в единственном поле, и второе сообщение
        // молча затёрло бы первое — то есть про потерянную обрезку человек
        // не узнал бы вовсе.
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
            let _ = tx.send(Event::Warning(format!(
                "{warning} Перезапустите Savio: при старте он сам попробует \
                 скачать ffmpeg ещё раз."
            )));
        }
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
        let result = run(id, &request, &out_dir, &tools, &tx, &notify, &control);
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

fn run(
    id: DownloadId,
    request: &Request,
    out_dir: &Path,
    tools: &Tools,
    tx: &Sender<Event>,
    notify: &(impl Fn() + Send + 'static),
    control: &Control,
) -> Result<(), String> {
    let _ = tx.send(Event::Stage("Читаю ссылку…".into()));
    notify();

    // Метаданные тянем отдельным быстрым вызовом, чтобы показать название
    // ещё до старта загрузки. Если не вышло — не страшно, идём дальше.
    if let Some(info) = probe(request, tools) {
        // Адрес и длительность забираем до отправки: `Info` уходит в UI
        // вместе со структурой.
        let cover = info.thumbnail_url.clone();
        let duration = info.duration_secs;
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
        //
        // Любая неудача здесь — строка в журнале, и только. Ни `Failed`, ни
        // даже `Warning`: превью — украшение, а баннер во весь экран из-за
        // мёртвой ссылки на картинку выглядел бы поломкой загрузки, которой
        // не произошло.
        if listening && let Some(url) = cover {
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

    let args = ytdlp::download_args(request, out_dir, tools);

    // Второй признак того, что UI про нас забыл, — закрытый приёмник, и он
    // не заменяет флаг, а дополняет: флаг отвечает «просили ли отменить»,
    // приёмник — «слушает ли кто-нибудь». Совпадают они не всегда: канал
    // роняет и закрытие окна. Уйти по этому признаку так же важно: для
    // очереди запуск после забвения опаснее, чем для одиночной загрузки —
    // следующий её элемент уже пошёл, и мы получили бы два yt-dlp разом,
    // вопреки правилу «строго по одному». Своего события не нужно —
    // достаточно посмотреть на исход отправки той строки, что и так уходит
    // в журнал.
    if tx
        .send(Event::Log(format!("yt-dlp {}", args.join(" "))))
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

    if let Ok(mut guard) = control.child.lock() {
        *guard = None;
    }

    if status.success() {
        match final_path {
            Some(path) => {
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
fn probe(request: &Request, tools: &Tools) -> Option<crate::model::MediaInfo> {
    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(ytdlp::probe_args(&request.url, request.cookies))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    ytdlp::hide_console(&mut cmd);

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let json = String::from_utf8_lossy(&output.stdout);
    Some(ytdlp::parse_media_info(&json))
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

    fn request() -> Request {
        Request {
            url: "https://example.invalid/watch?v=savio".into(),
            format: Format::Mp4,
            quality: Quality::default(),
            options: DownloadOptions::default(),
            section: Section::default(),
            cookies: CookieSource::None,
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
        let result = run(
            1,
            &request(),
            &std::env::temp_dir(),
            &tools(),
            &tx,
            &|| {},
            &control,
        );

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
        let result = run(
            1,
            &request(),
            &std::env::temp_dir(),
            &tools(),
            &tx,
            &|| {},
            &control,
        );

        let err = result.expect_err("без отмены дело обязано дойти до yt-dlp");
        assert!(err.contains("yt-dlp"), "неожиданная ошибка: {err}");
        drop(rx);
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
        let handle = start(1, request, dir.clone(), tx, || {}).expect("yt-dlp обязан быть найден");

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
