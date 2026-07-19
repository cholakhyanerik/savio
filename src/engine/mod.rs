//! Движок загрузки.
//!
//! Про UI не знает ничего: наружу торчит канал `Event` и колбэк-будильник.
//! Благодаря этому движок можно прицепить к CLI или к тестам, не трогая код.

pub mod binaries;
pub mod setup;
pub mod sha256;
pub mod ytdlp;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::model::{Event, Format};

pub use binaries::{Tools, discover};

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
    url: String,
    format: Format,
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
    }

    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let handle = Handle {
        child: Arc::clone(&child_slot),
    };

    std::thread::spawn(move || {
        let result = run(&url, format, &out_dir, &tools, &tx, &notify, &child_slot);
        if let Err(err) = result {
            let _ = tx.send(Event::Failed(err));
            notify();
        }
    });

    Ok(handle)
}

fn run(
    url: &str,
    format: Format,
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
    if let Some(info) = probe(url, tools) {
        let _ = tx.send(Event::Info(info));
        notify();
    }

    let args = ytdlp::download_args(url, format, out_dir, tools);
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

        Err(ytdlp::explain_failure(status.code().unwrap_or(-1), &tail))
    }
}

/// Быстрый запрос метаданных. Ошибки глушим: это украшение, а не необходимость.
fn probe(url: &str, tools: &Tools) -> Option<crate::model::MediaInfo> {
    let mut cmd = Command::new(&tools.ytdlp);
    cmd.args(ytdlp::probe_args(url))
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
