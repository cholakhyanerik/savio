//! Звук системы на Windows: WASAPI в режиме петли (loopback).
//!
//! Петля отдаёт ровно то, что микшер Windows посылает на устройство вывода
//! по умолчанию: ролик, созвон, музыку — без микрофона и без прав
//! администратора. Сэмплы уходят ffmpeg по TCP на петле (почему не через
//! stdin — в шапке `capture`).
//!
//! # Тишина, которую играет Savio
//!
//! Когда на компьютере ничего не звучит, петля не отдаёт **ничего** — даже
//! пакетов тишины. Время звука в файле считается по числу сэмплов, и каждая
//! пауза молча вырезалась бы из записи: звук после неё уезжал бы от картинки
//! вперёд на длину паузы. Поэтому рядом с петлёй Savio открывает второй
//! поток — воспроизведение тишины на то же устройство: микшер работает
//! непрерывно, и петля отдаёт пакеты всегда. Так же делает OBS. В микшере
//! громкости Windows на время записи появляется строка Savio — это она.
//!
//! COM здесь голый, через таблицы методов, как в `monitor::gpu`: крейт
//! `windows` ради пяти интерфейсов был бы зависимостью размером с проект.

use std::ffi::c_void;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Event;
use crate::i18n::{self, Key, Lang};
use crate::model::CaptureTrouble;

/// Как часто забирать пакеты. Буфер петли — секунда, так что спешить
/// незачем, а остановка ждёт не дольше одного такого шага.
///
/// Шаг выбран замером, а не на глаз: каждое пробуждение — полдюжины вызовов
/// WASAPI, и стоят они около 0.4–0.5 мс процессора. С шагом 10 мс поток
/// звука съедал 42–53 мс в секунду, втрое больше всего остального Savio во
/// время записи; с 30 мс — 12.5, со 100 мс — 5–6 (релиз, 2026-11-03,
/// Windows 11). На синхронизацию шаг не влияет: поправка начала считается
/// от запуска захвата, а время звука в файле — по числу сэмплов.
const POLL: Duration = Duration::from_millis(100);

/// Буфер обоих потоков WASAPI, в сотнях наносекунд: одна секунда.
/// С запасом на заминку ffmpeg — пока он не читает петлю, пакеты копятся
/// здесь, а не теряются.
const BUFFER_HNS: i64 = 10_000_000;

/// Сколько ждать, пока ffmpeg начнёт слушать петлю. Он открывает её вторым
/// входом, после видео, — это доли секунды.
const CONNECT_LIMIT: Duration = Duration::from_secs(15);

/// Сколько ждать строки «Input #0» после подключения. Она напечатана раньше,
/// чем ffmpeg начал слушать, и опоздать может лишь на чтение журнала.
const VIDEO_WAIT: Duration = Duration::from_secs(1);

/// Потолок тишины в начале. Замер — 0.36 с; больше двух секунд значит, что
/// мерили не то, и такая поправка навредила бы.
const LEAD_LIMIT: Duration = Duration::from_secs(2);

/// Как часто сверять, то ли устройство вывода стоит по умолчанию.
///
/// Раз в секунду: смену устройства делает человек, и секунду тишины на стыке
/// он не услышит. Сверка — пара вызовов WASAPI; с ней поток звука тратит
/// 6–9 мс процессора в секунду против 5–6 без неё (релиз, 2026-11-03), то
/// есть десятые доли процента ядра рядом с почти двумя ядрами у ffmpeg.
const FOLLOW_EVERY: Duration = Duration::from_secs(1);

/// Потолок тишины, которой закрывается одна дыра в звуке. Больше — значит,
/// часы или позиция устройства соврали, и такая «поправка» вклеила бы
/// в запись минуты тишины.
const GAP_LIMIT: Duration = Duration::from_secs(10);

/// Формат сэмплов микшера.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Format {
    /// Имя формата у ffmpeg: `f32le`, `s16le`…
    pub sample: &'static str,
    pub rate: u32,
    pub channels: u16,
    /// Байт на кадр — на все каналы одного мгновения.
    pub block: u16,
}

/// Аргументы звукового входа ffmpeg: сырые сэмплы с петли.
///
/// Частоту и каналы ffmpeg узнаёт только отсюда: у сырых сэмплов заголовка
/// нет, и промах здесь дал бы звук не той высоты, а не ошибку.
pub(super) fn input_args(format: &Format, port: u16) -> Vec<String> {
    vec![
        "-f".to_owned(),
        format.sample.to_owned(),
        "-ar".into(),
        format.rate.to_string(),
        "-ac".into(),
        format.channels.to_string(),
        // Очередь входа побольше: пакеты звука мелкие, а при умолчании
        // в восемь пакетов ffmpeg жалуется в журнал на каждую заминку.
        "-thread_queue_size".into(),
        "1024".into(),
        "-i".into(),
        format!("tcp://127.0.0.1:{port}?listen=1"),
    ]
}

/// Можно ли снять звук системы: открывается ли устройство вывода и понятен
/// ли его формат.
pub(super) fn probe() -> Result<Format, CaptureTrouble> {
    let _com = Apartment::enter()?;
    let devices = enumerator()?;
    let device = default_endpoint(&devices)?;
    let client = activate(&device)?;
    let mix = MixFormat::of(&client)?;
    Ok(mix.format)
}

/// Открывает звук в своём потоке и отдаёт его формат.
///
/// Поток живёт до конца записи: подключается к ffmpeg, ждёт, пока откроется
/// видео, и подаёт сэмплы, пока ffmpeg не закроет петлю или не поднимут
/// `abort`.
pub(super) fn spawn(
    port: u16,
    video_open: Option<Receiver<Instant>>,
    abort: Arc<AtomicBool>,
    tx: Sender<Event>,
    lang: Lang,
) -> Result<(Format, JoinHandle<()>), CaptureTrouble> {
    let (ready_tx, ready_rx) = sync_channel(1);
    let thread =
        std::thread::spawn(move || run(port, video_open, &abort, &tx, &ready_tx, lang));
    match ready_rx.recv() {
        Ok(Ok(format)) => Ok((format, thread)),
        Ok(Err(trouble)) => {
            let _ = thread.join();
            Err(trouble)
        }
        // Поток погиб, не ответив, — слов для хвоста нет, своих английских
        // на их место не ставим.
        Err(_) => Err(CaptureTrouble::NoSound(String::new())),
    }
}

fn run(
    port: u16,
    video_open: Option<Receiver<Instant>>,
    abort: &AtomicBool,
    tx: &Sender<Event>,
    ready: &SyncSender<Result<Format, CaptureTrouble>>,
    lang: Lang,
) {
    // Порядок объявлений — порядок уборки наоборот: потоки WASAPI
    // отпускаются раньше, чем поток выходит из COM.
    let _com = match Apartment::enter() {
        Ok(com) => com,
        Err(trouble) => {
            let _ = ready.send(Err(trouble));
            return;
        }
    };
    let opened = enumerator().and_then(|devices| {
        let device = default_endpoint(&devices)?;
        Ok((Streams::open(&device)?, devices))
    });
    let (streams, devices) = match opened {
        Ok(opened) => opened,
        Err(trouble) => {
            let _ = ready.send(Err(trouble));
            return;
        }
    };
    let format = streams.format;
    let _ = ready.send(Ok(format));

    let Some(mut socket) = connect(port, abort) else {
        return;
    };
    // Видео уже открыто: строка о нём напечатана раньше, чем ffmpeg начал
    // слушать петлю. Ждём лишь, пока её дочитает журнал.
    let video_at = video_open.and_then(|rx| rx.recv_timeout(VIDEO_WAIT).ok());

    let mut source = match streams.start() {
        Ok(()) => Source::Live(streams),
        Err(code) => {
            let _ = tx.send(Event::Warning(CaptureTrouble::SoundLost(code)));
            Source::Silent
        }
    };
    let started = Instant::now();

    // Поправка начала: видео в файле начинается с мгновения своего открытия,
    // звук — с первого сэмпла, а время звука — это число сэмплов. Значит,
    // перед первым настоящим сэмплом нужно ровно столько тишины, сколько
    // прошло между этими мгновениями (см. шапку `capture`).
    let lead = video_at.map_or(Duration::ZERO, |at| {
        started.saturating_duration_since(at).min(LEAD_LIMIT)
    });
    let _ = tx.send(Event::Log(i18n::fill(
        i18n::t(lang, Key::CaptureLogSound),
        &[
            &format.rate.to_string(),
            &format.channels.to_string(),
            format.sample,
            &format!("{:.3}", lead.as_secs_f64()),
        ],
    )));
    let mut feed = Feed {
        format,
        started,
        lead: frames_in(lead, format.rate),
        written: 0,
    };
    // Тишина начала — это догнать часы в самое первое мгновение.
    if feed.catch_up(&mut socket, started).is_err() {
        return;
    }

    let mut scratch = Vec::new();
    let mut refused = None;
    let mut checked = Instant::now();
    while !abort.load(Ordering::SeqCst) {
        let pumped = match &mut source {
            Source::Live(streams) => streams.pump(&mut socket, &mut scratch, &mut feed.written),
            // Звука нет, а вход стоять не должен: вставший вход остановил
            // бы и видео. Пока нечего писать — пишется тишина по часам.
            Source::Silent => feed
                .catch_up(&mut socket, Instant::now())
                .map_err(|_| Pump::Closed),
        };
        match pumped {
            Ok(()) => {}
            // ffmpeg закрыл петлю: запись кончилась, это обычный исход.
            Err(Pump::Closed) => return,
            // Устройство пропало — вынули наушники, отключили выход. Дальше
            // тишина, пока по умолчанию не встанет устройство, с которого
            // можно писать: его найдёт `follow`.
            Err(Pump::Device(code)) => {
                let _ = tx.send(Event::Warning(CaptureTrouble::SoundLost(code)));
                source = Source::Silent;
            }
        }
        if checked.elapsed() >= FOLLOW_EVERY {
            checked = Instant::now();
            let followed = follow(&devices, &mut source, &mut refused, &mut feed, &mut socket);
            match followed {
                Ok(Followed::Same) => {}
                Ok(Followed::Moved) => {
                    let _ = tx.send(Event::Log(
                        i18n::t(lang, Key::CaptureLogSoundMoved).to_owned(),
                    ));
                }
                Ok(Followed::Refused) => {
                    let _ = tx.send(Event::Warning(CaptureTrouble::SoundMoved));
                }
                Err(_) => return,
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Откуда сейчас берётся звук.
enum Source {
    /// Петля на устройстве вывода.
    Live(Streams),
    /// Петли нет: устройство пропало или звук ушёл на устройство, с которого
    /// эту запись не написать. Пишется тишина по часам.
    Silent,
}

/// Чем кончилась сверка с устройством по умолчанию.
enum Followed {
    /// Ничего не изменилось — или перейти было некуда.
    Same,
    /// Звук теперь берётся с нового устройства по умолчанию.
    Moved,
    /// Новое устройство по умолчанию есть, но формат у него другой: ffmpeg
    /// уже читает сэмплы в прежнем, и перейти на него нельзя.
    Refused,
}

/// Переходит на устройство вывода по умолчанию, если оно сменилось.
///
/// WASAPI этого сам не делает и о смене не сообщает: поток открыт на
/// конкретном устройстве и живёт, пока живо оно. Подключились наушники
/// и стали устройством по умолчанию — звук ушёл в них, а петля осталась на
/// колонках, где больше ничего не играет, и пишет тишину без единой ошибки
/// (Правило 6; так же поступает OBS — он слушает `OnDefaultDeviceChanged`).
///
/// `refused` — устройство, на которое уже не вышло перейти: пробовать его
/// каждую секунду незачем, пока по умолчанию не встанет другое.
fn follow(
    devices: &Com,
    source: &mut Source,
    refused: &mut Option<Vec<u16>>,
    feed: &mut Feed,
    socket: &mut TcpStream,
) -> std::io::Result<Followed> {
    // Устройства нет вовсе — переходить некуда, тишина идёт и так.
    let Ok(device) = default_endpoint(devices) else {
        return Ok(Followed::Same);
    };
    let Ok(id) = device_id(&device) else {
        return Ok(Followed::Same);
    };
    let current = match source {
        Source::Live(streams) => Some(&streams.device_id),
        Source::Silent => None,
    };
    if current == Some(&id) || refused.as_ref() == Some(&id) {
        return Ok(Followed::Same);
    }
    let next = match Streams::open(&device) {
        Ok(next) if next.format == feed.format => next,
        Ok(_) => {
            *refused = Some(id);
            return Ok(Followed::Refused);
        }
        // Не открылось — значит, и писать с него нечем. Скажет об этом
        // тишина в файле; предупреждение о пропаже звука уже прозвучало.
        Err(_) => {
            *refused = Some(id);
            return Ok(Followed::Same);
        }
    };
    if next.start().is_err() {
        *refused = Some(id);
        return Ok(Followed::Same);
    }
    // Прежние потоки останавливаются здесь же, при замене. Стык закрывается
    // тишиной до этого мгновения: звук нового устройства начнётся с него.
    *source = Source::Live(next);
    *refused = None;
    feed.catch_up(socket, Instant::now())?;
    Ok(Followed::Moved)
}

/// Сколько звука отдано ffmpeg — и сколько должно быть по часам.
///
/// Время звука в файле — это число кадров, а не часы, поэтому всякий
/// промежуток без пакетов (пропавшее устройство, стык при переходе на новое)
/// закрывается тишиной ровно до того мгновения, которое показывают часы.
struct Feed {
    format: Format,
    /// Мгновение запуска петли: от него отсчитывается время звука.
    started: Instant,
    /// Тишина в начала записи, в кадрах.
    lead: u64,
    /// Сколько кадров отдано всего, вместе с тишиной.
    written: u64,
}

impl Feed {
    /// Сколько кадров должно быть отдано к мгновению `now`.
    fn due(&self, now: Instant) -> u64 {
        self.lead + frames_in(now.saturating_duration_since(self.started), self.format.rate)
    }

    /// Догоняет часы тишиной. Ушедший вперёд звук не обрезается: лишние
    /// доли секунды безвреднее, чем дыра посреди сэмпла.
    fn catch_up(&mut self, socket: &mut TcpStream, now: Instant) -> std::io::Result<()> {
        let missing = self
            .due(now)
            .saturating_sub(self.written)
            .min(frames_in(GAP_LIMIT, self.format.rate));
        write_silence(socket, missing, self.format.block)?;
        self.written += missing;
        Ok(())
    }
}

/// Сколько кадров в таком промежутке при такой частоте.
fn frames_in(span: Duration, rate: u32) -> u64 {
    (span.as_secs_f64() * f64::from(rate)) as u64
}

/// Пишет `frames` кадров тишины кусками, а не одним буфером: дыра бывает
/// в несколько секунд, и мегабайт нулей ради неё заводить незачем.
fn write_silence(socket: &mut TcpStream, frames: u64, block: u16) -> std::io::Result<()> {
    const CHUNK: u64 = 4096;
    let zeros = vec![0; (frames.min(CHUNK) as usize) * usize::from(block)];
    let mut left = frames;
    while left > 0 {
        let now = left.min(CHUNK);
        socket.write_all(&zeros[..now as usize * usize::from(block)])?;
        left -= now;
    }
    Ok(())
}

/// Подключается к ffmpeg, слушающему петлю.
///
/// Короткий потолок на каждую попытку не для скорости, а для точности:
/// подключение к ещё не слушающему порту Windows повторяет само и может
/// продержать его полсекунды, а каждая лишняя доля секунды здесь — это
/// лишняя тишина в начале записи.
fn connect(port: u16, abort: &AtomicBool) -> Option<TcpStream> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let started = Instant::now();
    while !abort.load(Ordering::SeqCst) && started.elapsed() < CONNECT_LIMIT {
        if let Ok(socket) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            return Some(socket);
        }
        std::thread::sleep(POLL);
    }
    None
}

/// Формат из `WAVEFORMATEX` (или `WAVEFORMATEXTENSIBLE`) байтами.
///
/// Байтами, а не структурой: `WAVEFORMATEX` упакована по одному байту —
/// 18 байт, а не 20, — и в `WAVEFORMATEXTENSIBLE` всё, что идёт за ней,
/// лежит на два байта раньше, чем положила бы Rust-структура с обычным
/// выравниванием (`Samples` — на 18-м байте, а не на 20-м). Сами поля
/// `WAVEFORMATEX` от упаковки не сдвигаются.
fn parse_format(bytes: &[u8]) -> Option<Format> {
    const TAG_PCM: u16 = 1;
    const TAG_FLOAT: u16 = 3;
    const TAG_EXTENSIBLE: u16 = 0xFFFE;
    /// Хвост `KSDATAFORMAT_SUBTYPE_*` после первого поля GUID — общий у PCM
    /// и float: `-0000-0010-8000-00aa00389b71`.
    const SUBTYPE_TAIL: [u8; 12] = [
        0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ];

    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));

    let mut tag = u16_at(0)?;
    let channels = u16_at(2)?;
    let rate = u32_at(4)?;
    let block = u16_at(12)?;
    let bits = u16_at(14)?;
    if tag == TAG_EXTENSIBLE {
        if u16_at(16)? < 22 || bytes.get(28..40)? != SUBTYPE_TAIL {
            return None;
        }
        tag = u16::try_from(u32_at(24)?).ok()?;
    }
    let sample = match (tag, bits) {
        (TAG_FLOAT, 32) => "f32le",
        (TAG_FLOAT, 64) => "f64le",
        (TAG_PCM, 16) => "s16le",
        (TAG_PCM, 24) => "s24le",
        // 24 значащих бита в 32-битной ячейке тоже сюда: значение лежит
        // в старших битах, и как 32-битное оно читается верно.
        (TAG_PCM, 32) => "s32le",
        _ => return None,
    };
    if channels == 0 || rate == 0 || u32::from(block) != u32::from(channels) * u32::from(bits) / 8 {
        return None;
    }
    Some(Format {
        sample,
        rate,
        channels,
        block,
    })
}

// ---------------------------------------------------------------------------
// COM
// ---------------------------------------------------------------------------

#[repr(C)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const CLSID_MM_DEVICE_ENUMERATOR: Guid = Guid {
    d1: 0xBCDE_0395,
    d2: 0xE52F,
    d3: 0x467C,
    d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
};
const IID_IMM_DEVICE_ENUMERATOR: Guid = Guid {
    d1: 0xA956_64D2,
    d2: 0x9614,
    d3: 0x4F35,
    d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
};
const IID_IAUDIO_CLIENT: Guid = Guid {
    d1: 0x1CB9_AD4C,
    d2: 0xDBFA,
    d3: 0x4C32,
    d4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
};
const IID_IAUDIO_CAPTURE_CLIENT: Guid = Guid {
    d1: 0xC8AD_BD64,
    d2: 0xE71E,
    d3: 0x48A0,
    d4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17],
};
const IID_IAUDIO_RENDER_CLIENT: Guid = Guid {
    d1: 0xF294_ACFC,
    d2: 0x3146,
    d3: 0x4483,
    d4: [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2],
};

const COINIT_MULTITHREADED: u32 = 0;
const CLSCTX_ALL: u32 = 0x17;
/// `eRender` и `eConsole` из `EDataFlow` и `ERole`.
const FLOW_RENDER: i32 = 0;
const ROLE_CONSOLE: i32 = 0;
const SHAREMODE_SHARED: i32 = 0;
const STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
/// Пакет не продолжает предыдущий: петлю не успели забрать, и часть звука
/// пропала, — позиция устройства в нём ушла вперёд на длину пропажи.
const BUFFERFLAGS_DATA_DISCONTINUITY: u32 = 0x1;
const BUFFERFLAGS_SILENT: u32 = 0x2;
/// `E_NOTFOUND` — у `GetDefaultAudioEndpoint` это «устройства вывода нет».
const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;

// Номера в таблицах методов, считая три метода IUnknown.
const SLOT_RELEASE: usize = 2;
/// `IMMDeviceEnumerator::GetDefaultAudioEndpoint`.
const SLOT_DEFAULT_ENDPOINT: usize = 4;
/// `IMMDevice::Activate` и `IMMDevice::GetId`.
const SLOT_ACTIVATE: usize = 3;
const SLOT_GET_ID: usize = 5;
/// `IAudioClient`: `Initialize` 3, `GetBufferSize` 4, `GetCurrentPadding` 6,
/// `GetMixFormat` 8, `Start` 10, `Stop` 11, `GetService` 14.
const SLOT_INITIALIZE: usize = 3;
const SLOT_BUFFER_SIZE: usize = 4;
const SLOT_PADDING: usize = 6;
const SLOT_MIX_FORMAT: usize = 8;
const SLOT_START: usize = 10;
const SLOT_STOP: usize = 11;
const SLOT_SERVICE: usize = 14;
/// `IAudioCaptureClient` и `IAudioRenderClient`: `GetBuffer` 3,
/// `ReleaseBuffer` 4, у петли ещё `GetNextPacketSize` 5.
const SLOT_GET_BUFFER: usize = 3;
const SLOT_RELEASE_BUFFER: usize = 4;
const SLOT_NEXT_PACKET: usize = 5;

type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type DefaultEndpointFn = unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> i32;
type ActivateFn =
    unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> i32;
type GetIdFn = unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32;
type InitializeFn =
    unsafe extern "system" fn(*mut c_void, i32, u32, i64, i64, *const c_void, *const Guid) -> i32;
type CountFn = unsafe extern "system" fn(*mut c_void, *mut u32) -> i32;
type MixFormatFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32;
type ActionFn = unsafe extern "system" fn(*mut c_void) -> i32;
type ServiceFn = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
type CaptureGetBufferFn = unsafe extern "system" fn(
    *mut c_void,
    *mut *mut u8,
    *mut u32,
    *mut u32,
    *mut u64,
    *mut u64,
) -> i32;
type CaptureReleaseFn = unsafe extern "system" fn(*mut c_void, u32) -> i32;
type RenderGetBufferFn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> i32;
type RenderReleaseFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, model: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        context: u32,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> i32;
    fn CoTaskMemFree(memory: *mut c_void);
}

/// Код ответа словами для журнала: `HRESULT 0x88890004`.
fn hresult(code: i32) -> String {
    format!("HRESULT 0x{:08X}", code as u32)
}

fn check(code: i32) -> Result<(), String> {
    if code < 0 { Err(hresult(code)) } else { Ok(()) }
}

/// Поток в многопоточной квартире COM — на время жизни значения.
///
/// Не `Send` намеренно: `CoUninitialize` обязан звучать в том же потоке,
/// что и `CoInitializeEx`, и унести значение в другой поток не даст
/// компилятор.
struct Apartment(std::marker::PhantomData<*const ()>);

impl Apartment {
    fn enter() -> Result<Self, CaptureTrouble> {
        let code = unsafe { CoInitializeEx(null_mut(), COINIT_MULTITHREADED) };
        check(code).map_err(CaptureTrouble::NoSound)?;
        Ok(Apartment(std::marker::PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        // Парой к каждому удачному `CoInitializeEx`, включая `S_FALSE`.
        unsafe { CoUninitialize() };
    }
}

/// Указатель на объект COM, который отпускается сам.
struct Com(*mut c_void);

impl Com {
    /// Метод из таблицы объекта.
    ///
    /// # Safety
    /// `F` — тип функции ровно того метода, что стоит в `slot`.
    unsafe fn method<F: Copy>(&self, slot: usize) -> F {
        // Объект COM начинается с указателя на таблицу методов, а таблица —
        // массив указателей на функции.
        unsafe {
            let table = *self.0.cast::<*const *const c_void>();
            std::mem::transmute_copy(&*table.add(slot))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let release: ReleaseFn = unsafe { self.method(SLOT_RELEASE) };
            unsafe { release(self.0) };
        }
    }
}

/// Перечислитель устройств. Один на всю запись: устройство по умолчанию
/// спрашивают у него раз в секунду ([`follow`]).
fn enumerator() -> Result<Com, CaptureTrouble> {
    let mut raw = null_mut();
    let code = unsafe {
        CoCreateInstance(
            &CLSID_MM_DEVICE_ENUMERATOR,
            null_mut(),
            CLSCTX_ALL,
            &IID_IMM_DEVICE_ENUMERATOR,
            &mut raw,
        )
    };
    check(code).map_err(CaptureTrouble::NoSound)?;
    Ok(Com(raw))
}

/// Устройство вывода по умолчанию — то, куда сейчас идёт звук.
fn default_endpoint(devices: &Com) -> Result<Com, CaptureTrouble> {
    let default_endpoint: DefaultEndpointFn = unsafe { devices.method(SLOT_DEFAULT_ENDPOINT) };
    let mut raw = null_mut();
    let code = unsafe { default_endpoint(devices.0, FLOW_RENDER, ROLE_CONSOLE, &mut raw) };
    if code == E_NOTFOUND {
        return Err(CaptureTrouble::NoSpeakers);
    }
    check(code).map_err(CaptureTrouble::NoSound)?;
    Ok(Com(raw))
}

/// Постоянный идентификатор устройства — по нему видно, что по умолчанию
/// встало другое.
fn device_id(device: &Com) -> Result<Vec<u16>, String> {
    let get_id: GetIdFn = unsafe { device.method(SLOT_GET_ID) };
    let mut raw = null_mut();
    check(unsafe { get_id(device.0, &mut raw) })?;
    if raw.is_null() {
        return Err("IMMDevice::GetId".to_owned());
    }
    // Строка заканчивается нулём; потолок — на случай, если не заканчивается.
    let mut id = Vec::new();
    for index in 0..1024 {
        let unit = unsafe { *raw.add(index) };
        if unit == 0 {
            break;
        }
        id.push(unit);
    }
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(id)
}

fn activate(device: &Com) -> Result<Com, CaptureTrouble> {
    let activate: ActivateFn = unsafe { device.method(SLOT_ACTIVATE) };
    let mut raw = null_mut();
    let code = unsafe { activate(device.0, &IID_IAUDIO_CLIENT, CLSCTX_ALL, null_mut(), &mut raw) };
    check(code).map_err(CaptureTrouble::NoSound)?;
    Ok(Com(raw))
}

fn service(client: &Com, iid: &Guid) -> Result<Com, CaptureTrouble> {
    let get_service: ServiceFn = unsafe { client.method(SLOT_SERVICE) };
    let mut raw = null_mut();
    check(unsafe { get_service(client.0, iid, &mut raw) }).map_err(CaptureTrouble::NoSound)?;
    Ok(Com(raw))
}

/// Формат микшера — память COM, которую надо вернуть самим.
struct MixFormat {
    raw: *mut c_void,
    format: Format,
}

impl MixFormat {
    fn of(client: &Com) -> Result<Self, CaptureTrouble> {
        let get_mix_format: MixFormatFn = unsafe { client.method(SLOT_MIX_FORMAT) };
        let mut raw = null_mut();
        check(unsafe { get_mix_format(client.0, &mut raw) }).map_err(CaptureTrouble::NoSound)?;
        // Владелец памяти заводится сразу и единственный: ошибка разбора ниже
        // вернёт её через `Drop`. Собрать результат заново из его полей
        // (`MixFormat { format, ..mix }`) нельзя — указатель копируется,
        // и память освобождалась дважды: тест падал `STATUS_HEAP_CORRUPTION`.
        let mut mix = MixFormat {
            raw,
            format: Format {
                sample: "",
                rate: 0,
                channels: 0,
                block: 0,
            },
        };
        // Длину хвоста называет сам заголовок (`cbSize`), а он — 16-й и 17-й
        // байты восемнадцатибайтного `WAVEFORMATEX`.
        let head = unsafe { std::slice::from_raw_parts(raw.cast::<u8>(), 18) };
        let extra = usize::from(u16::from_le_bytes([head[16], head[17]]));
        let bytes = unsafe { std::slice::from_raw_parts(raw.cast::<u8>(), 18 + extra) };
        mix.format = parse_format(bytes).ok_or_else(|| {
            CaptureTrouble::NoSound(format!(
                "WAVEFORMATEX {:02x?}",
                &bytes[..bytes.len().min(40)]
            ))
        })?;
        Ok(mix)
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.raw) };
    }
}

/// Петля и тишина на одном устройстве.
struct Streams {
    format: Format,
    /// Чьи это потоки — чтобы заметить, что по умолчанию встало другое.
    device_id: Vec<u16>,
    capture_client: Com,
    capture: Com,
    render_client: Com,
    render: Com,
    /// Размер буфера тишины в кадрах.
    render_frames: u32,
    /// Где по позиции устройства должен начаться следующий пакет петли.
    /// `None` — пакетов ещё не было.
    next_position: Option<u64>,
}

/// Почему подача звука остановилась.
enum Pump {
    /// ffmpeg закрыл петлю.
    Closed,
    /// WASAPI ответил ошибкой.
    Device(String),
}

impl Streams {
    fn open(device: &Com) -> Result<Self, CaptureTrouble> {
        let device_id = device_id(device).map_err(CaptureTrouble::NoSound)?;
        let capture_client = activate(device)?;
        let mix = MixFormat::of(&capture_client)?;
        let initialize: InitializeFn = unsafe { capture_client.method(SLOT_INITIALIZE) };
        let code = unsafe {
            initialize(
                capture_client.0,
                SHAREMODE_SHARED,
                STREAMFLAGS_LOOPBACK,
                BUFFER_HNS,
                0,
                mix.raw,
                null(),
            )
        };
        check(code).map_err(CaptureTrouble::NoSound)?;
        let capture = service(&capture_client, &IID_IAUDIO_CAPTURE_CLIENT)?;

        let render_client = activate(device)?;
        let initialize: InitializeFn = unsafe { render_client.method(SLOT_INITIALIZE) };
        let code = unsafe {
            initialize(
                render_client.0,
                SHAREMODE_SHARED,
                0,
                BUFFER_HNS,
                0,
                mix.raw,
                null(),
            )
        };
        check(code).map_err(CaptureTrouble::NoSound)?;
        let buffer_size: CountFn = unsafe { render_client.method(SLOT_BUFFER_SIZE) };
        let mut render_frames = 0;
        check(unsafe { buffer_size(render_client.0, &mut render_frames) })
            .map_err(CaptureTrouble::NoSound)?;
        let render = service(&render_client, &IID_IAUDIO_RENDER_CLIENT)?;

        let streams = Streams {
            format: mix.format,
            device_id,
            capture_client,
            capture,
            render_client,
            render,
            render_frames,
            next_position: None,
        };
        streams.top_up().map_err(CaptureTrouble::NoSound)?;
        Ok(streams)
    }

    fn start(&self) -> Result<(), String> {
        let start: ActionFn = unsafe { self.render_client.method(SLOT_START) };
        check(unsafe { start(self.render_client.0) })?;
        let start: ActionFn = unsafe { self.capture_client.method(SLOT_START) };
        check(unsafe { start(self.capture_client.0) })
    }

    /// Доливает тишину в буфер воспроизведения — сколько в нём свободно.
    fn top_up(&self) -> Result<(), String> {
        let padding: CountFn = unsafe { self.render_client.method(SLOT_PADDING) };
        let mut queued = 0;
        check(unsafe { padding(self.render_client.0, &mut queued) })?;
        let free = self.render_frames.saturating_sub(queued);
        if free == 0 {
            return Ok(());
        }
        let get_buffer: RenderGetBufferFn = unsafe { self.render.method(SLOT_GET_BUFFER) };
        let release: RenderReleaseFn = unsafe { self.render.method(SLOT_RELEASE_BUFFER) };
        let mut data = null_mut();
        check(unsafe { get_buffer(self.render.0, free, &mut data) })?;
        // Флаг «тишина» вместо нулей в буфере: микшер сам знает, что играть.
        check(unsafe { release(self.render.0, free, BUFFERFLAGS_SILENT) })
    }

    /// Забирает всё, что накопила петля, и отдаёт ffmpeg. `written` растёт
    /// на каждый отданный кадр, вместе с тишиной на месте дыр.
    ///
    /// Пакет копируется и отпускается **до** записи в сокет: так буфер
    /// WASAPI освобождается сразу, хоть ffmpeg и замешкается с чтением.
    ///
    /// Дыра — пакет с флагом `DATA_DISCONTINUITY`: петлю не успели забрать
    /// за секунду её буфера, и часть звука пропала. Время звука в файле —
    /// число кадров, и без поправки весь звук после дыры уехал бы вперёд
    /// картинки на её длину. Длину называет позиция устройства: она
    /// продолжает считать и то, что пропало. Позицию берём **только** при
    /// флаге — без него она доверия не требует, а ошибись мы в её смысле,
    /// в запись вклеилась бы чужая тишина.
    fn pump(
        &mut self,
        socket: &mut TcpStream,
        scratch: &mut Vec<u8>,
        written: &mut u64,
    ) -> Result<(), Pump> {
        self.top_up().map_err(Pump::Device)?;
        let next_packet: CountFn = unsafe { self.capture.method(SLOT_NEXT_PACKET) };
        let get_buffer: CaptureGetBufferFn = unsafe { self.capture.method(SLOT_GET_BUFFER) };
        let release: CaptureReleaseFn = unsafe { self.capture.method(SLOT_RELEASE_BUFFER) };
        let gap_limit = frames_in(GAP_LIMIT, self.format.rate);
        loop {
            let mut packet = 0;
            check(unsafe { next_packet(self.capture.0, &mut packet) }).map_err(Pump::Device)?;
            if packet == 0 {
                return Ok(());
            }
            let (mut data, mut frames, mut flags, mut position) = (null_mut(), 0, 0, 0);
            let code = unsafe {
                get_buffer(
                    self.capture.0,
                    &mut data,
                    &mut frames,
                    &mut flags,
                    &mut position,
                    null_mut(),
                )
            };
            check(code).map_err(Pump::Device)?;
            let gap = match self.next_position {
                Some(next) if flags & BUFFERFLAGS_DATA_DISCONTINUITY != 0 => {
                    position.saturating_sub(next).min(gap_limit)
                }
                _ => 0,
            };
            self.next_position = Some(position + u64::from(frames));
            let bytes = frames as usize * usize::from(self.format.block);
            scratch.clear();
            if flags & BUFFERFLAGS_SILENT != 0 || data.is_null() {
                scratch.resize(bytes, 0);
            } else {
                scratch.extend_from_slice(unsafe { std::slice::from_raw_parts(data, bytes) });
            }
            check(unsafe { release(self.capture.0, frames) }).map_err(Pump::Device)?;
            write_silence(socket, gap, self.format.block).map_err(|_| Pump::Closed)?;
            socket.write_all(scratch).map_err(|_| Pump::Closed)?;
            *written += gap + u64::from(frames);
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        for client in [&self.capture_client, &self.render_client] {
            let stop: ActionFn = unsafe { client.method(SLOT_STOP) };
            unsafe { stop(client.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `WAVEFORMATEXTENSIBLE` микшера, как его отдаёт Windows 11 почти на
    /// любой машине: float32, 48 кГц, стерео.
    fn extensible(subtype: u32, bits: u16, channels: u16) -> Vec<u8> {
        let block = channels * bits / 8;
        let mut bytes = Vec::new();
        bytes.extend(0xFFFE_u16.to_le_bytes());
        bytes.extend(channels.to_le_bytes());
        bytes.extend(48_000_u32.to_le_bytes());
        bytes.extend((48_000 * u32::from(block)).to_le_bytes());
        bytes.extend(block.to_le_bytes());
        bytes.extend(bits.to_le_bytes());
        bytes.extend(22_u16.to_le_bytes());
        bytes.extend(bits.to_le_bytes());
        bytes.extend(3_u32.to_le_bytes());
        bytes.extend(subtype.to_le_bytes());
        bytes.extend([
            0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
        ]);
        bytes
    }

    #[test]
    fn the_usual_mixer_format_is_float_stereo() {
        assert_eq!(
            parse_format(&extensible(3, 32, 2)),
            Some(Format {
                sample: "f32le",
                rate: 48_000,
                channels: 2,
                block: 8,
            })
        );
        // 7.1 тоже разбирается: свести каналы в стерео — работа ffmpeg.
        assert_eq!(parse_format(&extensible(3, 32, 8)).map(|f| f.channels), Some(8));
        assert_eq!(parse_format(&extensible(1, 16, 2)).map(|f| f.sample), Some("s16le"));
    }

    /// Чужой подтип не выдаётся за float: сэмплы прочитались бы шумом.
    #[test]
    fn an_unknown_subtype_is_refused() {
        let mut bytes = extensible(3, 32, 2);
        bytes[39] ^= 0xFF;
        assert_eq!(parse_format(&bytes), None);
        assert_eq!(parse_format(&bytes[..10]), None);
    }

    /// Частота и каналы попадают в команду: у сырых сэмплов заголовка нет.
    #[test]
    fn the_input_names_rate_channels_and_the_port() {
        let args = input_args(
            &Format {
                sample: "f32le",
                rate: 44_100,
                channels: 6,
                block: 24,
            },
            50123,
        );
        assert!(args.windows(2).any(|w| w == ["-f", "f32le"]));
        assert!(args.windows(2).any(|w| w == ["-ar", "44100"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "6"]));
        assert_eq!(
            args.last().map(String::as_str),
            Some("tcp://127.0.0.1:50123?listen=1")
        );
    }

    /// Приёмник на петле вместо ffmpeg: считает, сколько байт ему отдали.
    fn sink() -> (TcpStream, JoinHandle<u64>) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let reader = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let mut total = 0u64;
            while let Ok(n @ 1..) = std::io::Read::read(&mut stream, &mut buf) {
                total += n as u64;
            }
            total
        });
        (TcpStream::connect(address).unwrap(), reader)
    }

    /// Забирает петлю шагами `POLL`, как `run`, в течение `span`.
    fn pump_for(source: &mut Source, socket: &mut TcpStream, written: &mut u64, span: Duration) {
        let mut scratch = Vec::new();
        let until = Instant::now() + span;
        while Instant::now() < until {
            if let Source::Live(streams) = source {
                assert!(streams.pump(socket, &mut scratch, written).is_ok(), "петля встала");
            }
            std::thread::sleep(POLL);
        }
    }

    /// Звука в итоге столько, сколько прошло времени, — с поправкой на то,
    /// что за последний шаг ещё лежит в буфере петли.
    fn assert_kept_time(started: Instant, written: u64, format: Format, socket: TcpStream, reader: JoinHandle<u64>) {
        let elapsed = started.elapsed().as_secs_f64();
        drop(socket);
        let bytes = reader.join().unwrap();
        assert_eq!(bytes, written * u64::from(format.block), "отдано не то, что насчитано");
        let got = written as f64 / f64::from(format.rate);
        eprintln!("прошло {elapsed:.3} с, звука {got:.3} с");
        assert!(
            (elapsed - got).abs() < 0.3,
            "прошло {elapsed:.3} с, а звука {got:.3} с: время звука уехало"
        );
    }

    /// Петлю не забирали дольше её буфера — WASAPI выбросил звук, а время
    /// звука в файле всё равно совпадает с часами: дыра закрыта тишиной по
    /// позиции устройства. Без поправки звука вышло бы на две секунды меньше.
    #[test]
    #[ignore = "нужна звуковая карта; снимает звук этой машины"]
    fn a_stalled_loopback_keeps_its_time() {
        let _com = Apartment::enter().unwrap();
        let devices = enumerator().unwrap();
        let streams = Streams::open(&default_endpoint(&devices).unwrap()).unwrap();
        let format = streams.format;
        let (mut socket, reader) = sink();
        streams.start().unwrap();
        let started = Instant::now();
        let mut source = Source::Live(streams);
        let mut written = 0;
        pump_for(&mut source, &mut socket, &mut written, Duration::from_millis(500));
        // Буфер петли — секунда; три секунды без забора его переполняют.
        std::thread::sleep(Duration::from_secs(3));
        pump_for(&mut source, &mut socket, &mut written, Duration::from_millis(500));
        assert_kept_time(started, written, format, socket, reader);
    }

    /// По умолчанию «встало другое устройство» — сверка переходит на то, что
    /// стоит по умолчанию сейчас, и закрывает стык тишиной до этого
    /// мгновения. Чужое устройство изображает подменённый идентификатор:
    /// менять настоящие настройки звука ради проверки нельзя. Пауза перед
    /// сверкой — то, что потерялось бы на стыке без поправки.
    #[test]
    #[ignore = "нужна звуковая карта; снимает звук этой машины"]
    fn the_loopback_follows_a_new_default_device() {
        let _com = Apartment::enter().unwrap();
        let devices = enumerator().unwrap();
        let streams = Streams::open(&default_endpoint(&devices).unwrap()).unwrap();
        let format = streams.format;
        let (mut socket, reader) = sink();
        streams.start().unwrap();
        let started = Instant::now();
        let mut feed = Feed {
            format,
            started,
            lead: 0,
            written: 0,
        };
        let mut source = Source::Live(streams);
        pump_for(&mut source, &mut socket, &mut feed.written, Duration::from_millis(500));
        if let Source::Live(streams) = &mut source {
            streams.device_id = vec![0];
        }
        std::thread::sleep(Duration::from_secs(1));
        let mut refused = None;
        let moved = follow(&devices, &mut source, &mut refused, &mut feed, &mut socket).unwrap();
        assert!(matches!(moved, Followed::Moved), "сверка не перешла на устройство");
        assert!(refused.is_none());
        pump_for(&mut source, &mut socket, &mut feed.written, Duration::from_millis(500));
        assert_kept_time(started, feed.written, format, socket, reader);
    }

    /// Тишина по часам: сколько прошло, столько и дописано, не больше
    /// потолка дыры.
    #[test]
    fn silence_catches_up_with_the_clock_and_no_further() {
        let (mut socket, reader) = sink();
        let format = Format {
            sample: "f32le",
            rate: 48_000,
            channels: 2,
            block: 8,
        };
        let started = Instant::now();
        let mut feed = Feed {
            format,
            started,
            lead: 4800,
            written: 0,
        };
        feed.catch_up(&mut socket, started).unwrap();
        assert_eq!(feed.written, 4800, "тишина начала не дописана");
        feed.catch_up(&mut socket, started + Duration::from_millis(500)).unwrap();
        assert_eq!(feed.written, 4800 + 24_000);
        // Ушедший вперёд звук не обрезается и не догоняется повторно.
        feed.written += 1000;
        feed.catch_up(&mut socket, started + Duration::from_millis(500)).unwrap();
        assert_eq!(feed.written, 4800 + 24_000 + 1000);
        // Час без звука — это не дыра в десять минут тишины, а потолок.
        feed.catch_up(&mut socket, started + Duration::from_secs(3600)).unwrap();
        let limit = frames_in(GAP_LIMIT, format.rate);
        assert_eq!(feed.written, 4800 + 24_000 + 1000 + limit);
        drop(socket);
        assert_eq!(reader.join().unwrap(), feed.written * 8 - 1000 * 8);
    }

    /// Петля этой машины открывается и называет понятный формат.
    #[test]
    #[ignore = "нужна звуковая карта; на сервере CI её нет"]
    fn the_loopback_of_this_machine_opens() {
        let format = probe().expect("петля не открылась");
        eprintln!("{format:?}");
        assert!(format.rate >= 8000 && format.channels >= 1);
    }
}
