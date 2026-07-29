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

use super::binaries::{self, FFMPEG_NAME, FFPROBE_NAME, Origin, YTDLP_NAME};
use super::sha256::{self, Sha256};
use crate::model::{Event, NO_DOWNLOAD, Progress, ToolVersion, ToolVersions};

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
    ///
    /// `sums` — файл контрольных сумм того же выпуска. Есть он не у всех
    /// источников, поэтому лежит здесь, а не общей константой.
    Bundle {
        url: &'static str,
        sums: &'static str,
    },
    /// Два плоских архива, по одному на программу.
    ///
    /// Контрольных сумм у этих источников нет: evermeet.cx выкладывает только
    /// отделённую GPG-подпись (проверять её нечем — нужен `gpg` и ключ автора,
    /// а зависимость ради этого противоречит минимальному набору), а у
    /// martin-riedl.de файл `<архив>.sha256` лежит рядом с настоящим архивом,
    /// но не с адресом `redirect/latest/…`, которым мы качаем: путь к нему
    /// известен только из заголовка `Location`. Проверено вживую: по
    /// `redirect/`-адресу сумма отдаёт 404 пять попыток подряд.
    Split {
        ffmpeg: &'static str,
        ffprobe: &'static str,
    },
}

/// Контрольные суммы сборок BtbN — один файл на весь выпуск, формат `sha256sum`.
///
/// Тег `latest` подвижный: BtbN перезаливает ассеты каждый день, поэтому сумму
/// нельзя зашить в код — через сутки константа стала бы ложным отказом. Берём
/// её из того же выпуска, что и архив.
#[cfg(any(windows, target_os = "linux"))]
const BTBN_SUMS: &str =
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/checksums.sha256";

/// Источники ffmpeg по порядку предпочтения: первый рабочий выигрывает.
///
/// Список, а не одна ссылка, потому что все источники здесь — чужие серверы,
/// и любой из них может отдать 404 или лечь. Раньше единственный сбойный адрес
/// означал, что ffmpeg не поставится вовсе; теперь это лишь повод взять
/// следующий. Полный провал по-прежнему не фатален — установка ffmpeg
/// заканчивается предупреждением, а не ошибкой (см. `run`).
///
/// На каждой ОС свой список, поэтому имя одно, а определений несколько.
// Windows и Linux — сборки BtbN, там оба бинарника лежат в одном архиве.
// Windows получает .zip, Linux — .tar.xz: см. `extract`.
#[cfg(all(windows, target_arch = "x86_64"))]
const FFMPEG_SOURCES: &[FfmpegSource] = &[FfmpegSource::Bundle {
    url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
    sums: BTBN_SUMS,
}];
#[cfg(all(windows, target_arch = "aarch64"))]
const FFMPEG_SOURCES: &[FfmpegSource] = &[FfmpegSource::Bundle {
    url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-winarm64-gpl.zip",
    sums: BTBN_SUMS,
}];
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const FFMPEG_SOURCES: &[FfmpegSource] = &[FfmpegSource::Bundle {
    url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-gpl.tar.xz",
    sums: BTBN_SUMS,
}];
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const FFMPEG_SOURCES: &[FfmpegSource] = &[FfmpegSource::Bundle {
    url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linuxarm64-gpl.tar.xz",
    sums: BTBN_SUMS,
}];
// Под macOS готовых сборок «всё в одном» нет: BtbN её не собирает, а у
// оставшихся источников ffmpeg и ffprobe разложены по отдельным архивам.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const FFMPEG_SOURCES: &[FfmpegSource] = &[
    FfmpegSource::Split {
        ffmpeg: "https://evermeet.cx/ffmpeg/getrelease/ffmpeg/zip",
        ffprobe: "https://evermeet.cx/ffmpeg/getrelease/ffprobe/zip",
    },
    // Запасной — тот же сервер, что основной у ARM: сборки Мартина Ридля
    // покрывают обе архитектуры macOS.
    FfmpegSource::Split {
        ffmpeg: "https://ffmpeg.martin-riedl.de/redirect/latest/macos/amd64/release/ffmpeg.zip",
        ffprobe: "https://ffmpeg.martin-riedl.de/redirect/latest/macos/amd64/release/ffprobe.zip",
    },
];
// Apple Silicon. Основной источник — ffmpeg.martin-riedl.de: в ссылке **нет
// номера версии**, сервер сам отдаёт текущую сборку. Именно этим он лучше
// osxexperts.net, где адрес вида `ffmpeg81arm.zip` намертво зашивает версию
// 8.1 и превращается в 404 в день, когда автор выкладывает 8.2, — а узнать
// об этом можно только от пользователя, у которого установка молча отвалилась.
//
// Проверено на скачанном файле, а не по описанию сайта: архив плоский (одна
// запись `ffmpeg` в корне, отсюда `strip = 0`), внутри Mach-O с cputype
// 0x0100000C, то есть настоящий arm64, а не Intel под Rosetta. Все динамические
// зависимости — только `/usr/lib/*`, которые есть на любой macOS: на Homebrew
// сборка не завязана и работает на чистой системе.
//
// osxexperts.net остаётся запасным: он живой и его стоит держать на случай,
// если основной ляжет, — но первым его ставить нельзя из-за версии в адресе.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const FFMPEG_SOURCES: &[FfmpegSource] = &[
    FfmpegSource::Split {
        ffmpeg: "https://ffmpeg.martin-riedl.de/redirect/latest/macos/arm64/release/ffmpeg.zip",
        ffprobe: "https://ffmpeg.martin-riedl.de/redirect/latest/macos/arm64/release/ffprobe.zip",
    },
    FfmpegSource::Split {
        ffmpeg: "https://www.osxexperts.net/ffmpeg81arm.zip",
        ffprobe: "https://www.osxexperts.net/ffprobe81arm.zip",
    },
];

/// Размер чанка при чтении из сети. 64 КБ — компромисс: меньше даёт лишние
/// системные вызовы, больше — рваный прогресс на медленном канале.
const CHUNK: usize = 64 * 1024;

/// Как часто отдавать прогресс в UI. Каждый чанк — это сотни событий в секунду
/// на быстром канале, и все они приводят к перерисовке окна.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Сколько раз пытаться получить ответ, прежде чем считать источник мёртвым.
///
/// Это не перестраховка «на всякий случай». У `ffmpeg.martin-riedl.de` за
/// балансировщиком стоит узел, который отвечает 404 на совершенно правильный
/// адрес: шесть HEAD-запросов подряд по одной ссылке дали 200, 404, 200, 404,
/// 200, 404 — строго через раз, а GET следом прошёл и вернул целый архив.
/// Проверено вживую, а не предположено. Без повтора примерно половина
/// пользователей Apple Silicon оставалась бы без ffmpeg, причём каждый раз
/// с разным исходом — то есть с неповторяемой жалобой.
///
/// Четырёх попыток хватает: вероятность попасть на битый узел четыре раза
/// подряд — около одной шестнадцатой, а платим мы за это лишь парой секунд
/// в том случае, когда сервер и правда лежит.
const REQUEST_ATTEMPTS: u32 = 4;

/// Пауза между попытками. Короткая намеренно: сбой здесь мгновенный (чужой
/// узел отвечает сразу), и растягивать паузу значит просто заставлять
/// пользователя смотреть на неподвижную полосу.
const RETRY_PAUSE: Duration = Duration::from_millis(700);

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
            // Установка — не элемент очереди, номера у неё нет и быть не
            // может: её события ходят по своему приёмнику, и разводить там
            // нечего. Тот же `NO_DOWNLOAD` стоит и у обновления движка ниже.
            Err(err) => {
                let _ = tx.send(Event::Failed {
                    id: NO_DOWNLOAD,
                    message: err,
                });
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
    install_ytdlp_tag(agent, dir, &tag, tx, notify, cancelled)
}

/// Ставит заранее известный выпуск.
///
/// Отделено от `install_ytdlp` ради обновления: там выпуск уже разрешён —
/// его номер нужен, чтобы сравнить с установленной версией и не качать
/// двадцать мегабайт впустую, когда обновляться не на что.
fn install_ytdlp_tag(
    agent: &ureq::Agent,
    dir: &Path,
    tag: &str,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<(), String> {
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

/// Ставит ffmpeg, перебирая источники по очереди до первого удачного.
///
/// Ошибка одного источника — не отказ: он лишь вычёркивается, и берётся
/// следующий. Наружу ошибка уходит, только когда кончились все, и даже тогда
/// это предупреждение, а не срыв установки (см. `run`).
///
/// Отмена — исключение: перебирать источники дальше после неё нельзя, иначе
/// нажатие «Отменить» приводило бы не к остановке, а к загрузке со следующего
/// сервера.
fn install_ffmpeg(
    agent: &ureq::Agent,
    dir: &Path,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut last = String::new();

    for (index, source) in FFMPEG_SOURCES.iter().enumerate() {
        match install_ffmpeg_from(source, agent, dir, tx, notify, cancelled) {
            Ok(()) => {
                let _ = tx.send(Event::Log(format!(
                    "ffmpeg установлен: {}",
                    dir.join(FFMPEG_NAME).display()
                )));
                notify();
                return Ok(());
            }
            Err(err) => {
                if cancelled.load(Ordering::Relaxed) {
                    return Err(err);
                }
                // Незавершённая попытка обязана убрать за собой: иначе от
                // первого источника остался бы один ffmpeg без ffprobe, и
                // проверка существования у следующего приняла бы разнородную
                // пару за целую установку.
                for name in [FFMPEG_NAME, FFPROBE_NAME] {
                    let _ = fs::remove_file(dir.join(name));
                }
                let _ = tx.send(Event::Log(format!(
                    "Источник ffmpeg №{} не сработал: {err}",
                    index + 1
                )));
                notify();
                last = err;
            }
        }
    }

    Err(if last.is_empty() {
        "не задан ни один источник ffmpeg".into()
    } else {
        // Число источников важно в сообщении: без него «не удалось скачать»
        // читается как «сайт полежал минуту», хотя перепробовано было всё.
        format!(
            "перепробованы все источники ({}), последняя ошибка — {last}",
            FFMPEG_SOURCES.len()
        )
    })
}

/// Ожидаемая сумма архива, если её удалось получить.
///
/// `None` здесь означает «сверять не с чем», и это **не** повод отказаться от
/// установки. Причина в асимметрии последствий: на Windows и Linux источник
/// ffmpeg ровно один, и превращение недоступного списка сумм в ошибку лишало
/// бы человека ffmpeg целиком из-за того, что у чужого сервера переименовался
/// служебный файл. Распакованное всё равно проверяется существованием файлов
/// (`tar` возвращает 0 и на пустой распаковке), а про пропущенную сверку
/// говорит журнал. Настоящее несовпадение суммы — совсем другое дело: оно
/// означает битый или подменённый архив, и вот оно источник отвергает.
fn expected_sum(
    agent: &ureq::Agent,
    sums: &str,
    url: &str,
    tx: &Sender<Event>,
) -> Option<String> {
    let asset = url.rsplit('/').next().unwrap_or_default();

    let list = match fetch_text(agent, sums) {
        Ok(list) => list,
        Err(err) => {
            let _ = tx.send(Event::Log(format!(
                "Список контрольных сумм ffmpeg недоступен ({err}) — ставлю без сверки."
            )));
            return None;
        }
    };

    // Имя ищем целиком, а не подстрокой: в том же списке лежит
    // `…-win64-gpl-shared.zip`, и поиск «по вхождению» брал бы сумму от него.
    // `find_sum` сравнивает имена на равенство — на это и опираемся.
    let found = sha256::find_sum(&list, asset).map(str::to_owned);
    if found.is_none() {
        let _ = tx.send(Event::Log(format!(
            "В списке контрольных сумм ffmpeg нет строки для {asset} — ставлю без сверки."
        )));
    }
    found
}

fn install_ffmpeg_from(
    source: &FfmpegSource,
    agent: &ureq::Agent,
    dir: &Path,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<(), String> {
    stage(tx, notify, "Скачиваю ffmpeg…");

    match *source {
        FfmpegSource::Bundle { url, sums } => {
            // Сумму спрашиваем до загрузки: качать сто с лишним мегабайт,
            // чтобы потом выяснить, что сверять их не с чем, незачем.
            let expected = expected_sum(agent, sums, url, tx);

            let name = archive_tmp_name(url);
            let digest = download(agent, url, &dir.join(&name), tx, notify, cancelled)?;

            // Несовпадение — отказ этого источника, а не всей установки:
            // выше по стеку `install_ffmpeg` возьмётся за следующий.
            if let Some(expected) = expected
                && sha256::hex(&digest) != expected
            {
                let _ = fs::remove_file(dir.join(&name));
                return Err(
                    "скачанный архив ffmpeg повреждён: контрольная сумма не совпала".into(),
                );
            }

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
    // Здесь же проходит граница «источник сработал»: пара найдена целиком,
    // значит можно не трогать остальные.
    for name in [FFMPEG_NAME, FFPROBE_NAME] {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(format!(
                "после распаковки не найден {name}: содержимое архива отличается от ожидаемого"
            ));
        }
        make_executable(&path)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Версии установленного
// ---------------------------------------------------------------------------

/// Спрашивает версии установленных инструментов в отдельном потоке.
///
/// Отдельный поток обязателен: `yt-dlp --version` — это запуск замороженной
/// PyInstaller-сборки, и отвечает она не мгновенно (десятые доли секунды, а на
/// холодном диске и с антивирусом — заметно дольше). В кадре отрисовки такому
/// места нет (Правило 1).
///
/// Ручки, как у установки, здесь нет: обе программы просто печатают строку и
/// выходят, отменять нечего. Ошибок наружу тоже не отдаём — их место в самом
/// `ToolVersions`: «не найдена» и «версию узнать не вышло» это не сбой, а
/// то, что надо показать.
pub fn start_versions(tx: Sender<Event>, notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let versions = ToolVersions {
            ytdlp: describe(binaries::locate(YTDLP_NAME).as_deref(), ytdlp_version),
            ffmpeg: describe(binaries::locate(FFMPEG_NAME).as_deref(), ffmpeg_version),
        };
        let _ = tx.send(Event::Versions(versions));
        notify();
    });
}

fn describe(path: Option<&Path>, read: impl Fn(&Path) -> Option<String>) -> ToolVersion {
    match path {
        None => ToolVersion::Missing,
        Some(path) => match read(path) {
            Some(version) => ToolVersion::Known(version),
            None => ToolVersion::Unknown,
        },
    }
}

/// Версия установленного ffmpeg — то, что он печатает первой строкой
/// `ffmpeg -version`.
///
/// `None` — «спросить не удалось»: не запустился, вышел с ошибкой или напечатал
/// что-то, в чём версии не видно. Отказом это не считается нигде: ffmpeg при
/// этом исправен, а не узнанная строка стоит подписи «версия неизвестна», но
/// никак не блокировки кнопки (Правило 2).
fn ffmpeg_version(path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.arg("-version")
        .stdout(Stdio::piped())
        // stderr, в отличие от `ytdlp_version`, забираем: `-version` печатает
        // в stdout у всех проверенных сборок, но ffmpeg вообще-то шлёт свой
        // баннер в stderr, и сборок его в мире несколько десятков. Проверить
        // каждую нельзя, а запасной поток стоит одной строки.
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    crate::engine::ytdlp::hide_console(&mut cmd);

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.as_ref(), stderr.as_ref()]
        .into_iter()
        .find_map(|text| parse_version_line(text.lines().next().unwrap_or_default()))
        .map(str::to_owned)
}

/// Сколько символов версии считаем правдоподобными.
///
/// Строку печатает чужая программа, и её вывод — не наш контракт: увидев
/// вместо версии абзац текста, показывать его в окне не нужно. Самая длинная
/// настоящая версия из встреченных — `N-125365-g9a01c1cb6a-20260630`, 29 знаков.
const VERSION_LIMIT: usize = 64;

/// Вырезает версию из первой строки вывода `-version`.
///
/// Приметой служит **слово `version`, за которым идёт токен**:
/// ```text
/// ffmpeg  version 8.1.2-full_build-www.gyan.dev  Copyright (c) 2000-2026 …
/// ffprobe version N-125365-g9a01c1cb6a-20260630  Copyright (c) 2007-2026 …
/// ```
/// Всё остальное, что напрашивается, проверено вживую и промахивается:
///
/// - `starts_with("ffmpeg version")` — у ffprobe первое слово другое, и та же
///   функция на нём молча вернула бы `None`;
/// - опора на `Copyright (c) 2000-` — у ffprobe там 2007, общее только слово;
/// - шаблон `X.Y.Z` — git-сборка печатает `N-125365-g9a01c1cb6a-20260630`,
///   точечного номера в ней нет вовсе, а ставится она обычным `winget`;
/// - обрезка токена по первому дефису «ради красоты» — `8.1.2-full_build…`
///   она укоротила бы до `8.1.2`, а `N-125365-…` до одинокой буквы `N`.
///
/// Поэтому токен берётся целиком и как есть, а укорачивается только на экране.
fn parse_version_line(line: &str) -> Option<&str> {
    let mut words = line.split_whitespace();
    words.by_ref().find(|word| *word == "version")?;
    words.next().filter(|version| version.len() <= VERSION_LIMIT)
}

// ---------------------------------------------------------------------------
// Обновление
// ---------------------------------------------------------------------------

/// Версия установленного yt-dlp — то, что он сам печатает по `--version`.
///
/// Формат совпадает с тегом выпуска на GitHub (`2026.07.04`), поэтому строки
/// можно сравнивать напрямую, не разбирая на числа. Если это когда-нибудь
/// перестанет быть правдой, худшее следствие — лишняя загрузка уже имеющейся
/// версии, а не поломка.
///
/// `None` означает «спросить не удалось»: файла нет, он битый или не
/// запускается. Это не ошибка — просто сравнивать будет не с чем.
fn ytdlp_version(path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    crate::engine::ytdlp::hide_console(&mut cmd);

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!version.is_empty()).then_some(version)
}

/// Чем пользователю обновлять чужую копию. У каждой ОС свой менеджер, и
/// совет «обновите пакет» без имени команды бесполезен.
#[cfg(windows)]
const YTDLP_SYSTEM_HINT: &str = "winget upgrade yt-dlp";
#[cfg(target_os = "macos")]
const YTDLP_SYSTEM_HINT: &str = "brew upgrade yt-dlp";
#[cfg(all(unix, not(target_os = "macos")))]
const YTDLP_SYSTEM_HINT: &str = "менеджером пакетов вашей системы";

#[cfg(windows)]
const FFMPEG_SYSTEM_HINT: &str = "winget upgrade ffmpeg";
#[cfg(target_os = "macos")]
const FFMPEG_SYSTEM_HINT: &str = "brew upgrade ffmpeg";
#[cfg(all(unix, not(target_os = "macos")))]
const FFMPEG_SYSTEM_HINT: &str = "менеджером пакетов вашей системы";

/// Что обновляем по кнопке.
///
/// Одно перечисление на две кнопки, а не две отдельные функции, потому что
/// различаются они только адресами и текстами: поток, ручка отмены, канал
/// событий и разбор исхода у них общие до последней строки.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Component {
    Ytdlp,
    Ffmpeg,
}

impl Component {
    /// Имя для сообщений — ровно то, которым программу зовут в жизни.
    fn name(self) -> &'static str {
        match self {
            Component::Ytdlp => "yt-dlp",
            Component::Ffmpeg => "ffmpeg",
        }
    }
}

/// Запускает обновление выбранного инструмента в отдельном потоке.
///
/// Заменяем **только свою** копию — ту, что Savio скачал в каталог данных.
/// Копию из PATH или лежащую рядом с exe не трогаем: первая принадлежит
/// пакетному менеджеру и подмена файла рассинхронизировала бы его с системой,
/// вторая — часть портативной поставки. В обоих случаях честнее сказать,
/// откуда взят файл и чем его обновить, чем сделать вид, что обновили.
///
/// Скачать свежую копию в каталог данных «про запас» тоже нельзя: каталог
/// данных в порядке поиска последний, работать продолжила бы прежняя версия,
/// и кнопка стала бы ровно той молчаливой пустышкой, ради которой её и завели.
pub fn start_update(
    what: Component,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) -> Handle {
    let cancelled = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        cancelled: Arc::clone(&cancelled),
    };

    std::thread::spawn(move || {
        match run_update(what, &tx, &notify, &cancelled) {
            Ok(Outcome::Updated(text)) => {
                let _ = tx.send(Event::Notice(text));
                let _ = tx.send(Event::Ready);
            }
            // Отказ трогать чужой файл — не сбой: делать было нечего, и всё
            // работает как прежде. Показать это красной «Ошибкой» значило бы
            // напугать человека там, где приложение поступило правильно;
            // `Warning` ровно для такого и заведён — «можно, но с оговоркой».
            Ok(Outcome::Declined(text)) => {
                let _ = tx.send(Event::Warning(text));
                let _ = tx.send(Event::Ready);
            }
            Err(err) => {
                let _ = tx.send(Event::Failed {
                    id: NO_DOWNLOAD,
                    message: err,
                });
            }
        }
        notify();
    });

    handle
}

/// Чем кончилось обновление, если оно не сорвалось.
enum Outcome {
    /// Своя копия обновлена, поставлена заново или уже была свежей.
    Updated(String),
    /// Копия чужая — не тронули и объяснили, чем её обновить.
    Declined(String),
}

fn run_update(
    what: Component,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<Outcome, String> {
    let dir = binaries::data_dir()
        .ok_or("Не удалось определить папку для инструментов: не задана домашняя папка.")?;

    // Смотрим на ту программу, которую и обновляем: у ffmpeg своё
    // происхождение, и системный ffmpeg при своём yt-dlp — обычное дело.
    let name = match what {
        Component::Ytdlp => YTDLP_NAME,
        Component::Ffmpeg => FFMPEG_NAME,
    };
    let found = binaries::locate_with_origin(name);

    // Чужую копию не трогаем — объясняем, откуда она и чем её обновить.
    if let Some((path, origin @ (Origin::System | Origin::Portable))) = &found {
        return Ok(Outcome::Declined(declined(what, path, *origin)));
    }

    fs::create_dir_all(&dir)
        .map_err(|e| format!("Не удалось создать папку {}: {e}", dir.display()))?;

    let agent = agent();
    let installed = found.as_ref().map(|(path, _)| path.as_path());

    match what {
        Component::Ytdlp => update_ytdlp(&agent, &dir, installed, tx, notify, cancelled),
        Component::Ffmpeg => update_ffmpeg(&agent, &dir, installed, tx, notify, cancelled),
    }
}

/// Текст отказа трогать чужую копию.
///
/// Общий на оба инструмента: беда одна и та же, меняются только имя и совет.
/// Путь в сообщении обязателен — без него «установлен в системе» проверить
/// нечем, а человек как раз и хочет понять, какой именно файл у него работает.
fn declined(what: Component, path: &Path, origin: Origin) -> String {
    let name = what.name();
    match origin {
        Origin::System => {
            let hint = match what {
                Component::Ytdlp => YTDLP_SYSTEM_HINT,
                Component::Ffmpeg => FFMPEG_SYSTEM_HINT,
            };
            format!(
                "{name} установлен в системе, а не Savio:\n{}\n\n\
                 Обновите его так же, как ставили: {hint}. \
                 Savio подменять чужой файл не станет — иначе он разойдётся \
                 с пакетным менеджером.",
                path.display()
            )
        }
        Origin::Portable => format!(
            "{name} лежит рядом с Savio:\n{}\n\n\
             Это портативная поставка — обновите её целиком или замените \
             этот файл вручную. Savio его не трогает, чтобы не сломать сборку.",
            path.display()
        ),
        Origin::Owned => unreachable!("своя копия обновляется, а не объясняется"),
    }
}

/// Перекачивает ffmpeg поверх своей копии.
///
/// Проверки «а не стоит ли уже последняя» здесь нет, и это не упущение.
/// Основной источник (сборки BtbN для Windows и Linux) выпускается тегом
/// `latest`, и **узнать версию, не скачав архив, невозможно**: проверено по
/// ответу GitHub API — `body` пуст, `label` у всех ассетов пуст, в имени файла
/// стоит слово `master`, а не номер, и внутри архива корневая папка тоже
/// называется `ffmpeg-master-latest-…`. Запоминать сборку с прошлого раза
/// нельзя по той же причине, по которой версия yt-dlp спрашивается у самого
/// бинарника: запомненное разъезжается с настоящим, если файл подменили снаружи.
///
/// Поэтому кнопка честно перекачивает архив целиком (около 160 МБ), а
/// сравнение версий происходит уже после — по нему и различаются сообщения
/// «обновлён» и «на сервере та же сборка». Говорить об этом заранее — дело UI.
fn update_ffmpeg(
    agent: &ureq::Agent,
    dir: &Path,
    installed: Option<&Path>,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<Outcome, String> {
    // Версию спрашиваем до перекачки: после неё файл уже другой.
    let current = installed.and_then(ffmpeg_version);

    // Стадию «Скачиваю ffmpeg…» ставит сам `install_ffmpeg`.
    install_ffmpeg(agent, dir, tx, notify, cancelled)
        .map_err(|err| format!("Не удалось обновить ffmpeg: {err}."))?;

    let fresh = ffmpeg_version(&dir.join(FFMPEG_NAME));

    Ok(Outcome::Updated(match fresh {
        Some(fresh) => update_summary(
            "ffmpeg",
            current.as_deref(),
            &fresh,
            // После перекачки сотни мегабайт одно «уже последней версии»
            // читается как «мы проверили и качать не стали» — а качали.
            " На сервере лежит та же сборка.",
        ),
        // Версию не прочитали — но файлы на месте: `install_ffmpeg` проверяет
        // это существованием, а не кодом возврата. Обещать номер, которого мы
        // не знаем, нельзя, а промолчать после нажатия кнопки — тем более.
        None => "ffmpeg скачан заново. Версию узнать не вышло.".to_owned(),
    }))
}

/// Итог обновления словами.
///
/// **Стрелки `→` здесь нет намеренно, и вернуть её нельзя.** Шрифты, которые
/// eframe кладёт в сборку по умолчанию, знака U+2192 не содержат, и вместо
/// него в окне рисуется пустой прямоугольник: «обновлён: 8.1.2 □ N-125829».
/// Проверено глазами на Windows 11 — ни сборка, ни `clippy`, ни тесты этого
/// не видят, а показывается это только после удачного обновления, то есть
/// в сценарии, который вручную повторяют редко. Стрелка тут напрашивается
/// сама, поэтому и предупреждение, и тест ниже.
fn update_summary(name: &str, current: Option<&str>, fresh: &str, unchanged: &str) -> String {
    match current {
        Some(current) if current == fresh => {
            format!("{name} уже последней версии ({fresh}).{unchanged}")
        }
        Some(current) => format!("{name} обновлён: было {current}, стало {fresh}."),
        // Версии до обновления не было — значит, это первая установка своей
        // копии, а не обновление. Обещать «обновлено с …» здесь нечестно.
        None => format!("{name} установлен, версия {fresh}."),
    }
}

fn update_ytdlp(
    agent: &ureq::Agent,
    dir: &Path,
    installed: Option<&Path>,
    tx: &Sender<Event>,
    notify: &impl Fn(),
    cancelled: &AtomicBool,
) -> Result<Outcome, String> {
    stage(tx, notify, "Ищу свежий выпуск yt-dlp…");
    let tag = latest_ytdlp_tag(agent)?;

    // Версию спрашиваем у самого бинарника, а не запоминаем при установке:
    // запомненная разъехалась бы с настоящей, если файл подменили снаружи.
    let current = installed.and_then(ytdlp_version);

    // Сравниваем строки, а не числа. Nightly-сборка (`2026.07.14.233956`)
    // со стабильным тегом никогда не совпадёт, и такую копию мы заменим
    // стабильной. Это осознанный размен: разбирать версии ради редкого случая
    // сложнее, чем один лишний раз скачать, а ошибка здесь безопасна в обе
    // стороны — худшее следствие — загрузка того, что уже есть.
    if let Some(current) = &current
        && current == &tag
    {
        // Оговорки «на сервере та же сборка» здесь нет и не нужно: у yt-dlp
        // версия узнаётся до загрузки, и мы честно ничего не качали.
        return Ok(Outcome::Updated(update_summary(
            "yt-dlp",
            Some(current),
            &tag,
            "",
        )));
    }

    // Стадию «Скачиваю yt-dlp…» ставит сам `install_ytdlp_tag` — здесь её
    // дублировать не надо.
    let _ = tx.send(Event::Log(format!("yt-dlp: выпуск {tag}")));
    install_ytdlp_tag(agent, dir, &tag, tx, notify, cancelled)?;

    Ok(Outcome::Updated(update_summary(
        "yt-dlp",
        current.as_deref(),
        &tag,
        "",
    )))
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

/// GET с повтором при неудаче.
///
/// Повторяем в том числе на 404: ровно им и отвечает битый узел балансировщика
/// (см. `REQUEST_ATTEMPTS`). Настоящий, честный 404 — например, когда
/// osxexperts.net поднял версию и старый адрес умер, — тоже пройдёт все
/// попытки, но заплатим мы за это лишь двумя секундами, а различить эти два
/// случая по ответу невозможно.
///
/// `cancelled` проверяем **между** попытками: без этого нажатие «Отменить»
/// на мёртвом сервере не давало бы никакого эффекта, пока не выйдут все
/// попытки со всеми паузами.
fn get_with_retry(
    agent: &ureq::Agent,
    url: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut last = String::new();

    for attempt in 1..=REQUEST_ATTEMPTS {
        if cancelled.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err("Установка отменена.".into());
        }

        match agent.get(url).call() {
            Ok(response) => return Ok(response),
            Err(err) => {
                last = human_net_error(&err);
                if attempt < REQUEST_ATTEMPTS {
                    std::thread::sleep(RETRY_PAUSE);
                }
            }
        }
    }

    Err(last)
}

fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String, String> {
    let mut body = get_with_retry(agent, url, None)?.into_body();
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

    let response = get_with_retry(agent, url, Some(cancelled))?;
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
        download_id: NO_DOWNLOAD,
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

    /// Источник обязан быть хотя бы один, иначе ffmpeg не поставится никогда,
    /// а сообщать об этом будет некому: сбой ffmpeg — всего лишь предупреждение.
    #[test]
    fn ffmpeg_has_at_least_one_source() {
        assert!(!FFMPEG_SOURCES.is_empty());
    }

    /// Основной источник для Apple Silicon не должен зашивать версию в адрес.
    ///
    /// Ровно на этом всё и ломалось: `ffmpeg81arm.zip` живёт до дня, когда
    /// автор выложит 8.2, и превращается в 404 без единого предупреждения.
    /// Ссылка без номера версии — единственное, что защищает от повтора,
    /// поэтому проверяем машинно, а не «помним».
    /// Кросс-компиляции под macOS на машине разработчика обычно нет (сборочному
    /// скрипту зависимости нужен C-компилятор под цель), поэтому этот тест
    /// впервые выполняется на CI. Написан нарочно скучно — без хитрых образцов
    /// и выводов типов: ошибка компиляции здесь всплыла бы уже в CI, а не тут.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn macos_arm_primary_source_has_no_version_in_url() {
        let urls: Vec<&str> = match &FFMPEG_SOURCES[0] {
            FfmpegSource::Split { ffmpeg, ffprobe } => vec![*ffmpeg, *ffprobe],
            FfmpegSource::Bundle(url) => vec![*url],
        };

        for url in urls {
            let file = url.rsplit('/').next().unwrap_or_default();
            assert!(
                !file.chars().any(|c| c.is_ascii_digit()),
                "в имени файла основного источника зашита версия: {url}"
            );
        }

        // Запасной источник обязан быть: основной — чужой сервер, и он падает.
        assert!(
            FFMPEG_SOURCES.len() >= 2,
            "у macOS ARM обязан быть запасной источник ffmpeg"
        );
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
    /// действительно того формата, который распаковщик этой ОС понимает, что
    /// внутри него ожидаемая раскладка и что сумму архива вообще есть с чем
    /// сверить. Тянет больше сотни мегабайт, поэтому тоже под `#[ignore]`.
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
        let mut log = Vec::new();
        for event in rx.iter() {
            if let Event::Log(line) = event {
                println!("log: {line}");
                log.push(line);
            }
        }

        result.expect("установка ffmpeg обязана пройти");

        // Сверка суммы обязана была состояться, а не быть пропущенной.
        //
        // Пропуск здесь — молчаливый (`expected_sum` возвращает `None`, и
        // установка идёт дальше как ни в чём не бывало), и он ровно тот
        // случай, ради которого написано Правило 6: переименуй BtbN свой
        // `checksums.sha256` — и мы перестанем проверять архив вообще, не
        // получив ни ошибки сборки, ни падения теста. Ловится это только
        // здесь, на живом источнике.
        //
        // На macOS сверять нечем в принципе (см. `FfmpegSource::Split`),
        // поэтому спрашиваем только там, где сумма обещана.
        #[cfg(any(windows, target_os = "linux"))]
        assert!(
            !log.iter().any(|line| line.contains("без сверки")),
            "архив ffmpeg поставился без сверки контрольной суммы: {log:?}"
        );

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

    /// Разбор версии на дословных первых строках настоящих сборок.
    ///
    /// Строки сняты с живых бинарников, а не выдуманы: точечная версия
    /// (`8.1.2-…`) и git-сборка (`N-125365-…`) устроены по-разному, и правило
    /// обязано брать обе. Промах здесь ничего не ломает при сборке и не виден
    /// в тестах остального — просто версия перестаёт показываться.
    #[test]
    fn version_is_taken_from_real_banner_lines() {
        let cases = [
            (
                "ffmpeg version 8.1.2-full_build-www.gyan.dev Copyright (c) 2000-2026 the FFmpeg developers",
                "8.1.2-full_build-www.gyan.dev",
            ),
            // ffprobe — не ffmpeg: привязка к первому слову промахнулась бы.
            (
                "ffprobe version 8.1.2-full_build-www.gyan.dev Copyright (c) 2007-2026 the FFmpeg developers",
                "8.1.2-full_build-www.gyan.dev",
            ),
            // Git-сборка: точечного номера нет вовсе, а ставит её обычный winget.
            (
                "ffmpeg version N-125365-g9a01c1cb6a-20260630 Copyright (c) 2000-2026 the FFmpeg developers",
                "N-125365-g9a01c1cb6a-20260630",
            ),
            // Сборка martin-riedl — та, что качается на macOS ARM.
            (
                "ffmpeg version 8.1.2-https://www.martin-riedl.de Copyright (c) 2000-2026 the FFmpeg developers",
                "8.1.2-https://www.martin-riedl.de",
            ),
            // Дистрибутивная сборка Debian/Ubuntu: после версии идёт ещё и
            // название пакета, но версия по-прежнему сразу за словом.
            (
                "ffmpeg version 7.1.1-1ubuntu1 Copyright (c) 2000-2025 the FFmpeg developers",
                "7.1.1-1ubuntu1",
            ),
        ];

        for (line, expected) in cases {
            assert_eq!(parse_version_line(line), Some(expected), "строка: {line}");
        }
    }

    /// Неузнанный вывод обязан давать `None`, а не мусор в окне.
    ///
    /// Формат печатает чужая программа, и промах разбора рано или поздно
    /// случится. Показать вместо версии кусок чужого текста хуже, чем честное
    /// «версия неизвестна», — и втрое хуже, если этот текст ещё и длинный.
    #[test]
    fn unrecognised_output_gives_no_version() {
        // Слова `version` нет вовсе.
        assert_eq!(parse_version_line("ffmpeg 8.1.2"), None);
        // Слово есть, но за ним ничего.
        assert_eq!(parse_version_line("ffmpeg version"), None);
        assert_eq!(parse_version_line(""), None);
        // Подстрокой слово не считается: `versioning` — не `version`.
        assert_eq!(parse_version_line("ffmpeg versioning info 8.1"), None);
        // Неправдоподобно длинный токен — не версия, а чужой текст.
        let long = format!("ffmpeg version {}", "x".repeat(VERSION_LIMIT + 1));
        assert_eq!(parse_version_line(&long), None);
    }

    /// Отказ трогать чужую копию обязан называть **ту** программу, которую
    /// нажали, и советовать команду для неё же.
    ///
    /// Совет не про ту программу — та же беда, о которой предупреждает
    /// `explain_failure`: человек уходит чинить не своё. Перепутать здесь легко
    /// (текст один на два инструмента), а компилятор этого не поймает.
    #[test]
    fn decline_names_the_right_tool() {
        let path = Path::new("/usr/bin/tool");

        let ytdlp = declined(Component::Ytdlp, path, Origin::System);
        assert!(ytdlp.contains("yt-dlp"), "не назван yt-dlp: {ytdlp}");
        assert!(!ytdlp.contains("ffmpeg"), "совет про чужую программу: {ytdlp}");
        assert!(ytdlp.contains(YTDLP_SYSTEM_HINT), "нет команды обновления");

        let ffmpeg = declined(Component::Ffmpeg, path, Origin::System);
        assert!(ffmpeg.contains("ffmpeg"), "не назван ffmpeg: {ffmpeg}");
        assert!(
            !ffmpeg.contains("yt-dlp"),
            "совет про чужую программу: {ffmpeg}"
        );
        assert!(ffmpeg.contains(FFMPEG_SYSTEM_HINT), "нет команды обновления");

        // Путь обязателен в обоих случаях: без него «установлен в системе»
        // проверить нечем.
        for text in [
            declined(Component::Ytdlp, path, Origin::Portable),
            declined(Component::Ffmpeg, path, Origin::Portable),
        ] {
            assert!(text.contains("tool"), "потерян путь к файлу: {text}");
        }
    }

    /// Итог обновления обязан называть обе версии и **не содержать стрелки**.
    ///
    /// Стрелка `→` в этой строке простояла один глазной прогон: шрифты eframe
    /// знака U+2192 не содержат, и в окне на её месте пустой прямоугольник.
    /// Компилятор такое пропускает, тесты остального — тоже, а увидеть можно
    /// только после удачного обновления. Отсюда проверка машинно.
    #[test]
    fn update_summary_names_both_versions_without_an_arrow() {
        let updated = update_summary("ffmpeg", Some("8.1.2"), "N-125829", "");
        assert!(updated.contains("8.1.2"), "потеряна прежняя версия");
        assert!(updated.contains("N-125829"), "потеряна новая версия");

        let same = update_summary("ffmpeg", Some("N-125829"), "N-125829", " Оговорка.");
        assert!(same.contains("уже последней"), "не сказано, что менять нечего");
        assert!(same.contains("Оговорка."), "потеряна оговорка");

        // Первая установка своей копии — обещать «обновлено с …» нечестно.
        let first = update_summary("yt-dlp", None, "2026.07.04", "");
        assert!(first.contains("установлен"), "первая установка названа обновлением");
        assert!(first.contains("2026.07.04"), "потеряна версия");

        for text in [&updated, &same, &first] {
            assert!(
                !text.chars().any(|c| c == '→' || c == '⟶' || c == '➜'),
                "в окне вместо стрелки нарисуется пустой прямоугольник: {text}"
            );
        }
    }

    /// Имя ассета для поиска суммы обязано браться из самой ссылки.
    ///
    /// В списке BtbN рядом лежит `…-win64-gpl-shared.zip`, и сумма от него
    /// подошла бы нашему архиву ровно никак: сверка провалилась бы на
    /// исправном файле, а источник на Windows один — ffmpeg не поставился бы
    /// вовсе. `find_sum` сравнивает имена целиком, здесь проверяем, что
    /// сравнивать ему дают правильное.
    #[test]
    fn checksum_is_looked_up_by_exact_asset_name() {
        let list = "\
1111111111111111111111111111111111111111111111111111111111111111  ffmpeg-master-latest-win64-gpl-shared.zip
2222222222222222222222222222222222222222222222222222222222222222  ffmpeg-master-latest-win64-gpl.zip
";
        let url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip";
        let asset = url.rsplit('/').next().unwrap();

        assert_eq!(asset, "ffmpeg-master-latest-win64-gpl.zip");
        assert_eq!(
            sha256::find_sum(list, asset),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
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
