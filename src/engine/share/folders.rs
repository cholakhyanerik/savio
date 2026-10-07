//! Где на этом компьютере лежат системные папки: рабочий стол, загрузки…
//!
//! Путь угадывать по имени нельзя, и это не перестраховка. На Windows
//! с OneDrive «Рабочий стол» и «Документы» живут в `…\OneDrive\Desktop`,
//! а `%USERPROFILE%\Desktop` при этом пуст или его нет вовсе: файл с телефона
//! лёг бы туда, где человек его не увидит, — с кодом успеха и словом «Готово»
//! на экране. На Linux имена папок переводятся вместе с системой
//! («Рабочий стол», «Загрузки»), и `~/Desktop` там чаще нет. Поэтому каждая
//! система спрашивается своим способом: Windows — `SHGetKnownFolderPath`,
//! Linux — файлом `user-dirs.dirs` по XDG, macOS — домашней папкой (имена
//! там английские всегда, переводится только их показ в Finder).
//!
//! Есть ли папка на диске, здесь не проверяется: это делает тот, кто зовёт,
//! заодно сверяя её с папкой раздачи.

use std::path::PathBuf;

use crate::model::Destination;

/// Системные папки этого компьютера: какая и где. Папки, о которых система
/// не знает, в списке нет.
///
/// Чтение реестра или файла — звать только из потока раздачи, не из кадра.
#[cfg(windows)]
pub fn system_folders() -> Vec<(Destination, PathBuf)> {
    Destination::SYSTEM
        .into_iter()
        .filter_map(|which| Some((which, windows::known_folder(which)?)))
        .collect()
}

#[cfg(target_os = "macos")]
pub fn system_folders() -> Vec<(Destination, PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    Destination::SYSTEM
        .into_iter()
        .filter_map(|which| {
            let name = match which {
                Destination::Shared => return None,
                Destination::Desktop => "Desktop",
                Destination::Downloads => "Downloads",
                Destination::Documents => "Documents",
                Destination::Pictures => "Pictures",
                // Не «Videos»: в macOS папка для видео называется «Movies».
                Destination::Videos => "Movies",
                Destination::Music => "Music",
            };
            Some((which, home.join(name)))
        })
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn system_folders() -> Vec<(Destination, PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    // XDG велит не верить пустому и относительному `XDG_CONFIG_HOME`.
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    // Файла нет — не беда: так бывает без пакета xdg-user-dirs, и тогда
    // остаются английские имена из `fallback_name`.
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).unwrap_or_default();

    Destination::SYSTEM
        .into_iter()
        .filter_map(|which| {
            let (key, fallback) = xdg_names(which)?;
            match user_dir(&text, key, &home) {
                Lookup::Set(path) => Some((which, path)),
                Lookup::Disabled => None,
                Lookup::Missing => Some((which, home.join(fallback))),
            }
        })
        .collect()
}

/// Имя записи в `user-dirs.dirs` и английское имя папки на случай, если
/// записи нет.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn xdg_names(which: Destination) -> Option<(&'static str, &'static str)> {
    Some(match which {
        Destination::Shared => return None,
        Destination::Desktop => ("XDG_DESKTOP_DIR", "Desktop"),
        Destination::Downloads => ("XDG_DOWNLOAD_DIR", "Downloads"),
        Destination::Documents => ("XDG_DOCUMENTS_DIR", "Documents"),
        Destination::Pictures => ("XDG_PICTURES_DIR", "Pictures"),
        Destination::Videos => ("XDG_VIDEOS_DIR", "Videos"),
        Destination::Music => ("XDG_MUSIC_DIR", "Music"),
    })
}

/// Что сказал `user-dirs.dirs` об одной папке.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
#[derive(Debug, PartialEq, Eq)]
enum Lookup {
    Set(PathBuf),
    /// Записи нет вовсе — берём английское имя.
    Missing,
    /// Запись есть и указывает на саму домашнюю папку: по формату
    /// xdg-user-dirs это значит «папки нет». Класть файлы прямо в домашнюю
    /// папку, выдав её за «Рабочий стол», было бы враньём.
    Disabled,
}

/// Разбирает одну запись `user-dirs.dirs`: `XDG_DESKTOP_DIR="$HOME/Рабочий стол"`.
///
/// Формат задан xdg-user-dirs: значение в двойных кавычках, путь либо
/// абсолютный, либо от `$HOME/`. Файл читается оболочкой, поэтому при повторе
/// побеждает последняя запись.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn user_dir(text: &str, key: &str, home: &std::path::Path) -> Lookup {
    let found = text.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')
            .map(str::trim)
    });
    let Some(raw) = found else {
        return Lookup::Missing;
    };
    let Some(value) = raw.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) else {
        return Lookup::Missing;
    };

    // `$HOME` — только целым словом: `$HOMEDIR/x` формат не разрешает.
    let path = if value == "$HOME" {
        return Lookup::Disabled;
    } else if let Some(rest) = value.strip_prefix("$HOME/") {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return Lookup::Disabled;
        }
        home.join(rest)
    } else if value.starts_with('/') {
        PathBuf::from(value)
    } else {
        // Относительный путь формат не разрешает — запись битая.
        return Lookup::Missing;
    };

    if path == home {
        Lookup::Disabled
    } else {
        Lookup::Set(path)
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::ptr::null_mut;

    use crate::model::Destination;

    /// GUID в том виде, в каком его ждёт Windows. Полями, а не байтами
    /// подряд: так он выровнен как `u32`, как того требует система.
    #[repr(C)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    // Обе библиотеки системные и уже загружены в процесс окном (winit берёт
    // из `shell32` перетаскивание файлов, из `ole32` — COM), так что запуск
    // Savio от этих строк не замедляется.
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(
            id: *const Guid,
            flags: u32,
            token: *mut c_void,
            path: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(mem: *mut c_void);
    }

    /// `FOLDERID_*` из `KnownFolders.h`. Числа не менялись с Windows Vista.
    fn folder_id(which: Destination) -> Option<Guid> {
        let (d1, d2, d3, d4) = match which {
            Destination::Shared => return None,
            Destination::Desktop => (
                0xB4BF_CC3A,
                0xDB2C,
                0x424C,
                [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
            ),
            Destination::Downloads => (
                0x374D_E290,
                0x123F,
                0x4565,
                [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B],
            ),
            Destination::Documents => (
                0xFDD3_9AD0,
                0x238F,
                0x46AF,
                [0xAD, 0xB4, 0x6C, 0x85, 0x48, 0x03, 0x69, 0xC7],
            ),
            Destination::Pictures => (
                0x33E2_8130,
                0x4E1E,
                0x4676,
                [0x83, 0x5A, 0x98, 0x39, 0x5C, 0x3B, 0xC3, 0xBB],
            ),
            Destination::Videos => (
                0x1898_9B1D,
                0x99B5,
                0x455B,
                [0x84, 0x1C, 0xAB, 0x7C, 0x74, 0xE4, 0xDD, 0xFC],
            ),
            Destination::Music => (
                0x4BD8_D571,
                0x6D19,
                0x48D3,
                [0xBE, 0x97, 0x42, 0x22, 0x20, 0x08, 0x0E, 0x43],
            ),
        };
        Some(Guid { d1, d2, d3, d4 })
    }

    /// Путь к папке так, как его видит проводник: с переносом в OneDrive
    /// и с любым другим, сделанным через «Свойства → Расположение».
    pub fn known_folder(which: Destination) -> Option<PathBuf> {
        let id = folder_id(which)?;
        let mut raw: *mut u16 = null_mut();
        let status = unsafe { SHGetKnownFolderPath(&id, 0, null_mut(), &mut raw) };

        let path = (status >= 0 && !raw.is_null()).then(|| {
            // Строка оканчивается нулём, длины система не сообщает.
            let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
            let wide = unsafe { std::slice::from_raw_parts(raw, len) };
            PathBuf::from(OsString::from_wide(wide))
        });
        // Освобождать велено при любом исходе, в том числе при ошибке:
        // система может выделить память и под пустой ответ. Нулевой указатель
        // `CoTaskMemFree` принимает.
        unsafe { CoTaskMemFree(raw.cast()) };
        path
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const DIRS: &str = "\
# This file is written by xdg-user-dirs-update
XDG_DESKTOP_DIR=\"$HOME/Рабочий стол\"
XDG_DOWNLOAD_DIR=\"$HOME/Загрузки\"
XDG_TEMPLATES_DIR=\"$HOME/\"
XDG_MUSIC_DIR=\"/mnt/media/Музыка\"
XDG_VIDEOS_DIR=\"$HOME\"
XDG_PICTURES_DIR=\"$HOME/Old\"
XDG_PICTURES_DIR=\"$HOME/Изображения\"
XDG_DOCUMENTS_DIR=Документы
";

    /// Переведённые имена из `user-dirs.dirs` берутся как есть: на русском
    /// Linux рабочий стол — «Рабочий стол», а не `~/Desktop`.
    #[test]
    fn the_desktop_is_read_from_user_dirs() {
        let home = Path::new("/home/вася");
        assert_eq!(
            user_dir(DIRS, "XDG_DESKTOP_DIR", home),
            Lookup::Set(home.join("Рабочий стол"))
        );
        assert_eq!(
            user_dir(DIRS, "XDG_DOWNLOAD_DIR", home),
            Lookup::Set(home.join("Загрузки"))
        );
        // Абсолютный путь — как есть.
        assert_eq!(
            user_dir(DIRS, "XDG_MUSIC_DIR", home),
            Lookup::Set(PathBuf::from("/mnt/media/Музыка"))
        );
        // Повтор: побеждает последняя запись, как у оболочки.
        assert_eq!(
            user_dir(DIRS, "XDG_PICTURES_DIR", home),
            Lookup::Set(home.join("Изображения"))
        );
    }

    /// Папка, равная домашней, по формату выключена, а битая запись — всё
    /// равно что её нет.
    #[test]
    fn a_disabled_or_broken_user_dir_is_not_a_folder() {
        let home = Path::new("/home/вася");
        assert_eq!(user_dir(DIRS, "XDG_TEMPLATES_DIR", home), Lookup::Disabled);
        assert_eq!(user_dir(DIRS, "XDG_VIDEOS_DIR", home), Lookup::Disabled);
        // Домашняя папка, записанная полным путём, — тоже выключенная.
        assert_eq!(
            user_dir("XDG_DESKTOP_DIR=\"/home/вася\"", "XDG_DESKTOP_DIR", home),
            Lookup::Disabled
        );
        // Без кавычек формат запись не признаёт.
        assert_eq!(user_dir(DIRS, "XDG_DOCUMENTS_DIR", home), Lookup::Missing);
        assert_eq!(user_dir(DIRS, "XDG_PUBLICSHARE_DIR", home), Lookup::Missing);
        assert_eq!(user_dir("", "XDG_DESKTOP_DIR", home), Lookup::Missing);
        // Похожее начало имени — не то же имя.
        assert_eq!(
            user_dir("XDG_DESKTOP_DIR_OLD=\"$HOME/x\"", "XDG_DESKTOP_DIR", home),
            Lookup::Missing
        );
    }

    /// Каждой системной папке есть что искать в `user-dirs.dirs`.
    #[test]
    fn every_system_folder_has_an_xdg_name() {
        for which in Destination::SYSTEM {
            assert!(xdg_names(which).is_some(), "{which:?}");
        }
        assert!(xdg_names(Destination::Shared).is_none());
    }

    /// Что сказала эта машина. Числа не сверяются — у каждой машины свои, —
    /// но путь обязан быть полным: относительный лёг бы рядом с Savio.
    #[test]
    fn system_folders_on_this_machine_are_absolute() {
        for (which, path) in system_folders() {
            assert!(path.is_absolute(), "{which:?}: {}", path.display());
        }
    }
}
