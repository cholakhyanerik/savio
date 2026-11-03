//! Что умеет эта система: какой у неё экран, чем его захватить и сжать,
//! откуда взять звук.
//!
//! Три ветки `#[cfg]`, и наружу они торчат одними и теми же именами:
//! [`primary`], [`sources`], [`ENCODERS`], [`caveat`], а вне Windows ещё
//! `sound_input` (на Windows звук снимает `loopback`).

use crate::model::ScreenMode;

/// Экран, который будет записан.
pub(super) struct Screen {
    /// Номер для источника — `monitor_idx` у `gfxcapture`. На других
    /// системах не нужен.
    pub index: u32,
    /// `None` — размер или частоту система не назвала.
    pub mode: Option<ScreenMode>,
}

#[cfg(not(windows))]
pub(super) use imp::sound_input;
pub(super) use imp::{ENCODERS, caveat, primary, sources};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use super::Screen;
    use crate::model::{CaptureTrouble, ScreenMode, VideoEncoder};

    /// Кадры с видеокарты — в память, в том формате, в каком их отдал захват.
    const DOWNLOAD: &str = "hwdownload,format=bgra";
    /// То же, но в NV12: Quick Sync из памяти принимает только его.
    const DOWNLOAD_NV12: &str = "hwdownload,format=bgra,format=nv12";

    /// Лестница кодировщиков, сверху вниз.
    ///
    /// Сначала кадры прямо с видеокарты: так NVENC держит и 4K на 144 Гц,
    /// потому что кадр не покидает видеопамять. Потом те же кодировщики
    /// с выгрузкой: AMF на Radeon 780M кадры с видеокарты не принял
    /// («SubmitInput() failed with error 18»), а выгруженные — принял
    /// (проверено вживую 2026-11-03, ffmpeg 9.0.2). Процессор — последним:
    /// x264 на 1440p и 144 Гц съедает несколько ядер.
    pub const ENCODERS: &[(VideoEncoder, Option<&str>)] = &[
        (VideoEncoder::Nvenc, None),
        (VideoEncoder::Amf, None),
        (VideoEncoder::Nvenc, Some(DOWNLOAD)),
        (VideoEncoder::Amf, Some(DOWNLOAD)),
        (VideoEncoder::QuickSync, Some(DOWNLOAD_NV12)),
        (VideoEncoder::X264, Some(DOWNLOAD)),
    ];

    /// Источники экрана, лучший первым (замеры — в шапке `capture`).
    ///
    /// `monitor_idx` у `gfxcapture` — номер в порядке `EnumDisplayMonitors`
    /// (по исходникам ffmpeg 9.0, `vsrc_gfxcapture_winrt.cpp`), и [`primary`]
    /// считает его тем же перечислением. У `ddagrab` номер свой — выход
    /// первой видеокарты, — и главный экран там почти всегда нулевой;
    /// выбора экрана у записи пока нет.
    pub fn sources(index: u32, rate: u32) -> Result<Vec<Vec<String>>, CaptureTrouble> {
        let lavfi = |graph: String| ["-f".to_owned(), "lavfi".into(), "-i".into(), graph].to_vec();
        Ok(vec![
            lavfi(format!(
                "gfxcapture=monitor_idx={index}:max_framerate={rate}:capture_cursor=1"
            )),
            lavfi(format!("ddagrab=output_idx=0:framerate={rate}:draw_mouse=1")),
        ])
    }

    /// Оговорок у Windows нет.
    pub fn caveat() -> Option<CaptureTrouble> {
        None
    }

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    /// `MONITORINFOEXW`.
    #[repr(C)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
        device: [u16; 32],
    }

    /// `DEVMODEW` с объединениями, развёрнутыми в вариант для экрана.
    /// Размер сверяется ниже: промах в раскладке функция приняла бы молча
    /// и вернула бы частоту из чужого поля.
    #[repr(C)]
    struct DevMode {
        device_name: [u16; 32],
        spec_version: u16,
        driver_version: u16,
        size: u16,
        driver_extra: u16,
        fields: u32,
        position_x: i32,
        position_y: i32,
        orientation: u32,
        fixed_output: u32,
        color: i16,
        duplex: i16,
        y_resolution: i16,
        tt_option: i16,
        collate: i16,
        form_name: [u16; 32],
        log_pixels: u16,
        bits_per_pel: u32,
        pels_width: u32,
        pels_height: u32,
        display_flags: u32,
        display_frequency: u32,
        icm_method: u32,
        icm_intent: u32,
        media_type: u32,
        dither_type: u32,
        reserved1: u32,
        reserved2: u32,
        panning_width: u32,
        panning_height: u32,
    }

    const _: () = assert!(std::mem::size_of::<MonitorInfo>() == 104);
    const _: () = assert!(std::mem::size_of::<DevMode>() == 220);

    const MONITORINFOF_PRIMARY: u32 = 1;
    /// `ENUM_CURRENT_SETTINGS` — `(DWORD)-1`.
    const ENUM_CURRENT_SETTINGS: u32 = u32::MAX;
    /// Сколько экранов готовы перечислить. Потолок по Правилу 1.
    const MONITOR_LIMIT: usize = 32;

    type EnumProc = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut Rect, isize) -> i32;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumDisplayMonitors(hdc: *mut c_void, clip: *const Rect, proc: EnumProc, data: isize)
        -> i32;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
        fn EnumDisplaySettingsW(device: *const u16, mode: u32, out: *mut DevMode) -> i32;
    }

    unsafe extern "system" fn collect(
        monitor: *mut c_void,
        _hdc: *mut c_void,
        _rect: *mut Rect,
        data: isize,
    ) -> i32 {
        // `data` — указатель на список из `primary`, живой всё перечисление.
        let list = unsafe { &mut *(data as *mut Vec<*mut c_void>) };
        list.push(monitor);
        i32::from(list.len() < MONITOR_LIMIT)
    }

    /// Главный экран: его номер в перечислении и режим.
    ///
    /// Режим — из `EnumDisplaySettingsW`, а не из прямоугольника монитора:
    /// прямоугольник у процесса без осведомлённости о DPI масштабирован
    /// (2560 превращаются в 2048 при 125 %), а режим всегда в настоящих
    /// точках экрана.
    pub fn primary() -> Option<Screen> {
        let mut monitors: Vec<*mut c_void> = Vec::new();
        unsafe {
            EnumDisplayMonitors(
                null_mut(),
                null(),
                collect,
                (&mut monitors as *mut Vec<*mut c_void>) as isize,
            )
        };
        for (index, &monitor) in monitors.iter().enumerate() {
            let mut info = MonitorInfo {
                size: std::mem::size_of::<MonitorInfo>() as u32,
                monitor: Rect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                work: Rect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                flags: 0,
                device: [0; 32],
            };
            if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0
                || info.flags & MONITORINFOF_PRIMARY == 0
            {
                continue;
            }
            return Some(Screen {
                index: u32::try_from(index).ok()?,
                mode: mode_of(&info.device),
            });
        }
        None
    }

    fn mode_of(device: &[u16; 32]) -> Option<ScreenMode> {
        // Всё нулями, кроме размера: так `DEVMODEW` и положено подавать.
        let mut mode: DevMode = unsafe { std::mem::zeroed() };
        mode.size = std::mem::size_of::<DevMode>() as u16;
        if unsafe { EnumDisplaySettingsW(device.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) } == 0 {
            return None;
        }
        // 0 и 1 — «частота по умолчанию у железа», то есть система её не
        // назвала (так в документации `DEVMODEW`). Это не частота.
        (mode.pels_width > 0 && mode.pels_height > 0 && mode.display_frequency > 1).then_some(
            ScreenMode {
                width: mode.pels_width,
                height: mode.pels_height,
                refresh: mode.display_frequency,
            },
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Главный экран находится, и режим у него правдоподобный. На машине
        /// без экрана (сервер CI) перечисление пусто — это не провал.
        #[test]
        fn the_primary_screen_has_a_plausible_mode() {
            let Some(screen) = primary() else {
                return;
            };
            if let Some(mode) = screen.mode {
                assert!(mode.width >= 320 && mode.height >= 200, "{mode:?}");
                assert!((20..=1000).contains(&mode.refresh), "{mode:?}");
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::process::{Command, Stdio};

    use super::Screen;
    use crate::model::{CaptureTrouble, ScreenMode, SoundSource, VideoEncoder};

    /// x11grab отдаёт кадры в памяти, и сжимает их процессор: аппаратные
    /// пути Linux (VA-API) требуют своей выгрузки на карту и своих
    /// устройств, и проверить их на машине разработчика нечем.
    pub const ENCODERS: &[(VideoEncoder, Option<&str>)] = &[(VideoEncoder::X264, None)];

    /// x11grab по умолчанию берёт рабочий стол целиком — размер не нужен.
    pub fn sources(_index: u32, rate: u32) -> Result<Vec<Vec<String>>, CaptureTrouble> {
        let display = std::env::var("DISPLAY")
            .ok()
            .filter(|display| !display.is_empty())
            .ok_or(CaptureTrouble::NoDisplay)?;
        Ok(vec![
            [
                "-f".to_owned(),
                "x11grab".into(),
                "-framerate".into(),
                rate.to_string(),
                "-draw_mouse".into(),
                "1".into(),
                "-i".into(),
                display,
            ]
            .to_vec(),
        ])
    }

    /// В сеансе Wayland x11grab открывается и пишет — но видит только окна
    /// X11 (XWayland), а остальное в записи чёрное. Сказать об этом до
    /// записи может только Savio: ffmpeg ответит успехом.
    pub fn caveat() -> Option<CaptureTrouble> {
        let wayland = std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland")
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|name| !name.is_empty());
        wayland.then_some(CaptureTrouble::Wayland)
    }

    /// Звук системы — «монитор» устройства вывода по умолчанию. Имя
    /// `@DEFAULT_MONITOR@` понимают и PulseAudio, и PipeWire.
    pub fn sound_input() -> (Vec<String>, SoundSource) {
        (
            ["-f", "pulse", "-thread_queue_size", "1024", "-i", "@DEFAULT_MONITOR@"]
                .map(String::from)
                .to_vec(),
            SoundSource::System,
        )
    }

    /// Режим главного экрана — из `xrandr`: стандартная библиотека про X11
    /// не знает ничего, а своя связка с Xlib ради двух чисел — зависимость
    /// от системной библиотеки, которой может не быть.
    pub fn primary() -> Option<Screen> {
        let out = Command::new("xrandr")
            .arg("--current")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        Some(Screen {
            index: 0,
            mode: parse_xrandr(&String::from_utf8_lossy(&out.stdout)),
        })
    }

    /// Текущий режим главного выхода, а нет главного — первого подключённого.
    ///
    /// Выход — строка без отступа со словом `connected`; его режимы — строки
    /// с отступом, а текущий помечен звёздочкой после частоты:
    /// `   2560x1440    143.97*+  59.95`.
    fn parse_xrandr(text: &str) -> Option<ScreenMode> {
        let mut first = None;
        let mut current: Option<bool> = None;
        for line in text.lines() {
            if !line.starts_with(char::is_whitespace) {
                let mut words = line.split_whitespace();
                let _name = words.next();
                current = (words.next() == Some("connected")).then(|| line.contains(" primary "));
                continue;
            }
            let Some(primary) = current else {
                continue;
            };
            let mut words = line.split_whitespace();
            let Some(size) = words.next() else {
                continue;
            };
            let Some(rate) = words.find(|word| word.contains('*')) else {
                continue;
            };
            // Строка, которую не разобрать, пропускается, а не обрывает
            // разбор: формат вывода принадлежит xrandr, а не нам.
            let Some((width, height)) = size.trim_end_matches('i').split_once('x') else {
                continue;
            };
            let (Ok(width), Ok(height), Ok(refresh)) = (
                width.parse(),
                height.parse(),
                rate.trim_end_matches(['*', '+']).parse::<f64>(),
            ) else {
                continue;
            };
            let mode = ScreenMode {
                width,
                height,
                refresh: refresh.round() as u32,
            };
            if primary {
                return Some(mode);
            }
            first.get_or_insert(mode);
        }
        first
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Ответ `xrandr` с двумя экранами: главный — второй, а не первый.
        #[test]
        fn the_primary_output_wins_over_the_first_one() {
            let text = "Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384\n\
                        eDP-1 connected 1920x1080+2560+0 (normal left inverted right) 309mm x 174mm\n\
                        \x20  1920x1080     60.02*+  59.97\n\
                        \x20  1680x1050     59.95\n\
                        HDMI-1 connected primary 2560x1440+0+0 (normal) 597mm x 336mm\n\
                        \x20  2560x1440    143.97*+  59.95\n\
                        DP-1 disconnected (normal left inverted right x axis y axis)\n";
            assert_eq!(
                parse_xrandr(text),
                Some(ScreenMode {
                    width: 2560,
                    height: 1440,
                    refresh: 144
                })
            );
        }

        /// Без главного — первый подключённый; отключённые не в счёт.
        #[test]
        fn without_a_primary_the_first_connected_output_counts() {
            let text = "DP-1 disconnected (normal)\n\
                        eDP-1 connected 1920x1080+0+0 (normal) 309mm x 174mm\n\
                        \x20  1920x1080     60.02*+\n";
            assert_eq!(
                parse_xrandr(text),
                Some(ScreenMode {
                    width: 1920,
                    height: 1080,
                    refresh: 60
                })
            );
            assert_eq!(parse_xrandr(""), None);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    use super::Screen;
    use crate::model::{CaptureTrouble, ScreenMode, SoundSource, VideoEncoder};

    /// VideoToolbox — кодировщик самой системы. Ключ `-q:v` (постоянное
    /// качество) он понимает только на Apple Silicon; на Intel проба
    /// откажет, и запись уйдёт к x264.
    pub const ENCODERS: &[(VideoEncoder, Option<&str>)] =
        &[(VideoEncoder::VideoToolbox, None), (VideoEncoder::X264, None)];

    /// Экран по имени, а не по номеру: номера у avfoundation сквозные
    /// с камерами, и «1» на машине с камерой и без неё — разные устройства.
    pub fn sources(_index: u32, rate: u32) -> Result<Vec<Vec<String>>, CaptureTrouble> {
        Ok(vec![
            [
                "-f".to_owned(),
                "avfoundation".into(),
                "-capture_cursor".into(),
                "1".into(),
                "-framerate".into(),
                rate.to_string(),
                "-i".into(),
                "Capture screen 0".into(),
            ]
            .to_vec(),
        ])
    }

    pub fn caveat() -> Option<CaptureTrouble> {
        None
    }

    /// Звук системы на macOS без стороннего виртуального устройства не
    /// снять ничем, поэтому — микрофон по умолчанию. Окно говорит об этом
    /// прямо (`SoundSource::Microphone`).
    pub fn sound_input() -> (Vec<String>, SoundSource) {
        (
            ["-f", "avfoundation", "-thread_queue_size", "1024", "-i", ":default"]
                .map(String::from)
                .to_vec(),
            SoundSource::Microphone,
        )
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
        fn CGDisplayModeGetRefreshRate(mode: *mut c_void) -> f64;
        fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: *mut c_void) -> usize;
        fn CGDisplayModeRelease(mode: *mut c_void);
    }

    /// Режим главного экрана. Встроенные экраны частоту часто не называют
    /// (0 Гц) — тогда режима нет, и запись идёт с частотой по умолчанию.
    pub fn primary() -> Option<Screen> {
        let mode = unsafe { CGDisplayCopyDisplayMode(CGMainDisplayID()) };
        if mode.is_null() {
            return Some(Screen {
                index: 0,
                mode: None,
            });
        }
        let (rate, width, height) = unsafe {
            (
                CGDisplayModeGetRefreshRate(mode),
                CGDisplayModeGetPixelWidth(mode),
                CGDisplayModeGetPixelHeight(mode),
            )
        };
        unsafe { CGDisplayModeRelease(mode) };
        let mode = (rate >= 1.0 && width > 0 && height > 0).then(|| ScreenMode {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
            refresh: rate.round() as u32,
        });
        Some(Screen { index: 0, mode })
    }
}
