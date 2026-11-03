//! Запись экрана и звука.
//!
//! Пишет ffmpeg, а Savio подбирает ему, **чем** писать на этой машине, и на
//! Windows сам подаёт звук системы. Про UI движок не знает: наружу — канал
//! [`Event`] и колбэк-будильник, как у загрузки и раздачи.
//!
//! # Почему проба, а не одна команда на всех
//!
//! Способов захватить экран и сжать кадры несколько, и какой заработает,
//! заранее не сказать: зависит от сборки ffmpeg, видеокарты и драйвера.
//! Проверено вживую (2026-11-03, ffmpeg 9.0.2, Radeon 780M, 2560×1440,
//! 144 Гц): AMD AMF принимает кадры с видеокарты из `gfxcapture` и `ddagrab`
//! без единого предупреждения при сборке команды — и падает на первом кадре
//! («SubmitInput() failed with error 18»), а с кадрами, выгруженными в память,
//! работает. NVENC на той же машине отвечает «Cannot load nvcuda.dll», Quick
//! Sync — «Error creating a MFX session». Поэтому [`probe`] перебирает
//! лестницу «источник × кодировщик» и запоминает первую рабочую ступень:
//! каждая проба — один кадр в `-f null`, четверть секунды.
//!
//! Источник на Windows — `gfxcapture` (Windows Graphics Capture), а `ddagrab`
//! только запасной, и порядок этот выбран замером, а не по вкусу: при
//! заказанных 144 к/с `ddagrab` вхолостую отдавал 89, а `gfxcapture` — 141.
//! `gfxcapture` появился лишь в ffmpeg 8.1, поэтому без запасного у старой
//! системной сборки записи не было бы вовсе.
//!
//! # Звук системы на Windows
//!
//! ffmpeg его снять не умеет: у DirectShow есть только микрофоны, а «Стерео
//! микшер» на большинстве машин выключен (здесь в списке dshow два микрофона
//! и ничего больше). Поэтому звук снимает [`loopback`] через WASAPI и подаёт
//! ffmpeg по петле TCP — **не** через stdin: stdin нужен для «q», которой
//! ffmpeg закрывает файл по-человечески. Слушает сам ffmpeg
//! (`tcp://127.0.0.1:…?listen=1`), Savio подключается к нему, и момент
//! подключения заодно отмечает, что видео уже открыто (см. ниже).
//!
//! # Синхронизация
//!
//! ffmpeg ставит каждый вход на ноль сам по себе, а открывает входы по
//! очереди. Видео открывается первым, звук — когда Savio подключится, и
//! между ними проходит время: замер — 0.36 с (2026-11-03). Без поправки звук
//! опережал бы картинку ровно на столько. Время звука в файле — это число
//! сэмплов, а не часы, поэтому поправка — тишина перед первым настоящим
//! сэмплом длиной от строки «Input #0» в журнале ffmpeg (видео открыто) до
//! запуска захвата звука.
//!
//! # Остановка
//!
//! «q» в stdin: ffmpeg дописывает файл и выходит с кодом 0. Убийство — только
//! если «q» не помогла: MP4 фрагментированный (`frag_keyframe`), и потому
//! даже убитая запись теряет лишь последние две секунды, а не весь файл,
//! как обычный MP4 без завершающего `moov`.

mod clock;
#[cfg(windows)]
mod loopback;
mod platform;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::binaries;
use super::tree::Group;
use super::ytdlp::hide_console;
use crate::i18n::{self, Key, Lang};
use crate::model::{
    CaptureKind, CaptureProgress, CaptureTrouble, FfmpegTail, ScreenMode, SoundSource,
    VideoEncoder,
};

/// С какой частотой писать, если система частоту экрана не назвала.
pub const FALLBACK_RATE: u32 = 60;

/// Потолок частоты. Не предел записи — защита от числа, которое система
/// вернула мусором: экранов быстрее 500 Гц не продают.
const MAX_RATE: u32 = 500;

/// Длина фрагмента MP4 в секундах — столько теряет запись, оборванная
/// падением или убийством. Она же — расстояние между опорными кадрами,
/// по которым плеер перематывает.
const FRAGMENT_SECS: u32 = 2;

/// Сколько ждать одну пробу. Кадр приходит за четверть секунды; потолок — на
/// случай, когда источник ждёт кадра, которого не будет (экран блокировки).
const PROBE_LIMIT: Duration = Duration::from_secs(15);

/// Сколько ждать, пока ffmpeg закроет файл после «q». Обычно доли секунды
/// (замер — 0.16 с), но кодировщику на процессоре надо дожать очередь кадров.
const STOP_LIMIT: Duration = Duration::from_secs(10);

/// Сколько ещё ждать, оборвав подачу звука, прежде чем убить ffmpeg.
const DRAIN_LIMIT: Duration = Duration::from_secs(3);

/// Сколько строк журнала ffmpeg отдаём окну за одну запись. Потолок по
/// Правилу 1: журнал окна ограничен и сам, но каждая строка — событие,
/// а ffmpeg на сбойном источнике повторяет одно предупреждение сотнями.
const LOG_FORWARD_LIMIT: usize = 200;

/// Сколько последних строк ffmpeg кладём в объяснение сбоя.
const TAIL_LINES: usize = 3;

/// Файл не больше этого, за который ffmpeg не насчитал ни доли секунды,
/// — не запись, а один заголовок. С `empty_moov` и `-flush_packets 1`
/// заголовок MP4 ложится на диск сразу, как открылись кодировщики
/// (замер — 781 байт), у MP3 — свои ID3 и Xing; упади ffmpeg следом,
/// человек увидел бы «сохранено: 0:00 · 1 КБ» и файл, который не играет.
/// Потолок, а не один «ноль секунд»: перестань ffmpeg отчитываться
/// о ходе работы при живой записи, большой файл не удалится.
const HEADER_LIMIT: u64 = 64 * 1024;

/// Битрейт звука. «Максимальное качество» MP3 — это 320 кбит/с, больше
/// формат не умеет; у AAC в MP4 то же число с запасом прозрачно.
const AUDIO_BITRATE: &str = "320k";

/// Частота дискретизации на выходе. 48 кГц — частота микшера Windows почти
/// на любой машине, и MP3 выше неё не умеет.
const AUDIO_RATE: &str = "48000";

/// Что ответила проба: чем можно записать на этой машине.
#[derive(Clone, Debug)]
pub struct Plan {
    ffmpeg: Option<PathBuf>,
    screen: Option<ScreenMode>,
    video: Result<Video, CaptureTrouble>,
    audio: Result<Audio, CaptureTrouble>,
    /// Оговорка про видео, которая не мешает писать (сеанс Wayland).
    caveat: Option<CaptureTrouble>,
}

impl Plan {
    /// Режим экрана. `None` — система размер и частоту не назвала, и тогда
    /// запись идёт с [`FALLBACK_RATE`].
    pub fn screen(&self) -> Option<ScreenMode> {
        self.screen
    }

    /// Чем будет сжиматься видео — или почему видео не записать.
    pub fn encoder(&self) -> Result<VideoEncoder, &CaptureTrouble> {
        self.video.as_ref().map(|video| video.encoder)
    }

    /// Откуда будет звук — или почему его не будет.
    pub fn sound(&self) -> Result<SoundSource, &CaptureTrouble> {
        self.audio.as_ref().map(Audio::source)
    }

    pub fn caveat(&self) -> Option<&CaptureTrouble> {
        self.caveat.as_ref()
    }

    /// Можно ли начать запись этого вида. `Err` — почему нет.
    ///
    /// Видео без звука записать можно (звук — оговорка), звук без звука —
    /// нельзя.
    pub fn ready(&self, kind: CaptureKind) -> Result<(), &CaptureTrouble> {
        match kind {
            CaptureKind::Video => self.video.as_ref().map(|_| ()),
            CaptureKind::Audio => self.audio.as_ref().map(|_| ()),
        }
    }
}

/// Как захватывается и сжимается видео.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Video {
    /// Аргументы входа целиком, вместе с `-i`.
    input: Vec<String>,
    /// Фильтр между источником и кодировщиком. `None` — кадры идут как есть.
    filter: Option<&'static str>,
    encoder: VideoEncoder,
    rate: u32,
}

/// Откуда звук. У каждой системы свой единственный путь.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Audio {
    /// Звук системы, который Savio снимает сам и подаёт ffmpeg по петле.
    /// Формат узнаётся при каждом запуске заново: между пробой и записью
    /// человек мог сменить устройство вывода.
    #[cfg(windows)]
    Loopback,
    /// Устройство, которое открывает сам ffmpeg: аргументы входа с `-i`.
    #[cfg(not(windows))]
    Device {
        input: Vec<String>,
        source: SoundSource,
    },
}

impl Audio {
    fn source(&self) -> SoundSource {
        match self {
            #[cfg(windows)]
            Audio::Loopback => SoundSource::System,
            #[cfg(not(windows))]
            Audio::Device { source, .. } => *source,
        }
    }
}

/// Новости записи — всё, что окно узнаёт о ней от движка.
#[derive(Debug)]
pub enum Event {
    /// Проба окончена.
    Probed(Plan),
    /// ffmpeg запущен, файл заведён.
    Started(PathBuf),
    /// Сколько уже записано. Приходит раз в секунду.
    Progress(CaptureProgress),
    /// Что-то пошло не так, но запись идёт.
    Warning(CaptureTrouble),
    /// Строка журнала ffmpeg.
    Log(String),
    /// Файл записан. `trouble` — запись кончилась не по просьбе, но то, что
    /// успело записаться, в файле.
    Saved {
        path: PathBuf,
        progress: CaptureProgress,
        trouble: Option<CaptureTrouble>,
    },
    /// Ничего не записано.
    Failed(CaptureTrouble),
}

/// Выясняет в отдельном потоке, чем писать. Ответ — единственное
/// [`Event::Probed`].
///
/// Поток, а не вызов: поиск ffmpeg ходит по диску, а каждая ступень лестницы
/// запускает процесс (Правило 1).
pub fn probe(notify: impl Fn() + Send + 'static) -> Receiver<Event> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(Event::Probed(plan()));
        notify();
    });
    rx
}

fn plan() -> Plan {
    let screen = platform::primary();
    let mode = screen.as_ref().and_then(|screen| screen.mode);
    let caveat = platform::caveat();
    let Some(ffmpeg) = binaries::locate(binaries::FFMPEG_NAME) else {
        return Plan {
            ffmpeg: None,
            screen: mode,
            video: Err(CaptureTrouble::NoFfmpeg),
            audio: Err(CaptureTrouble::NoFfmpeg),
            caveat,
        };
    };
    let rate = mode.map_or(FALLBACK_RATE, |mode| mode.refresh.clamp(1, MAX_RATE));
    let index = screen.as_ref().map_or(0, |screen| screen.index);
    let video = pick_video(&ffmpeg, index, rate);
    let audio = pick_audio(&ffmpeg);
    Plan {
        ffmpeg: Some(ffmpeg),
        screen: mode,
        video,
        audio,
        caveat,
    }
}

/// Первая рабочая ступень лестницы «источник × кодировщик».
fn pick_video(ffmpeg: &Path, index: u32, rate: u32) -> Result<Video, CaptureTrouble> {
    let mut last = FfmpegTail::Code(None);
    let mut found = None;
    for input in platform::sources(index, rate)? {
        match try_ffmpeg(ffmpeg, &source_probe_args(&input)) {
            Ok(()) => {
                found = Some(input);
                break;
            }
            Err(tail) => last = tail,
        }
    }
    let Some(input) = found else {
        return Err(CaptureTrouble::NoScreen(last));
    };

    for &(encoder, filter) in platform::ENCODERS {
        let video = Video {
            input: input.clone(),
            filter,
            encoder,
            rate,
        };
        match try_ffmpeg(ffmpeg, &encoder_probe_args(&video)) {
            Ok(()) => return Ok(video),
            Err(tail) => last = tail,
        }
    }
    Err(CaptureTrouble::NoEncoder(last))
}

#[cfg(windows)]
fn pick_audio(_ffmpeg: &Path) -> Result<Audio, CaptureTrouble> {
    loopback::probe().map(|_| Audio::Loopback)
}

#[cfg(not(windows))]
fn pick_audio(ffmpeg: &Path) -> Result<Audio, CaptureTrouble> {
    let (input, source) = platform::sound_input();
    // На macOS устройство не пробуем: первое же обращение к микрофону
    // спрашивает разрешение, и вопрос всплыл бы от одного открытия раздела.
    // Там проверкой служит сама запись.
    if cfg!(target_os = "macos") {
        return Ok(Audio::Device { input, source });
    }
    let mut args = vec!["-hide_banner".to_owned(), "-v".into(), "error".into()];
    args.extend(input.iter().cloned());
    args.extend(["-t", "0.2", "-f", "null", "-"].map(String::from));
    try_ffmpeg(ffmpeg, &args)
        .map(|()| Audio::Device { input, source })
        .map_err(CaptureTrouble::NoSoundInput)
}

/// Проба источника: один кадр в никуда.
fn source_probe_args(input: &[String]) -> Vec<String> {
    let mut args = vec!["-hide_banner".to_owned(), "-v".into(), "error".into()];
    args.extend(input.iter().cloned());
    args.extend(["-frames:v", "1", "-f", "null", "-"].map(String::from));
    args
}

/// Проба кодировщика: тот же кадр, но сжатый. Ровно те же фильтр и ключи,
/// что и в записи: иначе проба проверяла бы не то, чем будут писать.
fn encoder_probe_args(video: &Video) -> Vec<String> {
    let mut args = vec!["-hide_banner".to_owned(), "-v".into(), "error".into()];
    args.extend(video.input.iter().cloned());
    if let Some(filter) = video.filter {
        args.extend(["-vf".to_owned(), filter.to_owned()]);
    }
    args.extend(["-frames:v".to_owned(), "1".into()]);
    args.extend(encoder_args(video.encoder, video.rate));
    args.extend(["-f", "null", "-"].map(String::from));
    args
}

/// Ключи кодировщика.
///
/// Качество — постоянное, а не битрейт: у экрана то неподвижная таблица,
/// то полноэкранное видео, и битрейт, хороший для одного, губит другое.
/// `yuv420p` у программных путей обязателен: из BGRA x264 сам выбрал бы
/// 4:4:4, которого не играет половина плееров. Кодировщики видеокарты
/// приводят цвет сами.
fn encoder_args(encoder: VideoEncoder, rate: u32) -> Vec<String> {
    let mut args: Vec<&str> = match encoder {
        VideoEncoder::Nvenc => vec![
            "-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq", "21",
            "-b:v", "0",
        ],
        VideoEncoder::Amf => vec![
            "-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", "20", "-qp_p",
            "22",
        ],
        VideoEncoder::QuickSync => vec![
            "-c:v",
            "h264_qsv",
            "-preset",
            "medium",
            "-global_quality",
            "22",
        ],
        VideoEncoder::VideoToolbox => {
            vec!["-c:v", "h264_videotoolbox", "-q:v", "65", "-pix_fmt", "yuv420p"]
        }
        VideoEncoder::X264 => vec![
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
        ],
    };
    // Интервал в кадрах, а не в секундах, и при неподвижном экране кадров
    // меньше заказанных: опорный кадр выходит реже. Фрагменты от этого не
    // зависят — их режет по времени `-frag_duration` в `record_args`.
    let gop = (rate * FRAGMENT_SECS).to_string();
    args.extend(["-g", &gop]);
    args.into_iter().map(String::from).collect()
}

/// Команда записи. `audio` — аргументы звукового входа вместе с `-i`.
///
/// Чистая функция ради проверки: порядок входов здесь — часть синхронизации
/// (видео первым, см. шапку модуля), а номер звукового входа в `-map`
/// зависит от того, есть ли видео.
fn record_args(video: Option<&Video>, audio: Option<&[String]>, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        // `info`, а не `error`: по строке «Input #0» движок узнаёт, что видео
        // открыто, и от неё считает поправку звука. Тише ffmpeg на `info` не
        // становится только в начале — дальше `-nostats` его глушит.
        "-loglevel",
        "info",
        "-nostats",
        // Ход работы — в stdout построчно: время и размер файла от самого
        // ffmpeg, а не от часов Savio (Правило 6).
        "-progress",
        "pipe:1",
        "-stats_period",
        "1",
    ]
    .map(String::from)
    .to_vec();

    if let Some(video) = video {
        args.extend(video.input.iter().cloned());
    }
    if let Some(audio) = audio {
        args.extend(audio.iter().cloned());
    }

    let audio_index = usize::from(video.is_some());
    if video.is_some() {
        args.extend(["-map".to_owned(), "0:v".into()]);
    }
    if audio.is_some() {
        args.extend(["-map".to_owned(), format!("{audio_index}:a")]);
    }

    if let Some(video) = video {
        if let Some(filter) = video.filter {
            args.extend(["-vf".to_owned(), filter.to_owned()]);
        }
        args.extend(encoder_args(video.encoder, video.rate));
    }
    if audio.is_some() {
        let codec = if video.is_some() { "aac" } else { "libmp3lame" };
        args.extend(
            ["-c:a", codec, "-b:a", AUDIO_BITRATE, "-ar", AUDIO_RATE, "-ac", "2"].map(String::from),
        );
    }
    // Каждый пакет — сразу на диск. Иначе ffmpeg копит запись в буфере по
    // 256 КБ: размер в окне стоял бы на месте, а оборванная запись теряла бы
    // и этот буфер (замер 2026-11-03: без ключа `total_size` держался на
    // 28 байтах шесть секунд подряд).
    args.extend(["-flush_packets", "1"].map(String::from));
    if video.is_some() {
        // Фрагментированный MP4: каждые `FRAGMENT_SECS` в файле законченный
        // кусок. Обычный MP4 пишет оглавление в самом конце, и запись,
        // оборванная падением или убийством, не открывалась бы вовсе.
        //
        // Режет по времени `-frag_duration`, а не опорные кадры: AMF
        // и `-force_key_frames` пропускает мимо ушей, и с одним
        // `frag_keyframe` фрагмент на неподвижном экране тянулся по восемь
        // секунд. Проверено вживую: файл, обрезанный на 60 %, открывается
        // и играет 5 с из 7.
        args.extend(
            [
                "-frag_duration".to_owned(),
                (u64::from(FRAGMENT_SECS) * 1_000_000).to_string(),
                "-movflags".into(),
                "+frag_keyframe+empty_moov+default_base_moof".into(),
            ],
        );
    }
    // `-n`: имя свободно ([`unique_path`]), и чужой файл, появившийся под
    // ним за эти миллисекунды, лучше не затирать, а отказаться.
    args.push("-n".to_owned());
    args.push(out.to_string_lossy().into_owned());
    args
}

/// Запускает ffmpeg и ждёт его конца не дольше [`PROBE_LIMIT`].
/// `Err` — что он ответил: первые строки ругани, код или молчание.
fn try_ffmpeg(ffmpeg: &Path, args: &[String]) -> Result<(), FfmpegTail> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    die_with_savio(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|err| FfmpegTail::Launch(err.to_string()))?;

    // Ругань читаем в своём потоке: с `-v error` её мало, но труба у неё
    // конечная, и переполнившись, она остановила бы ffmpeg, а мы ждали бы
    // его конца до таймаута.
    let reader = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut lines = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let line = line.trim().to_owned();
                if !line.is_empty() && lines.len() < TAIL_LINES {
                    lines.push(line);
                }
            }
            lines.join(" ")
        })
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < PROBE_LIMIT => {
                std::thread::sleep(Duration::from_millis(15));
            }
            _ => {
                super::kill_and_reap(&mut child);
                break None;
            }
        }
    };
    let text = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) if text.is_empty() => Err(FfmpegTail::Code(status.code())),
        Some(_) => Err(FfmpegTail::Said(text)),
        None => Err(FfmpegTail::Silence(PROBE_LIMIT.as_secs())),
    }
}

/// Просит ядро остановить ffmpeg, если поток, запустивший его, умер раньше.
///
/// Упади Savio посреди записи — паникой, убийством, нехваткой памяти, —
/// и `on_exit` не прозвучит, а «q» не придёт никогда. На Windows ffmpeg
/// убьёт Job Object (`KILL_ON_JOB_CLOSE` в `tree`), а на Linux он жил бы
/// дальше — в своей группе процессов, равнодушный к закрытому stdin, —
/// и писал бы экран невидимо, пока не кончится место. `PR_SET_PDEATHSIG`
/// шлёт ему SIGTERM, когда умирает **поток**-родитель: это поток записи
/// (или пробы), и живёт он ровно до конца своего ffmpeg. На SIGTERM ffmpeg
/// дописывает файл, как на «q».
#[cfg(target_os = "linux")]
fn die_with_savio(cmd: &mut Command) {
    use std::ffi::{c_int, c_ulong};
    use std::os::unix::process::CommandExt;

    const PR_SET_PDEATHSIG: c_int = 1;
    const SIGTERM: c_ulong = 15;
    unsafe extern "C" {
        fn prctl(option: c_int, ...) -> c_int;
    }
    // Замыкание исполняется в дочернем процессе между `fork` и `exec`, где
    // разрешено только безопасное для сигналов; `prctl` — из таких.
    unsafe {
        cmd.pre_exec(|| {
            if prctl(PR_SET_PDEATHSIG, SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// На Windows ту же работу делает Job Object (`tree`). На macOS способа
/// нет: ни `PR_SET_PDEATHSIG`, ни задания там не бывает, и ffmpeg,
/// переживший упавший Savio, остаётся работать — задача 91 реестра.
#[cfg(not(target_os = "linux"))]
fn die_with_savio(_cmd: &mut Command) {}

// ---------------------------------------------------------------------------
// Запись
// ---------------------------------------------------------------------------

/// Что поток записи слышит снаружи.
enum Ctl {
    /// Окно попросило остановиться.
    Stop,
    /// ffmpeg закрыл stdout — значит, кончился.
    Ended,
}

/// Кончилась ли запись — для ожидания при закрытии окна.
#[derive(Default)]
struct Finished {
    done: Mutex<bool>,
    wake: Condvar,
}

impl Finished {
    fn mark(&self) {
        if let Ok(mut done) = self.done.lock() {
            *done = true;
        }
        self.wake.notify_all();
    }

    fn wait(&self, limit: Duration) {
        if let Ok(done) = self.done.lock() {
            let _ = self.wake.wait_timeout_while(done, limit, |done| !*done);
        }
    }
}

/// Ручка идущей записи.
pub struct Handle {
    ctl: Sender<Ctl>,
    finished: Arc<Finished>,
}

impl Handle {
    /// Просит закончить запись. Возвращает управление сразу; итог придёт
    /// событием [`Event::Saved`] или [`Event::Failed`].
    pub fn stop(&self) {
        let _ = self.ctl.send(Ctl::Stop);
    }

    /// Просит закончить и ждёт, пока файл будет дописан, но не дольше
    /// `limit`.
    ///
    /// Только для закрытия окна: процесс кончается сразу за `on_exit`, и
    /// ffmpeg, которому не дали дописать, оставил бы файл без последнего
    /// фрагмента (Правило 3).
    pub fn stop_and_wait(&self, limit: Duration) {
        self.stop();
        self.finished.wait(limit);
    }
}

/// Начинает запись в папку `dir`.
pub fn start(
    plan: &Plan,
    kind: CaptureKind,
    dir: PathBuf,
    lang: Lang,
    notify: impl Fn() + Send + Sync + 'static,
) -> (Receiver<Event>, Handle) {
    let (tx, rx) = channel();
    let (ctl_tx, ctl_rx) = channel();
    let finished = Arc::new(Finished::default());
    let handle = Handle {
        ctl: ctl_tx.clone(),
        finished: Arc::clone(&finished),
    };
    let plan = plan.clone();
    let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(notify);

    std::thread::spawn(move || {
        let job = Job {
            plan,
            kind,
            dir,
            lang,
        };
        let last = record(&job, &tx, &ctl_rx, ctl_tx, &notify);
        let _ = tx.send(last);
        notify();
        finished.mark();
    });

    (rx, handle)
}

struct Job {
    plan: Plan,
    kind: CaptureKind,
    dir: PathBuf,
    lang: Lang,
}

/// Вся запись от запуска до итога. Возвращает последнее событие.
fn record(
    job: &Job,
    tx: &Sender<Event>,
    ctl: &Receiver<Ctl>,
    ctl_tx: Sender<Ctl>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Event {
    let Some(ffmpeg) = &job.plan.ffmpeg else {
        return Event::Failed(CaptureTrouble::NoFfmpeg);
    };
    let video = match job.kind {
        CaptureKind::Video => match &job.plan.video {
            Ok(video) => Some(video),
            Err(trouble) => return Event::Failed(trouble.clone()),
        },
        CaptureKind::Audio => None,
    };
    let path = unique_path(&job.dir, &file_stem(job.kind, job.lang), job.kind.extension());

    // Звук системы на Windows: свой поток открывает устройство и называет
    // формат. Открывается он **до** запуска ffmpeg — формат нужен в команде.
    let abort = Arc::new(AtomicBool::new(false));
    let (video_open_tx, video_open_rx) = channel::<Instant>();
    let mut feeder = None;
    let known = job.plan.audio.is_err();
    let opened = job.plan.audio.clone().and_then(|audio| {
        open_audio(&audio, video.is_some(), video_open_rx, &abort, tx, &mut feeder, job.lang)
    });
    let input = match without_sound(opened, job.kind, known) {
        Ok((input, warning)) => {
            if let Some(warning) = warning {
                let _ = tx.send(Event::Warning(warning));
            }
            input
        }
        Err(trouble) => return Event::Failed(trouble),
    };

    let args = record_args(video, input.as_deref(), &path);
    let _ = tx.send(Event::Log(format!("ffmpeg {}", args.join(" "))));

    let group = Group::default();
    let mut cmd = Command::new(ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    group.arm(&mut cmd);
    die_with_savio(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            abort.store(true, Ordering::SeqCst);
            return Event::Failed(CaptureTrouble::Launch(err.to_string()));
        }
    };
    group.adopt(&child);
    let _ = tx.send(Event::Started(path.clone()));
    notify();

    let stdin = child.stdin.take();
    let errors = child.stderr.take().map(|stderr| {
        let tx = tx.clone();
        std::thread::spawn(move || read_journal(stderr, &tx, &video_open_tx))
    });
    let progress = child.stdout.take().map(|stdout| {
        let tx = tx.clone();
        let notify = Arc::clone(notify);
        let video = video.is_some();
        std::thread::spawn(move || {
            let last = read_progress(stdout, video, &tx, &*notify);
            let _ = ctl_tx.send(Ctl::Ended);
            last
        })
    });

    // Ждём, что случится раньше: просьба остановиться или конец ffmpeg.
    // Отвалившийся канал — тоже просьба: слать её больше некому.
    let asked = !matches!(ctl.recv(), Ok(Ctl::Ended));
    let (status, hung) = if asked {
        finish(&mut child, stdin, &abort, &group)
    } else {
        (child.wait().ok(), false)
    };
    group.release();
    abort.store(true, Ordering::SeqCst);
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    let tail = errors
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    let progress = progress
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();

    outcome(&path, progress, status, asked, hung, tail)
}

/// Что делать с результатом открытия звука: аргументы входа (или `None`
/// — звука не будет) и предупреждение для окна, а `Err` — писать нечего.
///
/// Видео без звука — запись, а не отказ. Но сказать о пропаже надо только
/// то, чего проба не знала (`known`): свою причину экран уже показывает
/// у строки «Звук», и повтор её в предупреждениях читался бы второй бедой.
fn without_sound(
    opened: Result<Vec<String>, CaptureTrouble>,
    kind: CaptureKind,
    known: bool,
) -> Result<(Option<Vec<String>>, Option<CaptureTrouble>), CaptureTrouble> {
    match (opened, kind) {
        (Ok(input), _) => Ok((Some(input), None)),
        (Err(trouble), CaptureKind::Audio) => Err(trouble),
        (Err(trouble), CaptureKind::Video) => Ok((None, (!known).then_some(trouble))),
    }
}

/// Открывает звук и отдаёт аргументы его входа вместе с `-i`.
///
/// Звук системы на Windows снимает свой поток ([`loopback`]): он открывает
/// устройство, называет формат и ждёт, пока ffmpeg начнёт слушать петлю.
#[cfg(windows)]
fn open_audio(
    audio: &Audio,
    with_video: bool,
    video_open: Receiver<Instant>,
    abort: &Arc<AtomicBool>,
    tx: &Sender<Event>,
    feeder: &mut Option<std::thread::JoinHandle<()>>,
    lang: Lang,
) -> Result<Vec<String>, CaptureTrouble> {
    match audio {
        Audio::Loopback => {
            let port = free_port().map_err(CaptureTrouble::NoSound)?;
            let video_open = with_video.then_some(video_open);
            let (format, thread) =
                loopback::spawn(port, video_open, Arc::clone(abort), tx.clone(), lang)?;
            *feeder = Some(thread);
            Ok(loopback::input_args(&format, port))
        }
    }
}

/// На других системах звук открывает сам ffmpeg, и открывать здесь нечего.
#[cfg(not(windows))]
fn open_audio(
    audio: &Audio,
    _with_video: bool,
    _video_open: Receiver<Instant>,
    _abort: &Arc<AtomicBool>,
    _tx: &Sender<Event>,
    _feeder: &mut Option<std::thread::JoinHandle<()>>,
    _lang: Lang,
) -> Result<Vec<String>, CaptureTrouble> {
    match audio {
        Audio::Device { input, .. } => Ok(input.clone()),
    }
}

/// Свободный порт на петле — для ffmpeg, который будет его слушать.
///
/// Между тем, как порт отпущен здесь, и тем, как его займёт ffmpeg, лежат
/// миллисекунды. Займи его за это время кто-то другой — ffmpeg не откроет
/// вход, и запись честно не начнётся, а звук чужому процессу всё равно не
/// уйдёт: подключается Savio, а не к Savio.
#[cfg(windows)]
fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|err| err.to_string())?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|err| err.to_string())
}

/// Останавливает ffmpeg: «q», затем обрыв звука, затем убийство.
///
/// Звук подаётся и после «q», и это не небрежность: поток ffmpeg, читающий
/// петлю, сидит в `recv` и увидит просьбу остановиться, только получив
/// следующие байты. Перестань Savio слать — ffmpeg ждал бы их вечно
/// (проверено вживую: без подачи «q» не закрывала файл и за 20 секунд,
/// с подачей — за 0.16 с).
fn finish(
    child: &mut Child,
    stdin: Option<std::process::ChildStdin>,
    abort: &AtomicBool,
    group: &Group,
) -> (Option<ExitStatus>, bool) {
    if let Some(mut stdin) = stdin {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
        if let Some(status) = wait_for(child, STOP_LIMIT) {
            return (Some(status), false);
        }
    }
    // «q» не помогла: обрываем звук — ffmpeg увидит конец входа.
    abort.store(true, Ordering::SeqCst);
    if let Some(status) = wait_for(child, DRAIN_LIMIT) {
        return (Some(status), false);
    }
    group.kill();
    super::kill_and_reap(child);
    (None, true)
}

fn wait_for(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if started.elapsed() < limit => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => return None,
        }
    }
}

/// Итог по тому, что лежит на диске, а не по коду возврата (Правило 6).
fn outcome(
    path: &Path,
    progress: CaptureProgress,
    status: Option<ExitStatus>,
    asked: bool,
    hung: bool,
    tail: Vec<String>,
) -> Event {
    let size = std::fs::metadata(path).map_or(0, |meta| meta.len());
    let tail = tail.join(" ");
    let code = status.and_then(|status| status.code());
    let trouble = if hung {
        Some(CaptureTrouble::Hung)
    } else if !asked || !status.is_some_and(|status| status.success()) {
        Some(CaptureTrouble::Broke { code, tail })
    } else {
        None
    };

    if size == 0 || (progress.secs <= 0.0 && size <= HEADER_LIMIT) {
        let _ = std::fs::remove_file(path);
        return Event::Failed(trouble.unwrap_or(CaptureTrouble::Empty));
    }
    Event::Saved {
        path: path.to_owned(),
        progress: CaptureProgress {
            bytes: progress.bytes.max(size),
            ..progress
        },
        trouble,
    }
}

/// Читает журнал ffmpeg: строки — окну, «Input #0» — потоку звука,
/// последние строки — в объяснение сбоя.
fn read_journal(
    stderr: impl std::io::Read,
    tx: &Sender<Event>,
    video_open: &Sender<Instant>,
) -> Vec<String> {
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut forwarded = 0;
    let mut seen_input = false;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if !seen_input && line.starts_with("Input #0") {
            seen_input = true;
            let _ = video_open.send(Instant::now());
        }
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line.trim().to_owned());
        if forwarded < LOG_FORWARD_LIMIT {
            forwarded += 1;
            // Без будильника: журнал не повод для кадра, его заберёт ближайший
            // кадр хода работы — тот приходит раз в секунду.
            let _ = tx.send(Event::Log(line.to_owned()));
        }
    }
    tail.into()
}

/// Читает ход работы из `-progress` и отдаёт его окну. Возвращает последний.
fn read_progress(
    stdout: impl std::io::Read,
    video: bool,
    tx: &Sender<Event>,
    notify: &(dyn Fn() + Send + Sync),
) -> CaptureProgress {
    let mut reader = ProgressReader::new(video);
    let mut last = reader.now;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some(progress) = reader.feed(&line) {
            last = progress;
            let _ = tx.send(Event::Progress(progress));
            notify();
        }
    }
    last
}

/// Разбор `-progress`: блоки строк `ключ=значение`, каждый кончается строкой
/// `progress=continue` (или `end` в самом конце).
struct ProgressReader {
    now: CaptureProgress,
}

impl ProgressReader {
    fn new(video: bool) -> Self {
        Self {
            now: CaptureProgress {
                frames: video.then_some(0),
                ..CaptureProgress::default()
            },
        }
    }

    /// Отдаёт ход работы на строке, закрывающей блок.
    ///
    /// Значения в начале записи бывают `N/A` — тогда остаётся прежнее, а не
    /// ноль: иначе время в окне прыгало бы назад.
    fn feed(&mut self, line: &str) -> Option<CaptureProgress> {
        let (key, value) = line.trim().split_once('=')?;
        match key {
            "frame" if self.now.frames.is_some() => {
                if let Ok(frames) = value.parse() {
                    self.now.frames = Some(frames);
                }
            }
            "total_size" => {
                if let Ok(bytes) = value.parse() {
                    self.now.bytes = bytes;
                }
            }
            // `out_time_ms` — то же число в микросекундах, названное неверно
            // ещё в старых выпусках; `out_time_us` — его исправленное имя.
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = value.parse::<i64>() {
                    self.now.secs = us.max(0) as f64 / 1_000_000.0;
                }
            }
            "progress" => return Some(self.now),
            _ => {}
        }
        None
    }
}

/// Имя файла без расширения: «Запись экрана 2026-11-03 14-05-12».
///
/// Дата — по часам машины, а не UTC: человек ищет запись по тому времени,
/// которое видел на экране. Двоеточий в имени нет — Windows их не пускает.
fn file_stem(kind: CaptureKind, lang: Lang) -> String {
    let prefix = i18n::t(
        lang,
        match kind {
            CaptureKind::Video => Key::CaptureFileVideo,
            CaptureKind::Audio => Key::CaptureFileAudio,
        },
    );
    match clock::now() {
        Some(at) => format!(
            "{prefix} {:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            at.year, at.month, at.day, at.hour, at.minute, at.second
        ),
        None => prefix.to_owned(),
    }
}

/// Путь, которого ещё нет: при совпадении к имени дописывается «(2)».
fn unique_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    (2..1000)
        .map(|n| dir.join(format!("{stem} ({n}).{extension}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(encoder: VideoEncoder, filter: Option<&'static str>) -> Video {
        Video {
            input: ["-f", "lavfi", "-i", "gfxcapture=monitor_idx=0:max_framerate=144"]
                .map(String::from)
                .to_vec(),
            filter,
            encoder,
            rate: 144,
        }
    }

    /// Звуковой вход в том виде, в каком его собирает петля на Windows.
    fn pipe() -> Vec<String> {
        [
            "-f", "f32le", "-ar", "48000", "-ac", "2", "-i", "tcp://127.0.0.1:50123?listen=1",
        ]
        .map(String::from)
        .to_vec()
    }

    fn position(args: &[String], value: &str) -> usize {
        args.iter()
            .position(|arg| arg == value)
            .unwrap_or_else(|| panic!("в команде нет «{value}»: {args:?}"))
    }

    /// Видео открывается первым, а звук ссылается на второй вход. На этом
    /// порядке держится поправка звука: её отсчитывают от строки «Input #0»,
    /// и встань звук нулевым входом — тишина легла бы не туда.
    #[test]
    fn video_comes_first_and_sound_maps_to_the_second_input() {
        let args = record_args(
            Some(&video(VideoEncoder::Amf, Some("hwdownload,format=bgra"))),
            Some(&pipe()[..]),
            Path::new("out.mp4"),
        );
        let lavfi = position(&args, "gfxcapture=monitor_idx=0:max_framerate=144");
        let tcp = position(&args, "tcp://127.0.0.1:50123?listen=1");
        assert!(lavfi < tcp, "звук открылся раньше видео: {args:?}");
        assert!(args.windows(2).any(|w| w == ["-map", "0:v"]));
        assert!(args.windows(2).any(|w| w == ["-map", "1:a"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(args.windows(2).any(|w| w == ["-vf", "hwdownload,format=bgra"]));
        // Частоту и каналы звука ffmpeg узнаёт только от нас: у сырых
        // сэмплов заголовка нет.
        assert!(args.windows(2).any(|w| w == ["-f", "f32le"]));
        assert!(args.windows(2).any(|w| w == ["-ar", "48000"]));
    }

    /// Только звук — MP3 в 320 кбит/с, без видео и без фрагментов MP4.
    #[test]
    fn sound_only_is_a_320k_mp3_of_the_first_input() {
        let args = record_args(None, Some(&pipe()[..]), Path::new("out.mp3"));
        assert!(args.windows(2).any(|w| w == ["-map", "0:a"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "libmp3lame"]));
        assert!(args.windows(2).any(|w| w == ["-b:a", "320k"]));
        assert!(!args.iter().any(|arg| arg == "-movflags"));
        assert!(!args.iter().any(|arg| arg.starts_with("-c:v")));
    }

    /// MP4 фрагментированный: без этого запись, оборванная падением, не
    /// открывалась бы вовсе.
    #[test]
    fn a_video_survives_being_cut_short() {
        let args = record_args(
            Some(&video(VideoEncoder::X264, Some("hwdownload,format=bgra"))),
            None,
            Path::new("out.mp4"),
        );
        let flags = &args[position(&args, "-movflags") + 1];
        assert!(flags.contains("frag_keyframe"), "{flags}");
        assert!(flags.contains("empty_moov"), "{flags}");
        // Фрагменты режутся по времени: AMF опорные кадры по времени не ставит.
        assert!(args.windows(2).any(|w| w == ["-frag_duration", "2000000"]));
        assert!(args.windows(2).any(|w| w == ["-flush_packets", "1"]));
    }

    /// Ход работы идёт в stdout, а журнал — с «Input #0»: без `info` его нет,
    /// без `-progress` нечем показать, что запись идёт.
    #[test]
    fn the_command_reports_progress_and_names_its_inputs() {
        let args = record_args(Some(&video(VideoEncoder::Nvenc, None)), None, Path::new("o.mp4"));
        assert!(args.windows(2).any(|w| w == ["-progress", "pipe:1"]));
        assert!(args.windows(2).any(|w| w == ["-loglevel", "info"]));
        // stdin нужен для «q»: `-nostdin` её отключил бы.
        assert!(!args.iter().any(|arg| arg == "-nostdin"));
        assert_eq!(args.last().map(String::as_str), Some("o.mp4"));
        assert_eq!(args[args.len() - 2], "-n");
    }

    /// Программный путь просит `yuv420p` и опорный кадр раз в две секунды.
    #[test]
    fn a_software_encoder_asks_for_420_and_regular_keyframes() {
        let args = encoder_args(VideoEncoder::X264, 144);
        assert!(args.windows(2).any(|w| w == ["-pix_fmt", "yuv420p"]));
        assert!(args.windows(2).any(|w| w == ["-g", "288"]));
        // Кадры с видеокарты в формате не нуждаются, и `-pix_fmt` у них
        // сломал бы путь без выгрузки.
        let nvenc = encoder_args(VideoEncoder::Nvenc, 60);
        assert!(!nvenc.iter().any(|arg| arg == "-pix_fmt"));
    }

    /// Проба сжатия проверяет то же, чем будут писать: тот же фильтр, те же
    /// ключи кодировщика.
    #[test]
    fn the_encoder_probe_uses_the_recording_filter_and_keys() {
        let video = video(VideoEncoder::Amf, Some("hwdownload,format=bgra"));
        let probe = encoder_probe_args(&video);
        let record = record_args(Some(&video), None, Path::new("o.mp4"));
        for key in encoder_args(VideoEncoder::Amf, 144) {
            assert!(probe.contains(&key), "в пробе нет «{key}»");
            assert!(record.contains(&key), "в записи нет «{key}»");
        }
        assert!(probe.windows(2).any(|w| w == ["-vf", "hwdownload,format=bgra"]));
        assert!(probe.windows(2).any(|w| w == ["-frames:v", "1"]));
    }

    /// Блок `-progress`, как его печатает ffmpeg 9.0.2 (снят вживую).
    #[test]
    fn progress_is_read_block_by_block() {
        let mut reader = ProgressReader::new(true);
        let block = "frame=412\nfps=82.40\nstream_0_0_q=-0.0\nbitrate=N/A\n\
                     total_size=1835008\nout_time_us=5021000\nout_time_ms=5021000\n\
                     out_time=00:00:05.021000\ndup_frames=0\ndrop_frames=0\nspeed=1x\n\
                     progress=continue";
        let mut got = None;
        for line in block.lines() {
            if let Some(progress) = reader.feed(line) {
                got = Some(progress);
            }
        }
        let got = got.expect("блок не дошёл до «progress=»");
        assert_eq!(got.frames, Some(412));
        assert_eq!(got.bytes, 1_835_008);
        assert!((got.secs - 5.021).abs() < 1e-9, "{}", got.secs);
    }

    /// `N/A` в начале записи не обнуляет уже известное: время в окне не
    /// прыгает назад.
    #[test]
    fn a_missing_value_keeps_the_previous_one() {
        let mut reader = ProgressReader::new(true);
        for line in ["total_size=1000", "out_time_us=2000000", "frame=10", "progress=continue"] {
            reader.feed(line);
        }
        let mut last = None;
        for line in ["total_size=N/A", "out_time_us=N/A", "frame=N/A", "progress=continue"] {
            last = reader.feed(line).or(last);
        }
        let last = last.expect("блок не закрылся");
        assert_eq!(last.bytes, 1000);
        assert_eq!(last.frames, Some(10));
        assert!((last.secs - 2.0).abs() < 1e-9);
    }

    /// У записи звука кадров нет, и строка `frame=` их не заводит: иначе
    /// окно показало бы «0 к/с» у MP3.
    #[test]
    fn sound_only_progress_has_no_frames() {
        let mut reader = ProgressReader::new(false);
        reader.feed("frame=0");
        let got = reader.feed("progress=continue").expect("блок не закрылся");
        assert_eq!(got.frames, None);
    }

    /// Занятое имя не затирается: к нему дописывается номер.
    #[test]
    fn a_taken_name_gets_a_number() {
        let dir = std::env::temp_dir().join(format!("savio-capture-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = unique_path(&dir, "Запись", "mp4");
        assert_eq!(first, dir.join("Запись.mp4"));
        std::fs::write(&first, b"x").unwrap();
        let second = unique_path(&dir, "Запись", "mp4");
        assert_eq!(second, dir.join("Запись (2).mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Имя файла называет вид записи на языке окна и не содержит двоеточий.
    #[test]
    fn the_file_name_says_what_it_is_and_suits_windows() {
        for lang in Lang::ALL {
            for kind in CaptureKind::ALL {
                let stem = file_stem(kind, lang);
                assert!(!stem.contains(':'), "{stem}");
                assert!(!stem.trim().is_empty());
            }
        }
    }

    /// Итог судит по диску: пустой файл — не запись, даже с кодом 0.
    #[test]
    fn an_empty_file_is_a_failure_whatever_the_exit_code() {
        let dir = std::env::temp_dir().join(format!("savio-capture-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.mp4");
        std::fs::write(&path, b"").unwrap();
        let event = outcome(&path, CaptureProgress::default(), None, true, false, Vec::new());
        assert!(
            matches!(event, Event::Failed(CaptureTrouble::Broke { .. })),
            "{event:?}"
        );
        assert!(!path.exists(), "пустой файл остался лежать");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Запись, кончившаяся сама, сохраняется, но не выдаётся за удачную.
    #[test]
    fn a_recording_that_ended_by_itself_is_kept_but_flagged() {
        let dir = std::env::temp_dir().join(format!("savio-capture-broke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("part.mp4");
        std::fs::write(&path, b"data").unwrap();
        let progress = CaptureProgress {
            secs: 5.0,
            bytes: 4,
            frames: Some(400),
        };
        let event = outcome(
            &path,
            progress,
            None,
            false,
            false,
            vec!["No space left on device".into()],
        );
        match event {
            Event::Saved {
                trouble: Some(CaptureTrouble::Broke { tail, .. }),
                ..
            } => assert!(tail.contains("No space"), "{tail}"),
            other => panic!("ожидали сохранённую оборванную запись: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Один заголовок без единой доли секунды — не запись: файл убран, и
    /// окно говорит «не записалось», а не «сохранено: 0:00 · 1 КБ».
    #[test]
    fn a_file_with_only_a_header_is_not_a_recording() {
        let dir = std::env::temp_dir().join(format!("savio-capture-head-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("head.mp4");
        std::fs::write(&path, vec![0u8; 781]).unwrap();
        let event = outcome(
            &path,
            CaptureProgress::default(),
            None,
            false,
            false,
            vec!["Error while opening encoder".into()],
        );
        assert!(
            matches!(event, Event::Failed(CaptureTrouble::Broke { .. })),
            "{event:?}"
        );
        assert!(!path.exists(), "заголовок остался лежать");
        // А большой файл без отчёта о времени — не удаляется: это запись,
        // о которой ffmpeg не успел или не смог отчитаться.
        std::fs::write(&path, vec![0u8; 1 << 20]).unwrap();
        let event = outcome(&path, CaptureProgress::default(), None, false, false, Vec::new());
        assert!(matches!(event, Event::Saved { .. }), "{event:?}");
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Видео без звука пишется, MP3 без звука — нет, а о пропаже звука
    /// предупреждение уходит, только если проба о ней не знала: иначе
    /// экран показал бы одну беду дважды.
    #[test]
    fn a_missing_sound_is_told_once() {
        let trouble = || CaptureTrouble::NoSound("HRESULT 0x88890004".into());
        assert_eq!(
            without_sound(Err(trouble()), CaptureKind::Video, true),
            Ok((None, None))
        );
        assert_eq!(
            without_sound(Err(trouble()), CaptureKind::Video, false),
            Ok((None, Some(trouble())))
        );
        assert_eq!(
            without_sound(Err(trouble()), CaptureKind::Audio, false),
            Err(trouble())
        );
        let input = vec!["-i".to_owned(), "x".to_owned()];
        assert_eq!(
            without_sound(Ok(input.clone()), CaptureKind::Video, false),
            Ok((Some(input), None))
        );
    }

    /// ffmpeg, запущенный потоком, умирает вместе с ним: на этом держится
    /// то, что упавший Savio не оставляет невидимую запись экрана. `sleep`
    /// вместо ffmpeg — механизм тот же, а ffmpeg на машине CI нет.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_dies_with_the_thread_that_started_it() {
        use std::os::unix::process::ExitStatusExt;
        let child = std::thread::spawn(|| {
            let mut cmd = Command::new("sleep");
            cmd.arg("30");
            die_with_savio(&mut cmd);
            cmd.spawn().expect("sleep не запустился")
        })
        .join()
        .unwrap();
        let mut child = child;
        let status = wait_for(&mut child, Duration::from_secs(5));
        let _ = child.kill();
        let status = status.expect("процесс пережил поток, который его запустил");
        assert_eq!(status.signal(), Some(15), "{status:?}");
    }

    /// Настоящая запись настоящим ffmpeg: экран и звук системы, остановка
    /// «q». Проверяется то, чего не видят ни сборка, ни тесты выше (Правило
    /// 6): файл открывается, в нём обе дорожки, и звук не короче видео
    /// больше чем на четверть секунды — то есть поправка начала сработала.
    #[test]
    #[ignore = "пишет экран и звук этой машины; нужен ffmpeg"]
    fn real_recording_of_screen_and_sound() {
        let rx = probe(|| {});
        let Ok(Event::Probed(plan)) = rx.recv() else {
            panic!("проба не ответила");
        };
        eprintln!(
            "экран {:?}, сжатие {:?}, звук {:?}",
            plan.screen(),
            plan.encoder(),
            plan.sound()
        );
        plan.ready(CaptureKind::Video).expect("видео не записать");

        let dir = std::env::temp_dir().join(format!("savio-capture-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (rx, handle) = start(&plan, CaptureKind::Video, dir.clone(), Lang::Ru, || {});
        let started = Instant::now();
        let mut saved = None;
        let mut stopped = false;
        while saved.is_none() {
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(Event::Log(line)) => eprintln!("ffmpeg: {line}"),
                Ok(Event::Saved { path, progress, trouble }) => {
                    saved = Some((path, progress, trouble));
                }
                Ok(Event::Failed(trouble)) => panic!("запись не удалась: {trouble:?}"),
                Ok(other) => eprintln!("{other:?}"),
                Err(err) => panic!("движок замолчал: {err}"),
            }
            if !stopped && started.elapsed() > Duration::from_secs(6) {
                stopped = true;
                handle.stop();
            }
        }
        let (path, progress, trouble) = saved.unwrap();
        assert_eq!(trouble, None, "запись кончилась не по просьбе");
        assert!(progress.secs > 4.0, "в файле {} с", progress.secs);

        let ffprobe = binaries::locate(binaries::FFPROBE_NAME).expect("нужен ffprobe");
        let out = Command::new(ffprobe)
            .args(["-v", "error", "-show_entries", "stream=codec_type,duration", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .expect("ffprobe");
        let text = String::from_utf8_lossy(&out.stdout);
        eprintln!("{text}");
        let duration = |kind: &str| {
            text.lines()
                .find(|line| line.starts_with(kind))
                .and_then(|line| line.split(',').nth(1))
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or_else(|| panic!("нет дорожки {kind}: {text}"))
        };
        let (video, audio) = (duration("video"), duration("audio"));
        assert!(
            (video - audio).abs() < 0.25,
            "видео {video} с, звук {audio} с: начала разошлись"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
