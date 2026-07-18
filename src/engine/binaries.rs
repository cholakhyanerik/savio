//! Поиск внешних бинарников.
//!
//! Порядок: рядом с нашим exe (портативная поставка) → PATH (системная установка).
//! Скачивание при первом запуске сюда же ляжет третьим шагом, когда решим
//! вопрос с лицензией на распространение ffmpeg.

use std::path::PathBuf;

#[cfg(windows)]
pub const YTDLP_NAME: &str = "yt-dlp.exe";
#[cfg(not(windows))]
pub const YTDLP_NAME: &str = "yt-dlp";

#[cfg(windows)]
pub const FFMPEG_NAME: &str = "ffmpeg.exe";
#[cfg(not(windows))]
pub const FFMPEG_NAME: &str = "ffmpeg";

/// Найденные инструменты.
pub struct Tools {
    pub ytdlp: PathBuf,
    /// ffmpeg опционален на этапе поиска, но обязателен для реальной работы:
    /// без него не склеить видео+аудио и не извлечь MP3. Отсутствие — это
    /// предупреждение в UI, а не отказ запускаться.
    pub ffmpeg: Option<PathBuf>,
}

pub fn locate(name: &str) -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

pub fn discover() -> Result<Tools, String> {
    let ytdlp = locate(YTDLP_NAME).ok_or_else(|| {
        format!(
            "Не найден {YTDLP_NAME}. Положите его рядом с Savio \
             или установите так, чтобы он был доступен в PATH."
        )
    })?;

    Ok(Tools {
        ytdlp,
        ffmpeg: locate(FFMPEG_NAME),
    })
}
