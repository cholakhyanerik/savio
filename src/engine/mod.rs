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
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::model::{Event, Request, human_duration};

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

/// Ручка запущенной загрузки: позволяет её отменить.
pub struct Handle {
    child: Arc<Mutex<Option<Child>>>,
}

impl Handle {
    /// Убивает процесс. Событие `Failed` при этом не шлётся —
    /// отмену UI показывает сам, чтобы не выглядела как ошибка.
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.child.lock()
            && let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
    }
}

/// Запускает загрузку в отдельном потоке.
///
/// `notify` вызывается после каждого события — UI на нём делает repaint.
pub fn start(
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

    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let handle = Handle {
        child: Arc::clone(&child_slot),
    };

    std::thread::spawn(move || {
        let result = run(&request, &out_dir, &tools, &tx, &notify, &child_slot);
        if let Err(err) = result {
            let _ = tx.send(Event::Failed(err));
            notify();
        }
    });

    Ok(handle)
}

fn run(
    request: &Request,
    out_dir: &Path,
    tools: &Tools,
    tx: &Sender<Event>,
    notify: &(impl Fn() + Send + 'static),
    child_slot: &Arc<Mutex<Option<Child>>>,
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

    let args = ytdlp::download_args(request, out_dir, tools);
    let _ = tx.send(Event::Log(format!("yt-dlp {}", args.join(" "))));

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

    if let Ok(mut guard) = child_slot.lock() {
        *guard = Some(child);
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
            ytdlp::Line::Progress(p) => {
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
        let mut guard = child_slot
            .lock()
            .map_err(|_| "Внутренняя ошибка синхронизации".to_string())?;
        match guard.as_mut() {
            Some(child) => child.wait().map_err(|e| format!("Сбой ожидания: {e}"))?,
            None => return Err("Процесс потерян".into()),
        }
    };
    let _ = stderr_thread.join();

    if let Ok(mut guard) = child_slot.lock() {
        *guard = None;
    }

    if status.success() {
        match final_path {
            Some(path) => {
                let _ = tx.send(Event::Done(path));
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

        let _ = tx.send(result.unwrap_or_else(Event::Failed));
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
