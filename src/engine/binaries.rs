//! Поиск внешних бинарников.
//!
//! Порядок: рядом с нашим exe (портативная поставка) → PATH (системная
//! установка) → каталог данных Savio (докачано при первом запуске).
//!
//! Каталог данных стоит **последним** намеренно: если у пользователя уже есть
//! системный ffmpeg, работать надо с ним, а не с нашей копией. Первые два шага
//! этим порядком не затронуты и продолжают работать как раньше.
//!
//! Сами бинарники в поставку Savio не входят и входить не должны: сборки
//! ffmpeg от BtbN собраны с `--enable-gpl`, и включение их в дистрибутив
//! означало бы распространение GPL-кода со всеми обязательствами по исходникам.
//! Загрузка на машину пользователя с апстрима распространением не является.

use std::path::PathBuf;

#[cfg(windows)]
pub const YTDLP_NAME: &str = "yt-dlp.exe";
#[cfg(not(windows))]
pub const YTDLP_NAME: &str = "yt-dlp";

#[cfg(windows)]
pub const FFMPEG_NAME: &str = "ffmpeg.exe";
#[cfg(not(windows))]
pub const FFMPEG_NAME: &str = "ffmpeg";

/// yt-dlp зовёт `ffprobe` сам — например, при починке HLS-потоков (трансляции,
/// премьеры). Без него постобработка ругается и ремуксит вслепую, поэтому
/// качаем пару, а не один `ffmpeg`.
#[cfg(windows)]
pub const FFPROBE_NAME: &str = "ffprobe.exe";
#[cfg(not(windows))]
pub const FFPROBE_NAME: &str = "ffprobe";

/// Каталог, куда Savio кладёт докачанные инструменты.
///
/// Соглашения разные на каждой ОС, но идея одна: у пользователя, без прав
/// администратора и без записи в системные каталоги.
///
/// На Windows это именно `%LOCALAPPDATA%`, а не `%APPDATA%`: последний в
/// доменной сети входит в перемещаемый профиль и гонял бы сотню мегабайт
/// ffmpeg по сети при каждом входе. Заодно `%LOCALAPPDATA%` не попадает под
/// перенос папок в OneDrive — в `Документах` бинарник мог бы стать заглушкой
/// «файл доступен онлайн» и не запуститься.
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .filter(|v| !v.is_empty())
                    .map(|p| PathBuf::from(p).join("AppData").join("Local"))
            })?;
        Some(base.join("Savio").join("bin"))
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Savio")
                .join("bin"),
        )
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // XDG требует игнорировать `XDG_DATA_HOME`, если он пуст или задан
        // относительным путём, — отсюда проверка `is_absolute`.
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".local").join("share"))
            })?;
        // Не `~/.local/bin`: он лежит в PATH, и наш ffmpeg молча подменил бы
        // системный для всех остальных программ пользователя.
        Some(base.join("savio").join("bin"))
    }
}

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

    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    {
        return Some(found);
    }

    // Последний шаг — то, что Savio скачал себе сам при первом запуске.
    let candidate = data_dir()?.join(name);
    candidate.is_file().then_some(candidate)
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
