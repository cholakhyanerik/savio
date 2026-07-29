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

/// Каталог Savio в данных пользователя — корень для всего, что мы храним
/// между запусками: докачанные инструменты (`bin`) и запомненные настройки.
///
/// Соглашения разные на каждой ОС, но идея одна: у пользователя, без прав
/// администратора и без записи в системные каталоги.
///
/// На Windows это именно `%LOCALAPPDATA%`, а не `%APPDATA%`: последний в
/// доменной сети входит в перемещаемый профиль и гонял бы сотню мегабайт
/// ffmpeg по сети при каждом входе. Заодно `%LOCALAPPDATA%` не попадает под
/// перенос папок в OneDrive — в `Документах` бинарник мог бы стать заглушкой
/// «файл доступен онлайн» и не запуститься.
pub fn app_dir() -> Option<PathBuf> {
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
        Some(base.join("Savio"))
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Savio"),
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
        Some(base.join("savio"))
    }
}

/// Каталог, куда Savio кладёт докачанные инструменты.
///
/// Отдельная подпапка внутри `app_dir`, а не сам `app_dir`: рядом лежат и
/// другие наши файлы (настройки), и мешать их с бинарниками не стоит — по
/// имени папки должно быть видно, что в ней лежит.
///
/// И не `~/.local/bin` на Linux: он лежит в PATH, и наш ffmpeg молча подменил
/// бы системный для всех остальных программ пользователя.
pub fn data_dir() -> Option<PathBuf> {
    Some(app_dir()?.join("bin"))
}

/// Найденные инструменты.
pub struct Tools {
    pub ytdlp: PathBuf,
    /// ffmpeg опционален на этапе поиска, но обязателен для реальной работы:
    /// без него не склеить видео+аудио и не извлечь MP3. Отсутствие — это
    /// предупреждение в UI, а не отказ запускаться.
    pub ffmpeg: Option<PathBuf>,
    /// ffprobe ищется отдельно от ffmpeg, хотя качаются они всегда парой.
    ///
    /// Пара может разъехаться по разным папкам, и это не выдуманный случай:
    /// у пользователя в PATH лежит один ffmpeg без ffprobe, `missing()`
    /// считает такую установку неполной и докачивает пару к себе, а порядок
    /// поиска оставляет в деле системный ffmpeg и наш ffprobe. Знать об этом
    /// нужно потому, что yt-dlp ищет ffprobe **сам** — рядом с тем ffmpeg,
    /// что назван в `--ffmpeg-location`, — и в такой раскладке не находит
    /// (см. `engine::start`).
    pub ffprobe: Option<PathBuf>,
}

/// Откуда взялся найденный бинарник.
///
/// Нужно не для поиска, а для обновления: свою копию Savio вправе заменить, а
/// чужую — нет. Молча положить свежий yt-dlp в каталог данных, когда работает
/// копия из PATH, нельзя вдвойне: порядок поиска оставит в деле старую, и
/// «обновление» окажется пустышкой, о которой пользователь никак не узнает.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Лежит рядом с exe Savio — портативная поставка. Её собрал тот, кто
    /// раздаёт сборку, и подменять её файлы мы не вправе.
    Portable,
    /// Найден в PATH: системная установка или пакетный менеджер
    /// (winget, brew, apt). Обновляется своим менеджером, а не нами.
    System,
    /// Каталог данных Savio — единственное, что мы скачали сами
    /// и можем заменять.
    Owned,
}

pub fn locate(name: &str) -> Option<PathBuf> {
    locate_with_origin(name).map(|(path, _)| path)
}

/// То же, что `locate`, но сообщает и происхождение файла.
///
/// Порядок обхода обязан совпадать с `locate` до последнего шага: разойдись
/// они, и обновлялся бы не тот файл, который потом запускается.
pub fn locate_with_origin(name: &str) -> Option<(PathBuf, Origin)> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some((candidate, Origin::Portable));
            }
        }

    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    {
        return Some((found, Origin::System));
    }

    // Последний шаг — то, что Savio скачал себе сам при первом запуске.
    let candidate = data_dir()?.join(name);
    candidate
        .is_file()
        .then_some((candidate, Origin::Owned))
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
        ffprobe: locate(FFPROBE_NAME),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог инструментов обязан остаться прежним — `<данные>/bin`.
    ///
    /// Сместись он на уровень выше, и ошибки не случилось бы ни при сборке,
    /// ни в работе: Savio просто перестал бы находить уже скачанные ffmpeg
    /// и yt-dlp и молча выкачал бы триста мегабайт заново, в новое место.
    #[test]
    fn tools_live_in_a_bin_subfolder_of_the_app_folder() {
        let (Some(app), Some(data)) = (app_dir(), data_dir()) else {
            // Ни `LOCALAPPDATA`, ни `HOME` — сравнивать нечего.
            return;
        };
        assert_eq!(data.parent(), Some(app.as_path()));
        assert_eq!(data.file_name(), Some(std::ffi::OsStr::new("bin")));
    }
}
