//! Доменные типы. Ничего не знают ни про UI, ни про yt-dlp.

use std::path::PathBuf;

/// Что именно скачиваем.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Видео со звуком, максимум в пределах MP4-контейнера.
    Mp4,
    /// Только аудиодорожка, перекодированная в MP3.
    Mp3,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Mp4 => "MP4 — видео",
            Format::Mp3 => "MP3 — аудио",
        }
    }
}

/// Метаданные ролика, полученные до начала загрузки.
#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub title: Option<String>,
    pub uploader: Option<String>,
    pub duration_secs: Option<f64>,
}

/// Состояние текущей загрузки.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub speed_bps: Option<f64>,
    pub eta_secs: Option<u64>,
}

impl Progress {
    /// Доля выполнения, если общий размер известен.
    ///
    /// Для потоковых источников `total` приходит нулём — в этом случае
    /// показывать нечего, и UI рисует неопределённый индикатор.
    pub fn fraction(&self) -> Option<f32> {
        if self.total > 0 {
            Some((self.downloaded as f32 / self.total as f32).clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

/// События, которые движок отдаёт наружу. Единственный канал связи с UI.
#[derive(Clone, Debug)]
pub enum Event {
    /// Метаданные подъехали.
    Info(MediaInfo),
    /// Сменилась фаза работы — человекочитаемая строка для статус-бара.
    Stage(String),
    Progress(Progress),
    /// Строка диагностики (stderr yt-dlp, аргументы запуска и т.п.).
    Log(String),
    /// Готово, файл лежит здесь.
    Done(PathBuf),
    Failed(String),
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["Б", "КБ", "МБ", "ГБ"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
