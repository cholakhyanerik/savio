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

/// Похожа ли строка на ссылку, которую есть смысл отдавать yt-dlp.
///
/// Проверка намеренно грубая: её задача — поймать очевидную опечатку
/// (забытый протокол, случайный текст из буфера обмена), а не отфильтровать
/// всё неверное. Список поддерживаемых сайтов знает yt-dlp, а не Savio,
/// поэтому UI только подсвечивает поле и **не** блокирует кнопку: решение
/// всегда остаётся за пользователем.
pub fn looks_like_url(text: &str) -> bool {
    let text = text.trim();
    let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    else {
        return false;
    };

    // Хост — всё до первого разделителя пути. Он обязан существовать и
    // содержать точку: «https://» и «https://youtube» ссылками ещё не являются.
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();

    host.contains('.') && !host.starts_with('.') && !host.ends_with('.') && !host.contains(' ')
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_accepts_real_links() {
        assert!(looks_like_url("https://youtube.com/watch?v=abc"));
        assert!(looks_like_url("http://vk.com/video-1_2"));
        assert!(looks_like_url("https://www.kinobase.org/film/123"));
        // Пробелы по краям — обычное дело при вставке из буфера.
        assert!(looks_like_url("  https://youtu.be/abc  "));
    }

    #[test]
    fn url_rejects_obvious_junk() {
        assert!(!looks_like_url(""));
        assert!(!looks_like_url("youtube.com"), "нет протокола");
        assert!(!looks_like_url("https://"), "нет хоста");
        assert!(!looks_like_url("https://youtube"), "хост без точки");
        assert!(!looks_like_url("ftp://example.com"), "не http(s)");
        assert!(!looks_like_url("просто текст"));
        assert!(!looks_like_url("https://.com"), "хост начинается с точки");
        assert!(!looks_like_url("https://example."), "хост кончается точкой");
    }

    #[test]
    fn bytes_switch_units() {
        assert_eq!(human_bytes(0), "0 Б");
        assert_eq!(human_bytes(512), "512 Б");
        assert_eq!(human_bytes(1024), "1.0 КБ");
        assert_eq!(human_bytes(1536), "1.5 КБ");
        assert_eq!(human_bytes(1024 * 1024), "1.0 МБ");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 ГБ");
        // Больше гигабайта единица не растёт — дальше просто копятся ГБ.
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 ГБ");
    }

    #[test]
    fn duration_hides_zero_hours() {
        assert_eq!(human_duration(0), "0:00");
        assert_eq!(human_duration(9), "0:09");
        assert_eq!(human_duration(75), "1:15");
        assert_eq!(human_duration(3600), "1:00:00");
        assert_eq!(human_duration(3671), "1:01:11");
    }

    #[test]
    fn fraction_is_none_without_total() {
        // Потоковые источники присылают total = 0 — процент показать нечем.
        let p = Progress {
            downloaded: 100,
            total: 0,
            ..Progress::default()
        };
        assert_eq!(p.fraction(), None);
    }

    #[test]
    fn fraction_clamps_to_one() {
        let p = Progress {
            downloaded: 150,
            total: 100,
            ..Progress::default()
        };
        assert_eq!(p.fraction(), Some(1.0));

        let half = Progress {
            downloaded: 50,
            total: 100,
            ..Progress::default()
        };
        assert_eq!(half.fraction(), Some(0.5));
    }
}
