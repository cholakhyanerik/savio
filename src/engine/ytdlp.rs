//! Сборка аргументов yt-dlp и разбор его вывода.
//!
//! Прогресс читается не из человекочитаемого вывода, а из машинного:
//! `--progress-template` заставляет yt-dlp печатать готовый JSON, который
//! остаётся только распарсить. Скрейпинг обычного вывода ломается при
//! каждом изменении форматирования, поэтому так делать не стоит.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::binaries::Tools;
use crate::model::{Format, MediaInfo, Progress};

/// Каждое поле, способное прийти пустым, обязано иметь `|default`.
/// Иначе yt-dlp подставит голое `NA` без кавычек и сломает JSON —
/// конверсия `j` к null-полю не применяется.
const DOWNLOAD_TEMPLATE: &str = concat!(
    r#"download:{"status":%(progress.status)j,"#,
    r#""downloaded":%(progress.downloaded_bytes|0)f,"#,
    r#""total":%(progress.total_bytes,progress.total_bytes_estimate|0)f,"#,
    r#""speed":%(progress.speed|0)f,"#,
    r#""eta":%(progress.eta|0)f}"#,
);

const POSTPROCESS_TEMPLATE: &str = concat!(
    r#"postprocess:{"status":%(progress.status)j,"#,
    r#""pp":%(progress.postprocessor)j}"#,
);

/// `after_move` — единственная стадия, на которой путь уже окончательный.
/// На этой стадии `--print` не включает `--simulate`, поэтому загрузка идёт как обычно.
const DONE_TEMPLATE: &str = r#"after_move:{"event":"done","path":%(filepath)j}"#;

pub fn probe_args(url: &str) -> Vec<String> {
    vec![
        "-J".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        url.into(),
    ]
}

pub fn download_args(url: &str, format: Format, out_dir: &Path, tools: &Tools) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--newline".into(),
        // --quiet глушит обычный вывод, --progress возвращает обратно
        // только прогресс. Вместе они дают чистый машинный поток.
        "--quiet".into(),
        "--progress".into(),
        "--progress-template".into(),
        DOWNLOAD_TEMPLATE.into(),
        "--progress-template".into(),
        POSTPROCESS_TEMPLATE.into(),
        "--print".into(),
        DONE_TEMPLATE.into(),
        "--no-playlist".into(),
        "--windows-filenames".into(),
        "-P".into(),
        out_dir.to_string_lossy().into_owned(),
        "-o".into(),
        "%(title)s.%(ext)s".into(),
    ];

    match format {
        // Сначала пробуем чистый MP4/M4A (склейка без перекодирования),
        // затем любые лучшие дорожки, затем готовый совмещённый файл.
        Format::Mp4 => args.extend([
            "-f".into(),
            "bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b".into(),
            "--merge-output-format".into(),
            "mp4".into(),
        ]),
        Format::Mp3 => args.extend([
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--audio-quality".into(),
            "0".into(),
        ]),
    }

    if let Some(ffmpeg) = &tools.ffmpeg {
        args.push("--ffmpeg-location".into());
        args.push(ffmpeg.to_string_lossy().into_owned());
    }

    args.push(url.into());
    args
}

/// На Windows GUI-приложение не должно моргать консолью при каждом запуске.
#[cfg(windows)]
pub fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_console(_cmd: &mut Command) {}

/// Разобранная строка вывода yt-dlp.
pub enum Line {
    Progress(Progress),
    Stage(String),
    Done(PathBuf),
    /// Не наш формат — отдаём в лог как есть.
    Other(String),
}

pub fn parse_line(line: &str) -> Line {
    let line = line.trim();
    if line.is_empty() {
        return Line::Other(String::new());
    }

    if let Some(payload) = line.strip_prefix("download:")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            let num = |key: &str| v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0);
            let speed = num("speed");
            let eta = num("eta");
            return Line::Progress(Progress {
                downloaded: num("downloaded") as u64,
                total: num("total") as u64,
                speed_bps: (speed > 0.0).then_some(speed),
                eta_secs: (eta > 0.0).then_some(eta as u64),
            });
        }

    if let Some(payload) = line.strip_prefix("postprocess:")
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            let pp = v.get("pp").and_then(|x| x.as_str()).unwrap_or("обработка");
            return Line::Stage(format!("Обработка: {pp}"));
        }

    if line.starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && v.get("event").and_then(|x| x.as_str()) == Some("done")
                && let Some(path) = v.get("path").and_then(|x| x.as_str()) {
                    return Line::Done(PathBuf::from(path));
                }

    Line::Other(line.to_owned())
}

pub fn parse_media_info(json: &str) -> MediaInfo {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return MediaInfo::default();
    };
    let text = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.to_owned())
    };
    MediaInfo {
        title: text("title"),
        uploader: text("uploader"),
        duration_secs: v.get("duration").and_then(|x| x.as_f64()),
    }
}
