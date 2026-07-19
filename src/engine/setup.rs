//! Докачивание внешних инструментов при первом запуске.
//!
//! Про UI не знает ничего: наружу торчат те же `Event` и `notify`, что и у
//! загрузки ролика. Пакетные менеджеры (winget / brew / apt) сознательно не
//! используются: `apt` требует root, `winget` есть не на всех Windows 10 и
//! поднимает UAC, `brew` у большинства не установлен вовсе. Установка «молча,
//! без окна терминала» через них недостижима, а прямая загрузка статических
//! сборок работает одинаково на всех трёх ОС и без прав администратора.
//!
//! Асимметрия по важности сохраняет Правило 2: без `yt-dlp` работать нельзя,
//! а без `ffmpeg` — можно, поэтому сбой его загрузки не считается ошибкой
//! установки и приводит лишь к предупреждению в журнале.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use super::binaries::{self, FFMPEG_NAME, FFPROBE_NAME, YTDLP_NAME};
use super::sha256::{self, Sha256};
use crate::model::{Event, Progress};

// ---------------------------------------------------------------------------
// Что и откуда качать
// ---------------------------------------------------------------------------

/// Тег релиза резолвим отдельным запросом и дальше берём **оба** файла
/// (бинарник и суммы) из него: ссылка `/releases/latest/download/` в момент
/// выхода нового релиза может отдать файлы из разных версий, и сверка суммы
/// упадёт на ровном месте.
const YTDLP_RELEASE_API: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
const YTDLP_SUMS: &str = "SHA2-256SUMS";

// Имя ассета yt-dlp. Голый `yt-dlp` (zipimport) не берём — он требует
// установленного Python; нужны самодостаточные сборки.
#[cfg(all(windows, target_arch = "x86_64"))]
const YTDLP_ASSET: &str = "yt-dlp.exe";
#[cfg(all(windows, target_arch = "aarch64"))]
const YTDLP_ASSET: &str = "yt-dlp_arm64.exe";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const YTDLP_ASSET: &str = "yt-dlp_linux";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const YTDLP_ASSET: &str = "yt-dlp_linux_aarch64";
#[cfg(target_os = "macos")]
const YTDLP_ASSET: &str = "yt-dlp_macos";

/// Откуда брать ffmpeg с ffprobe.
///
/// Формат архива выбран под возможности распаковщика каждой ОС, а не по
/// удобству: см. комментарий к `extract`.
///
/// `allow(dead_code)` здесь обязателен: на каждой ОС используется ровно один
/// вариант, и второй компилятор честно считает неиспользуемым. Убирать
/// «лишний» вариант нельзя — он нужен другой платформе.
#[allow(dead_code)]
enum FfmpegSource {
    /// Один архив, внутри — `<корень>/bin/ffmpeg` и `<корень>/bin/ffprobe`.
    Bundle(&'static str),
    /// Два плоских архива, по одному на программу.
    Split {
        ffmpeg: &'static str,
        ffprobe: &'static str,
    },
}

// Windows и Linux — сборки BtbN, там оба бинарника лежат в одном архиве.
// Windows получает .zip, Linux — .tar.xz: см. `extract`.
#[cfg(all(windows, target_arch = "x86_64"))]
const FFMPEG_SOURCE: FfmpegSource = FfmpegSource::Bundle(
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
);
#[cfg(all(windows, target_arch = "aarch64"))]
const FFMPEG_SOURCE: FfmpegSource = FfmpegSource::Bundle(
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-winarm64-gpl.zip",
);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const FFMPEG_SOURCE: FfmpegSource = FfmpegSource::Bundle(
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz",
);
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const FFMPEG_SOURCE: FfmpegSource = FfmpegSource::Bundle(
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linuxarm64-gpl.tar.xz",
);
// Под macOS готовых сборок «всё в одном» нет: BtbN её не собирает, а у
// оставшихся источников ffmpeg и ffprobe разложены по отдельным архивам.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const FFMPEG_SOURCE: FfmpegSource = FfmpegSource::Split {
    ffmpeg: "https://evermeet.cx/ffmpeg/getrelease/ffmpeg/zip",
    ffprobe: "https://evermeet.cx/ffmpeg/getrelease/ffprobe/zip",
};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const FFMPEG_SOURCE: FfmpegSource = FfmpegSource::Split {
    ffmpeg: "https://www.osxexperts.net/ffmpeg81arm.zip",
    ffprobe: "https://www.osxexperts.net/ffprobe81arm.zip",
};

/// Размер чанка при чтении из сети. 64 КБ — компромисс: меньше даёт лишние
/// системные вызовы, больше — рваный прогресс на медленном канале.
const CHUNK: usize = 64 * 1024;

/// Как часто отдавать прогресс в UI. Каждый чанк — это сотни событий в секунду
/// на быстром канале, и все они приводят к перерисовке окна.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Проверка
// ---------------------------------------------------------------------------

/// Чего не хватает на машине.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Missing {
    pub ytdlp: bool,
    pub ffmpeg: bool,
}

impl Missing {
    pub fn any(self) -> bool {
        self.ytdlp || self.ffmpeg
    }
}

/// Быстрая проверка: только обращения к файловой системе, ничего не качает.
///
/// `ffprobe` проверяем наравне с `ffmpeg`: без него yt-dlp не чинит HLS-потоки.
pub fn missing() -> Missing {
    Missing {
        ytdlp: binaries::locate(YTDLP_NAME).is_none(),
        ffmpeg: binaries::locate(FFMPEG_NAME).is_none()
            || binaries::locate(FFPROBE_NAME).is_none(),
    }
}

/// Ручка установки: позволяет её прервать.
///
/// Прервать нужно уметь обязательно — иначе зависшая на медленном канале
/// загрузка держала бы пользователя в модальном окне без выхода.
pub struct Handle {
    cancelled: Arc<AtomicBool>,
}

impl Handle {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Установка
// ---------------------------------------------------------------------------

/// Запускает установку в отдельном потоке.
///
/// Ровно как `engine::start`: UI-поток не блокируется ни на секунду, всё
/// общение идёт событиями.
pub fn start(
    what: Missing,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) -> Handle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        cancelled: Arc::clone(&cancelled),
    };

    std::thread::spawn(move || {
        match run(what, &tx, &notify, &cancelled) {
            Ok(()) => {
                let _ = tx.send(Event::Ready);
            }
            Err(err) => {
                let _ = tx.send(Event::Failed(err));
            }
        }
        notify();
    });

    handle
}

fn run(
    what: Missing,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let dir = binaries::data_dir()
        .ok_or("Не удалось определить папку для инструментов: не задана домашняя папка.")?;
    // `-C` в несуществующий каталог tar не создаёт, а падает с `could not chdir`.
    fs::create_dir_all(&dir)
        .map_err(|e| format!("Не удалось создать папку {}: {e}", dir.display()))?;

    let agent = agent();

    if what.ytdlp {
        install_ytdlp(&agent, &dir, tx, notify, cancelled)?;
    }

    if what.ffmpeg {
        // Сбой ffmpeg не срывает установку: без него Savio работает и просто
        // предупреждает, как и раньше. Источники сборок под macOS — сайты
        // энтузиастов без стабильных ссылок, ронять из-за них запуск нельзя.
        if let Err(err) = install_ffmpeg(&agent, &dir, tx, notify, cancelled) {
            if cancelled.load(Ordering::Relaxed) {
                return Err(err);
            }
            // Не только в журнал: он свёрнут, а перед первой же загрузкой
            // очищается — то есть исчезает ровно тогда, когда понадобится.
            // Причину видно на экране, а `Warning` не мешает `Ready`.
            let _ = tx.send(Event::Warning(format!(
                "Не удалось установить ffmpeg: {err}. \
                 Склейка видео со звуком и конвертация в MP3 работать не будут."
            )));
            notify();
        }
    }

    Ok(())
}

fn install_ytdlp(
    agent: &ureq::Agent,
    dir: &Path,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<(), String> {
    stage(tx, notify, "Ищу свежий выпуск yt-dlp…");
    let tag = latest_ytdlp_tag(agent)?;
    let _ = tx.send(Event::Log(format!("yt-dlp: выпуск {tag}")));

    let base = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{tag}");

    // Суммы тянем из того же выпуска, что и бинарник.
    let sums = fetch_text(agent, &format!("{base}/{YTDLP_SUMS}"))
        .map_err(|e| format!("Не удалось получить контрольные суммы yt-dlp: {e}"))?;
    let expected = sha256::find_sum(&sums, YTDLP_ASSET).ok_or_else(|| {
        format!("В списке контрольных сумм yt-dlp нет строки для {YTDLP_ASSET}.")
    })?;

    stage(tx, notify, "Скачиваю yt-dlp…");
    let tmp = dir.join(binary_tmp_name("ytdlp"));
    let digest = download(
        agent,
        &format!("{base}/{YTDLP_ASSET}"),
        &tmp,
        tx,
        notify,
        cancelled,
    )?;

    let actual = sha256::hex(&digest);
    if actual != expected {
        let _ = fs::remove_file(&tmp);
        return Err(
            "Скачанный yt-dlp повреждён: контрольная сумма не совпала. \
             Попробуйте запустить Savio ещё раз."
                .into(),
        );
    }

    make_executable(&tmp)?;
    let target = dir.join(YTDLP_NAME);
    replace(&tmp, &target)?;
    let _ = tx.send(Event::Log(format!("yt-dlp установлен: {}", target.display())));
    notify();
    Ok(())
}

fn install_ffmpeg(
    agent: &ureq::Agent,
    dir: &Path,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<(), String> {
    stage(tx, notify, "Скачиваю ffmpeg…");

    match FFMPEG_SOURCE {
        FfmpegSource::Bundle(url) => {
            let name = archive_tmp_name(url);
            download(agent, url, &dir.join(&name), tx, notify, cancelled)?;

            stage(tx, notify, "Распаковываю ffmpeg…");
            // Внутри архива путь вида `ffmpeg-master-latest-win64-gpl/bin/ffmpeg`,
            // поэтому снимаем два уровня и забираем только нужную пару.
            let members = [
                format!("*/bin/{FFMPEG_NAME}"),
                format!("*/bin/{FFPROBE_NAME}"),
            ];
            let refs: Vec<&str> = members.iter().map(String::as_str).collect();
            let result = extract(dir, &name, 2, &refs);
            let _ = fs::remove_file(dir.join(&name));
            result?;
        }
        FfmpegSource::Split { ffmpeg, ffprobe } => {
            for (url, member) in [(ffmpeg, FFMPEG_NAME), (ffprobe, FFPROBE_NAME)] {
                let name = archive_tmp_name(url);
                download(agent, url, &dir.join(&name), tx, notify, cancelled)?;

                stage(tx, notify, "Распаковываю ffmpeg…");
                // Эти архивы плоские: `--strip-components` здесь не нужен, а
                // явное имя отсекает служебный мусор вроде `__MACOSX/._ffmpeg`.
                let result = extract(dir, &name, 0, &[member]);
                let _ = fs::remove_file(dir.join(&name));
                result?;
            }
        }
    }

    // tar завершается с кодом 0, даже если по шаблону не нашлось ни одного
    // файла, — существование проверяем сами, коду возврата верить нельзя.
    for name in [FFMPEG_NAME, FFPROBE_NAME] {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(format!(
                "После распаковки не найден {name}: содержимое архива отличается от ожидаемого."
            ));
        }
        make_executable(&path)?;
    }

    let _ = tx.send(Event::Log(format!(
        "ffmpeg установлен: {}",
        dir.join(FFMPEG_NAME).display()
    )));
    notify();
    Ok(())
}

// ---------------------------------------------------------------------------
// Сеть
// ---------------------------------------------------------------------------

/// Настроенный HTTP-клиент.
///
/// Все таймауты в ureq по умолчанию отключены: без явного `timeout_global`
/// зависшее соединение держало бы модалку установки вечно.
///
/// `timeout_global` — это бюджет на всю операцию целиком, а не время простоя:
/// он тикает и когда данные исправно идут. Поэтому час, а не пятнадцать минут.
/// ffmpeg весит около 160 МБ, и на канале в 500 кбит/с честная загрузка займёт
/// три четверти часа — таймаут покороче обрывал бы её у самого конца, раз за
/// разом, именно у тех, кому и так тяжелее всех. От зависшего соединения
/// защищает `timeout_connect`, а от затянувшейся загрузки — кнопка «Отменить».
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // GitHub API отвечает 403 на запрос без User-Agent.
        .user_agent(concat!("savio/", env!("CARGO_PKG_VERSION")))
        // Подлинность скачанного держится на TLS, поэтому переход на http
        // по редиректу запрещаем: иначе и файл, и контрольную сумму к нему
        // мог бы подменить один и тот же посредник.
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(3600)))
        .build()
        .into()
}

fn latest_ytdlp_tag(agent: &ureq::Agent) -> Result<String, String> {
    let body = fetch_text(agent, YTDLP_RELEASE_API)
        .map_err(|e| format!("Не удалось узнать свежий выпуск yt-dlp: {e}"))?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| "GitHub вернул неожиданный ответ о выпуске yt-dlp.".to_string())?;

    value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "В ответе GitHub нет номера выпуска yt-dlp.".to_string())
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut body = agent
        .get(url)
        .call()
        .map_err(|e| human_net_error(&e))?
        .into_body();
    body.read_to_string()
        .map_err(|e| format!("не удалось прочитать ответ: {e}"))
}

/// Качает `url` в `dest`, попутно считая SHA-256 и отдавая прогресс.
///
/// Хеш считается на лету: перечитывать сотню мегабайт с диска ради него незачем.
///
/// Недокачанный файл за собой убирает при любом исходе. Иначе оборвавшаяся
/// на середине загрузка ffmpeg оставила бы в каталоге данных сотню мегабайт
/// мусора, и так — при каждой неудачной попытке.
fn download(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<[u8; 32], String> {
    let result = download_inner(agent, url, dest, tx, notify, cancelled);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

fn download_inner(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<[u8; 32], String> {
    // Обнуляем прогресс до запроса. Иначе, пока идут DNS, TLS и редиректы,
    // в UI висят цифры предыдущего файла: подпись уже «Скачиваю ffmpeg», а
    // полоса всё ещё заполнена на 100% от только что скачанного yt-dlp.
    // Нулевой `total` — это неопределённый индикатор, что здесь и правда так:
    // размер следующего файла ещё неизвестен.
    let _ = tx.send(Event::Progress(Progress::default()));
    notify();

    let response = agent.get(url).call().map_err(|e| human_net_error(&e))?;
    // Размер снимаем до того, как тело поглощено читателем.
    let total = response.body().content_length().unwrap_or(0);
    let mut reader = response.into_body().into_reader();

    let mut file = fs::File::create(dest)
        .map_err(|e| format!("не удалось создать файл {}: {e}", dest.display()))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    let started = Instant::now();
    let mut last_sent = Instant::now();

    loop {
        // Недокачанный файл удалит обёртка `download`.
        if cancelled.load(Ordering::Relaxed) {
            return Err("Установка отменена.".into());
        }

        let read = reader
            .read(&mut buf)
            .map_err(|e| format!("обрыв загрузки: {e}"))?;
        if read == 0 {
            break;
        }

        file.write_all(&buf[..read])
            .map_err(|e| format!("не удалось записать файл: {e}"))?;
        hasher.update(&buf[..read]);
        done += read as u64;

        // Событие на каждый чанк — это сотни перерисовок в секунду.
        if last_sent.elapsed() >= PROGRESS_EVERY {
            send_progress(tx, notify, done, total, started);
            last_sent = Instant::now();
        }
    }

    file.flush()
        .map_err(|e| format!("не удалось дописать файл: {e}"))?;
    send_progress(tx, notify, done, total, started);

    Ok(hasher.finish())
}

fn send_progress(tx: &Sender<Event>, notify: &impl Fn(), done: u64, total: u64, started: Instant) {
    let secs = started.elapsed().as_secs_f64();
    let speed = (secs > 0.0).then(|| done as f64 / secs).filter(|s| *s > 0.0);
    let eta = match (speed, total > done) {
        (Some(speed), true) => Some(((total - done) as f64 / speed) as u64),
        _ => None,
    };

    let _ = tx.send(Event::Progress(Progress {
        downloaded: done,
        total,
        speed_bps: speed,
        eta_secs: eta,
    }));
    notify();
}

/// Сообщения ureq рассчитаны на разработчика — переводим на человеческий.
fn human_net_error(err: &ureq::Error) -> String {
    let detail = err.to_string();
    match err {
        ureq::Error::StatusCode(code) => {
            format!("сервер ответил кодом {code}")
        }
        ureq::Error::Timeout(_) => {
            "истекло время ожидания. Проверьте подключение к интернету".into()
        }
        _ => format!("нет связи с сервером ({detail})"),
    }
}

// ---------------------------------------------------------------------------
// Файлы и распаковка
// ---------------------------------------------------------------------------

/// Распаковывает нужные файлы из архива системным `tar`.
///
/// Отдельной библиотеки под это нет намеренно: `tar` есть на всех трёх ОС
/// (на Windows — `bsdtar` из System32, начиная с Windows 10 1803).
///
/// Форматы архивов выбраны под возможности этих распаковщиков, а не наугад:
/// **`.tar.xz` не должен попадать на Windows**. Штатный `bsdtar` там собран
/// только с zlib и xz не понимает — при этом `tar --help` показывает ключ
/// `-J, --lzma`, так что по справке поддержку не определить. Отсюда `.zip`
/// на Windows и macOS и `.tar.xz` только на Linux.
///
/// Путь к каталогу передаётся **рабочим каталогом процесса, а не аргументом**
/// `-C`, и архив зовётся по имени, а не по полному пути. Причина не
/// косметическая: `bsdtar` из System32 собран с ANSI-точкой входа, и Windows
/// перекодирует аргументы командной строки в текущую ANSI-кодовую страницу
/// процесса. Всё, чего в ней нет, превращается в `?`. Учётная запись «Иван»
/// на системе с кодовой страницей 1252 (обычная английская Windows) давала
/// `tar: could not chdir to '...\????\bin'` — и установка ffmpeg не работала
/// вообще, молча и навсегда. Рабочий каталог Rust ставит широким API, и путь
/// границу процесса как текст не пересекает.
///
/// На русской Windows (кодовая страница 1251) ошибка не воспроизводится,
/// поэтому руками её не поймать — только тестом ниже.
fn extract(dir: &Path, archive_name: &str, strip: u8, members: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new(tar_program());
    cmd.current_dir(dir)
        .arg("-xf")
        .arg(archive_name)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    if strip > 0 {
        cmd.arg(format!("--strip-components={strip}"));
    }

    // Асимметрия не случайна: GNU tar (Linux) без `--wildcards` понимает имена
    // членов архива буквально и по шаблону ничего не найдёт, а bsdtar на
    // Windows этого ключа не знает вовсе и падает с ошибкой разбора.
    #[cfg(all(unix, not(target_os = "macos")))]
    cmd.arg("--wildcards");

    cmd.args(members);
    crate::engine::ytdlp::hide_console(&mut cmd);

    let output = cmd.output().map_err(|e| {
        format!(
            "не удалось запустить {} для распаковки: {e}",
            tar_program().display()
        )
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Самая частая поломка на минимальных сборках Linux: GNU tar зовёт
        // внешний xz, а пакета с ним в системе нет.
        if stderr.contains("xz") && stderr.contains("Cannot exec") {
            return Err(
                "не найдена программа xz, нужная для распаковки. Установите пакет xz-utils".into(),
            );
        }
        let tail = stderr.lines().next_back().unwrap_or("").trim();
        return Err(if tail.is_empty() {
            "tar не смог распаковать архив".into()
        } else {
            format!("tar не смог распаковать архив: {tail}")
        });
    }

    Ok(())
}

/// Какой именно `tar` запускать.
///
/// На Windows — строго системный, по абсолютному пути, а **не** `tar` из PATH.
/// Git for Windows, MSYS2 и Cygwin кладут в PATH свой GNU tar, и он оказывается
/// раньше системного: `where tar` на машине с установленным Git выдаёт сначала
/// `C:\Program Files\Git\usr\bin\tar.exe`. Этот GNU tar zip не читает вовсе
/// («This does not look like a tar archive», код 2), поэтому установка ffmpeg
/// молча ломалась бы у всех, у кого стоит Git, — а это почти каждый разработчик.
/// Проверено вживую, а не по документации.
///
/// `%SystemRoot%`, а не жёсткое `C:\Windows`: система не обязана стоять на диске C.
#[cfg(windows)]
fn tar_program() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join("tar.exe")
}

/// На macOS и Linux `tar` из PATH — это то, что нужно: bsdtar и GNU tar
/// соответственно. Подмены, как на Windows, здесь не бывает.
#[cfg(not(windows))]
fn tar_program() -> PathBuf {
    PathBuf::from("tar")
}

/// Имя временного файла архива.
///
/// Расширение берём из ссылки: по нему `tar` определяет формат архива.
/// Номер процесса в имени нужен на случай двух запущенных Savio: без него
/// они писали бы в один и тот же файл и распаковали бы друг другу мусор.
fn archive_tmp_name(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("archive");
    let ext = if tail.ends_with(".tar.xz") {
        ".tar.xz"
    } else {
        ".zip"
    };
    format!(".savio-tmp-{}{ext}", std::process::id())
}

/// Имя временного файла для одиночного бинарника — по той же причине с номером
/// процесса, что и у архива.
fn binary_tmp_name(name: &str) -> String {
    format!(".savio-tmp-{}-{name}", std::process::id())
}

/// Ставит файл на место одним движением.
///
/// Качаем во временный файл и переименовываем: оборванная на середине
/// загрузка иначе оставила бы обрубок, который `locate()` нашёл бы как
/// готовый инструмент — `is_file()` не отличает целый файл от битого.
fn replace(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        let _ = fs::remove_file(to);
    }
    fs::rename(from, to).map_err(|e| {
        format!(
            "не удалось переместить файл в {}: {e}",
            to.display()
        )
    })
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("не удалось сделать файл исполняемым: {e}"))
}

/// На Windows бит исполняемости не нужен — право на запуск определяется ACL.
#[cfg(windows)]
fn make_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn stage(tx: &Sender<Event>, notify: &impl Fn(), text: &str) {
    let _ = tx.send(Event::Stage(text.to_owned()));
    notify();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_extension_follows_url() {
        // Расширение важно: по нему tar выбирает распаковщик.
        assert!(archive_tmp_name("https://x/ffmpeg-linux64-gpl.tar.xz").ends_with(".tar.xz"));
        assert!(archive_tmp_name("https://x/ffmpeg-win64-gpl.zip").ends_with(".zip"));
        // У evermeet расширения в ссылке нет вовсе — там отдаётся zip.
        assert!(archive_tmp_name("https://evermeet.cx/ffmpeg/getrelease/ffmpeg/zip").ends_with(".zip"));
    }

    #[test]
    fn ytdlp_asset_is_self_contained() {
        // Голый `yt-dlp` — zipimport-обёртка, требующая Python в системе.
        // Ассет обязан быть самодостаточным, иначе установка бессмысленна.
        assert_ne!(YTDLP_ASSET, "yt-dlp");
        assert!(!YTDLP_ASSET.ends_with(".zip"));
    }

    /// На Windows распаковщик обязан браться из системного каталога, а не из
    /// PATH: там первым лежит GNU tar от Git, который zip не читает.
    #[cfg(windows)]
    #[test]
    fn tar_is_taken_from_system_directory() {
        let path = tar_program();
        assert!(path.is_absolute(), "путь к tar обязан быть абсолютным");
        assert!(
            path.to_string_lossy().to_lowercase().contains("system32"),
            "ожидался системный tar, получен {}",
            path.display()
        );
    }

    /// Сквозная проверка распаковки на настоящем архиве.
    ///
    /// Стережёт сразу четыре молчаливых поломки: неверную глубину
    /// `--strip-components` (tar при ней возвращает 0 и не распаковывает
    /// ничего), пропущенный `--wildcards` на GNU tar, его же недопустимость
    /// на Windows — и кириллицу в пути.
    ///
    /// Каталог назван по-русски намеренно. Системный `bsdtar` на Windows
    /// получает аргументы в ANSI-кодировке, и на английской системе
    /// (кодовая страница 1252) кириллица в пути превращалась в `????`.
    /// В ASCII-каталоге тест этого не увидит, поэтому путь обязан быть
    /// с кириллицей. Архив лежит там же, где и распаковка, — как в бою.
    #[test]
    fn extract_takes_only_requested_files() {
        const ARCHIVE: &str = "test.tar";
        let pid = std::process::id();

        // Готовим дерево и собираем архив в каталоге без кириллицы: путь к
        // архиву при создании тоже пошёл бы в tar аргументом, и тест падал бы
        // на подготовке, ничего не проверив.
        let ascii_root = std::env::temp_dir().join(format!("savio-fixture-{pid}"));
        let nested = ascii_root.join("pkg-1.0").join("bin");
        fs::create_dir_all(&nested).expect("создать дерево для архива");
        for name in ["ffmpeg-test", "ffprobe-test", "ffplay-test"] {
            fs::write(nested.join(name), name.as_bytes()).expect("создать файл");
        }

        let created = Command::new(tar_program())
            .current_dir(&ascii_root)
            .arg("-cf")
            .arg(ARCHIVE)
            .arg("pkg-1.0")
            .output()
            .expect("запустить tar для создания архива");
        assert!(
            created.status.success(),
            "не удалось создать тестовый архив: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        // А распаковываем уже в каталог с кириллицей — как у пользователя
        // с учётной записью «Иван». Переносим архив средствами Rust: его
        // файловые вызовы работают с широкими путями и не портят имена.
        let out = std::env::temp_dir().join(format!("savio-тест-{pid}"));
        fs::create_dir_all(&out).expect("создать каталог назначения");
        fs::rename(ascii_root.join(ARCHIVE), out.join(ARCHIVE)).expect("перенести архив");

        extract(&out, ARCHIVE, 2, &["*/bin/ffmpeg-test", "*/bin/ffprobe-test"])
            .expect("распаковка обязана пройти");

        assert!(out.join("ffmpeg-test").is_file(), "ffmpeg не распакован");
        assert!(out.join("ffprobe-test").is_file(), "ffprobe не распакован");
        // Лишнее из архива тянуть не надо: шаблоны заданы поимённо.
        assert!(
            !out.join("ffplay-test").exists(),
            "распаковано лишнее — шаблоны членов архива не применились"
        );

        let _ = fs::remove_dir_all(&ascii_root);
        let _ = fs::remove_dir_all(&out);
    }

    /// Настоящая загрузка yt-dlp с GitHub — от резолва выпуска до сверки суммы.
    ///
    /// В обычном прогоне отключена: тест требует сети и тянет несколько
    /// мегабайт, а `cargo test` обязан проходить и без интернета.
    /// Запуск вручную: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "требует доступа в сеть"]
    fn real_ytdlp_download_matches_checksum() {
        let dir = std::env::temp_dir().join("savio-net-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("создать временный каталог");

        let (tx, rx) = std::sync::mpsc::channel();
        let cancelled = AtomicBool::new(false);

        let result = install_ytdlp(&agent(), &dir, &tx, &|| {}, &cancelled);
        drop(tx);
        for event in rx.iter() {
            if let Event::Log(line) = event {
                println!("log: {line}");
            }
        }

        result.expect("установка yt-dlp обязана пройти");
        let installed = dir.join(YTDLP_NAME);
        assert!(installed.is_file(), "yt-dlp не оказался на месте");
        // Временный файл обязан быть переименован, а не оставлен рядом.
        assert!(
            !dir.join(binary_tmp_name("ytdlp")).exists(),
            "остался временный файл"
        );
        let size = fs::metadata(&installed).expect("метаданные").len();
        assert!(size > 1_000_000, "подозрительно маленький файл: {size} байт");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Настоящая загрузка и распаковка ffmpeg.
    ///
    /// Проверяет то, что нельзя проверить синтетикой: что архив по ссылке
    /// действительно того формата, который распаковщик этой ОС понимает, и что
    /// внутри него ожидаемая раскладка. Тянет больше сотни мегабайт, поэтому
    /// тоже под `#[ignore]`.
    #[test]
    #[ignore = "требует доступа в сеть и качает >100 МБ"]
    fn real_ffmpeg_download_and_extract() {
        let dir = std::env::temp_dir().join("savio-ffmpeg-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("создать временный каталог");

        let (tx, rx) = std::sync::mpsc::channel();
        let cancelled = AtomicBool::new(false);

        let result = install_ffmpeg(&agent(), &dir, &tx, &|| {}, &cancelled);
        drop(tx);
        for event in rx.iter() {
            if let Event::Log(line) = event {
                println!("log: {line}");
            }
        }

        result.expect("установка ffmpeg обязана пройти");
        for name in [FFMPEG_NAME, FFPROBE_NAME] {
            let path = dir.join(name);
            assert!(path.is_file(), "{name} не распакован");
            let size = fs::metadata(&path).expect("метаданные").len();
            assert!(size > 1_000_000, "{name} подозрительно мал: {size} байт");
        }
        // Архив после распаковки удаляется — иначе сотня мегабайт осталась бы
        // лежать в каталоге данных навсегда.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("прочитать каталог")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".savio-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "остался мусор: {leftovers:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_missing_when_all_found() {
        // `any()` — единственный признак, по которому UI решает, показывать ли
        // модалку. Пустой `Missing` обязан означать «показывать нечего».
        assert!(!Missing::default().any());
        assert!(Missing { ytdlp: true, ffmpeg: false }.any());
        assert!(Missing { ytdlp: false, ffmpeg: true }.any());
    }
}
