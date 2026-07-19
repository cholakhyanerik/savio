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
    if !line.starts_with('{') {
        return Line::Other(if line.is_empty() {
            String::new()
        } else {
            line.to_owned()
        });
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Line::Other(line.to_owned());
    };

    // Префикс в `--progress-template download:{…}` выбирает момент вывода
    // и самим yt-dlp съедается — в поток приходит голый JSON без него.
    // Поэтому шаблоны различаем по набору полей, а не по началу строки.
    if v.get("event").and_then(|x| x.as_str()) == Some("done") {
        if let Some(path) = v.get("path").and_then(|x| x.as_str()) {
            return Line::Done(PathBuf::from(path));
        }
        return Line::Other(line.to_owned());
    }

    // `pp` есть только у шаблона постобработки.
    if v.get("pp").is_some() {
        let pp = v.get("pp").and_then(|x| x.as_str()).unwrap_or("обработка");
        return Line::Stage(format!("Обработка: {pp}"));
    }

    if v.get("downloaded").is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Строки взяты из реального вывода yt-dlp: префикса `download:` в них
    /// нет — он остаётся в аргументах, а не в потоке.
    const REAL_PROGRESS: &str = r#"{"status":"downloading","downloaded":195633173.000000,"total":712445280.000000,"speed":15943362.460976,"eta":32.000000}"#;

    #[test]
    fn progress_is_parsed_without_prefix() {
        let Line::Progress(p) = parse_line(REAL_PROGRESS) else {
            panic!("строка прогресса не распознана");
        };
        assert_eq!(p.downloaded, 195_633_173);
        assert_eq!(p.total, 712_445_280);
        assert_eq!(p.speed_bps, Some(15_943_362.460976));
        assert_eq!(p.eta_secs, Some(32));
        assert_eq!(p.fraction(), Some(195_633_173.0 / 712_445_280.0));
    }

    #[test]
    fn zero_speed_and_eta_become_none() {
        let line = r#"{"status":"downloading","downloaded":10.0,"total":0.0,"speed":0.0,"eta":0.0}"#;
        let Line::Progress(p) = parse_line(line) else {
            panic!("строка прогресса не распознана");
        };
        assert_eq!(p.total, 0);
        assert_eq!(p.speed_bps, None);
        assert_eq!(p.eta_secs, None);
        // Общий размер неизвестен — UI покажет неопределённый индикатор.
        assert_eq!(p.fraction(), None);
    }

    #[test]
    fn postprocess_becomes_stage() {
        let line = r#"{"status":"processing","pp":"Merger"}"#;
        let Line::Stage(stage) = parse_line(line) else {
            panic!("постобработка не распознана");
        };
        assert_eq!(stage, "Обработка: Merger");
    }

    #[test]
    fn done_carries_path() {
        let line = r#"{"event":"done","path":"C:\\Users\\me\\video.mp4"}"#;
        let Line::Done(path) = parse_line(line) else {
            panic!("завершение не распознано");
        };
        assert_eq!(path, PathBuf::from(r"C:\Users\me\video.mp4"));
    }

    #[test]
    fn junk_goes_to_log() {
        assert!(matches!(parse_line("[youtube] Extracting URL"), Line::Other(s) if !s.is_empty()));
        assert!(matches!(parse_line("   "), Line::Other(s) if s.is_empty()));
        // Оборванный JSON не должен ронять разбор.
        assert!(matches!(parse_line(r#"{"status":"#), Line::Other(_)));
    }

    #[test]
    fn media_info_survives_missing_fields() {
        let info = parse_media_info(r#"{"title":"Ролик","uploader":"Автор","duration":75.0}"#);
        assert_eq!(info.title.as_deref(), Some("Ролик"));
        assert_eq!(info.uploader.as_deref(), Some("Автор"));
        assert_eq!(info.duration_secs, Some(75.0));

        // Метаданные — украшение: их отсутствие не должно ничего ломать.
        let empty = parse_media_info("{}");
        assert_eq!(empty.title, None);
        assert_eq!(empty.duration_secs, None);

        let broken = parse_media_info("не json");
        assert_eq!(broken.title, None);
    }
}
