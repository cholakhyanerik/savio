//! Раздача файлов по своей сети: телефон открывает страницу браузером.
//!
//! Слой «Движок»: сокеты, разбор HTTP, файлы. Про `egui` модуль не знает
//! ничего — наружу уезжают `ShareEvent`, а экран рисует `app.rs`.
//!
//! # Кто кого находит
//!
//! Слушает компьютер, подключается телефон. Обратного пути нет: браузер на
//! телефоне снаружи никто не запускает, а «телефоны в сети» ничего не
//! слушают, так что искать их бессмысленно. Всё, что может Savio, — назвать
//! свой адрес (текстом и QR-кодом).
//!
//! # Почему HTTP руками
//!
//! Нужны ровно строка запроса, несколько заголовков и тело потоком в обе
//! стороны. Это меньше кода, чем объяснение, зачем ради этого веб-фреймворк
//! с асинхронной средой. Соединение на каждый запрос одно (`Connection:
//! close`): на своей сети новое TCP-подключение стоит доли миллисекунды,
//! а простаивающие keep-alive подключения занимали бы место под
//! `CONNECTION_LIMIT` и держали бы поток, который надо ещё и будить при
//! остановке.
//!
//! # Правило 6 в этом модуле
//!
//! `TcpListener::bind` проходит успешно всегда, даже когда брандмауэр не
//! пустит снаружи ни одного подключения. Проверить это изнутри нечем:
//! подключение к своему же адресу идёт мимо брандмауэра. Поэтому единственное
//! доказательство того, что раздача видна, — `ShareEvent::Visitor`, то есть
//! настоящий запрос с другой машины, а об остальном экран говорит словами.
//!
//! # Остановка
//!
//! Забытая раздача опаснее всего тем, что о ней не помнят: папка открыта всей
//! сети. Поэтому `Handle::stop` не просит, а обрывает — и ожидание в `accept`,
//! и каждое открытое подключение вместе с идущей передачей (подробности
//! у `Shared`).

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::i18n::{self, Key, Lang};
use crate::model::{Event, ShareAddress, ShareEvent, TransferDirection};

/// Страница для телефона. Одна, со встроенными стилями и скриптом: всё,
/// что лежит отдельным файлом, пришлось бы раздавать отдельным маршрутом.
const PAGE: &str = include_str!("../../assets/share/index.html");

/// Какие порты пробуем по порядку.
///
/// Сначала круглые и привычные: адрес с ними проще продиктовать. Занято всё —
/// берём любой свободный (ноль в конце), раздача важнее красоты числа.
const PORTS: [u16; 6] = [8080, 8081, 8082, 8088, 8090, 0];

/// Сколько подключений обслуживаем разом.
///
/// Каждое — свой поток. Телефон открывает их несколько (страница, список,
/// видео с перемоткой, отправка), так что тесно не будет, а чужая машина в
/// сети не заведёт тысячу потоков одним циклом.
const CONNECTION_LIMIT: usize = 32;

/// Потолок заголовка запроса. Настоящий запрос браузера — полкилобайта.
const HEAD_LIMIT: usize = 16 * 1024;

/// Сколько молчания терпим от подключения.
///
/// Минута, а не секунды: телефон с погасшим экраном притормаживает сеть, и
/// обрывать из-за этого гигабайтное видео на середине было бы обидно.
/// А вот вечным ожидание быть не должно — мёртвое подключение держало бы
/// место под `CONNECTION_LIMIT` до конца раздачи.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Кусок, которым гоняем данные. Он же — как часто смотрим на остановку.
const CHUNK: usize = 256 * 1024;

/// Как часто сообщаем о ходе передачи. Каждое сообщение — это кадр окна,
/// и четырёх в секунду полосе хватает, чтобы ехать плавно.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Длина ключа в адресе.
///
/// Двенадцать знаков из алфавита в 32 — шестьдесят бит. Перебирать их по сети
/// — годы даже на тысяче запросов в секунду, а раздача живёт минуты. Длиннее
/// не нужно: если QR-код не читается, адрес набирают руками.
const KEY_LEN: usize = 12;

/// Алфавит ключа: без `0`, `o`, `1` и `l`, которые на экране телефона путают.
const KEY_ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// Потолок длины имени файла в байтах UTF-8.
///
/// ext4 держит 255 байт на имя, NTFS — 255 знаков UTF-16. 180 оставляют
/// запас на « (999)» при совпадении и не упираются ни в один из пределов.
const NAME_LIMIT: usize = 180;

/// Сколько файлов показываем в списке «Забрать с компьютера».
const LIST_LIMIT: usize = 2000;

/// Начало имени временного файла, пока он принимается.
///
/// С точки: на Unix такой файл скрыт, а список на странице точку в начале
/// не показывает нигде. Недокачанный файл не должен выглядеть готовым —
/// приём тот же, что в `engine::cut`.
const TEMP_PREFIX: &str = ".savio-upload-";

/// Ручка раздачи: пока её не попросили, сервер работает.
pub struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    /// Останавливает раздачу: закрывает порт и обрывает все подключения,
    /// включая идущие передачи. Недопринятые файлы удаляются.
    ///
    /// Возвращает управление сразу. Звать можно сколько угодно раз.
    pub fn stop(&self) {
        self.shared.stop();
    }

    /// Останавливает и ждёт, пока подключения доделают уборку, но не дольше
    /// `limit`.
    ///
    /// Только для закрытия окна. Там eframe сразу за `on_exit` зовёт
    /// `process::exit`, и поток приёма, не успевший удалить недопринятый
    /// файл, оставил бы его в папке под временным именем. В остальное время
    /// ждать незачем: поток доделает своё и без нас.
    pub fn stop_and_wait(&self, limit: Duration) {
        self.shared.stop();
        let started = Instant::now();
        while self.shared.active.load(Ordering::SeqCst) > 0 && started.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Общее у ручки, сервера и подключений.
///
/// Остановка состоит из трёх половин, и каждой одной мало:
/// * флаг — его смотрят между кусками данных и после каждого `accept`;
/// * «будильник» — поток сервера спит в `accept`, и флаг он увидит только
///   со следующим подключением. Поэтому остановка сама подключается к порту
///   через петлю: запрос приходит, поток просыпается и видит флаг;
/// * список открытых подключений — `shutdown` на каждое. Телефону он
///   сразу говорит «конец», а поток передачи, уснувший в `read`, на Linux
///   и macOS будит немедленно. На Windows — нет, и это молчаливое отличие
///   (Правило 6; проверено вживую первой версией теста
///   `stopping_cuts_an_upload_in_flight`, которая ждала молчащий телефон):
///   там `shutdown` не прерывает уже идущий `recv`. Подключение всё равно
///   мертво — первый же пакет от телефона Windows встречает сбросом, и
///   `read` возвращает ошибку, — но если телефон замолчал, поток досыпает
///   до `IO_TIMEOUT`. Ничего записать он при этом уже не может, и
///   недопринятый файл удалит, проснувшись; не успеет до закрытия окна —
///   уберёт `sweep_temp_files` при следующей раздаче.
///
/// Порядок у сторон зеркальный, и это вся развязка гонок: остановка сначала
/// поднимает флаг, потом читает будильник и список; поток сначала записывает
/// себя в будильник или в список, потом смотрит на флаг. Третьего исхода нет:
/// либо остановка его нашла, либо он увидел флаг.
struct Shared {
    stopped: AtomicBool,
    key: String,
    /// Куда подключиться, чтобы разбудить `accept`.
    wake: Mutex<Option<SocketAddr>>,
    connections: Mutex<HashMap<u64, TcpStream>>,
    /// Сколько подключений обслуживается прямо сейчас.
    active: AtomicUsize,
    next_id: AtomicU64,
}

impl Shared {
    fn new(key: String) -> Self {
        Self {
            stopped: AtomicBool::new(false),
            key,
            wake: Mutex::new(None),
            connections: Mutex::new(HashMap::new()),
            active: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
        }
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);

        for stream in lock(&self.connections).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }

        if let Some(addr) = *lock(&self.wake) {
            // В своём потоке: зовут это из кадра отрисовки, а подключение —
            // это ввод-вывод, пусть и через петлю (Правило 1).
            std::thread::spawn(move || {
                let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(2));
            });
        }
    }
}

/// Замок, переживающий панику соседнего потока: список подключений после
/// неё всё так же годен, а оставить раздачу неостановимой нельзя.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Что общее у всех подключений одной раздачи.
struct Server {
    shared: Arc<Shared>,
    dir: PathBuf,
    tx: Sender<Event>,
    notify: Arc<dyn Fn() + Send + Sync>,
    /// На каком языке говорить и с окном, и с телефоном.
    ///
    /// Телефон отвечает тем же языком, что и окно, намеренно: это один и тот
    /// же человек, и разводить его устройства по языкам было бы странностью
    /// без причины. Поле, а не аргумент: язык у идущей раздачи не меняется —
    /// `SavioApp` перезапускает её при смене языка.
    lang: Lang,
}

impl Server {
    /// Короткий доступ к строке на языке раздачи.
    fn text(&self, key: Key) -> &'static str {
        i18n::t(self.lang, key)
    }

    fn send(&self, event: ShareEvent) {
        // Приёмник умер — экран закрыли. Работать больше не на кого, но
        // решает это не отправка, а флаг остановки: `on_exit` и уход с экрана
        // зовут `Handle::stop`.
        if self.tx.send(Event::Share(event)).is_ok() {
            (self.notify)();
        }
    }
}

/// Запускает раздачу папки `dir` в отдельном потоке.
///
/// Первым событием приходит `Ready` с адресами или `Stopped` с причиной.
pub fn start(
    dir: PathBuf,
    lang: Lang,
    tx: Sender<Event>,
    notify: impl Fn() + Send + Sync + 'static,
) -> Handle {
    start_with(
        Listen {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ports: &PORTS,
            find: find_addresses,
        },
        dir,
        lang,
        tx,
        notify,
    )
}

/// Где слушать и как назвать адреса. Отдельно ради тестов: они слушают
/// петлю, а не всю сеть, — иначе каждый прогон спрашивал бы брандмауэр.
struct Listen {
    bind: IpAddr,
    ports: &'static [u16],
    find: fn(u16, &str) -> Vec<ShareAddress>,
}

fn start_with(
    listen: Listen,
    dir: PathBuf,
    lang: Lang,
    tx: Sender<Event>,
    notify: impl Fn() + Send + Sync + 'static,
) -> Handle {
    let shared = Arc::new(Shared::new(generate_key()));
    let server = Arc::new(Server {
        shared: Arc::clone(&shared),
        dir,
        tx,
        notify: Arc::new(notify),
        lang,
    });

    std::thread::spawn(move || serve(&server, &listen));

    Handle { shared }
}

fn serve(server: &Arc<Server>, listen: &Listen) {
    let Some((listener, port)) = bind(listen.bind, listen.ports) else {
        server.send(ShareEvent::Stopped(
            server.text(Key::SharePortsBusy).to_owned(),
        ));
        return;
    };

    let wake_ip = if listen.bind.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        listen.bind
    };
    *lock(&server.shared.wake) = Some(SocketAddr::new(wake_ip, port));
    // После записи будильника, а не до — см. порядок у `Shared`.
    if server.shared.stopped() {
        return;
    }

    sweep_temp_files(&server.dir);

    let addresses = (listen.find)(port, &server.shared.key);
    if addresses.is_empty() {
        server.send(ShareEvent::Stopped(
            server.text(Key::ShareNoLocalAddress).to_owned(),
        ));
        return;
    }
    server.send(ShareEvent::Ready(addresses));

    for incoming in listener.incoming() {
        if server.shared.stopped() {
            return;
        }
        let stream = match incoming {
            Ok(stream) => stream,
            // Оборванное на полпути подключение — обычное дело. Но бывает и
            // стойкая беда (кончились дескрипторы): без паузы цикл крутился бы
            // вхолостую на полном ядре.
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        if server.shared.active.load(Ordering::SeqCst) >= CONNECTION_LIMIT {
            let mut stream = stream;
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = respond_text(
                &mut stream,
                503,
                i18n::t(server.lang, Key::ShareTooManyConnections),
                false,
            );
            continue;
        }
        server.shared.active.fetch_add(1, Ordering::SeqCst);

        let server = Arc::clone(server);
        std::thread::spawn(move || {
            connection(&server, stream);
            server.shared.active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Открывает первый свободный порт из списка.
fn bind(ip: IpAddr, ports: &[u16]) -> Option<(TcpListener, u16)> {
    ports.iter().find_map(|&port| {
        let listener = TcpListener::bind((ip, port)).ok()?;
        let port = listener.local_addr().ok()?.port();
        Some((listener, port))
    })
}

/// Обслуживает одно подключение: один запрос, один ответ.
fn connection(server: &Server, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let peer = stream.peer_addr().ok();

    let id = server.shared.next_id();
    let Ok(watch) = stream.try_clone() else {
        return;
    };
    lock(&server.shared.connections).insert(id, watch);
    // Вычёркиваемся при любом выходе, включая панику: иначе остановка через
    // час стала бы звать `shutdown` на давно закрытый сокет.
    let _registered = Registered { shared: &server.shared, id };
    if server.shared.stopped() {
        return;
    }

    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::with_capacity(CHUNK, read_half);
    let mut writer = stream;

    match read_head(&mut reader) {
        Ok(Some(text)) => match parse_head(&text) {
            Ok(head) => route(server, &head, &mut reader, &mut writer, peer),
            Err(status) => {
                let _ = respond_text(&mut writer, status, server.text(Key::ShareBadRequest), false);
            }
        },
        // Подключились и ушли, ничего не спросив. Так ведут себя браузеры,
        // заранее открывающие подключение «про запас», и наш же будильник.
        Ok(None) => return,
        Err(status) => {
            let _ = respond_text(&mut writer, status, server.text(Key::ShareBadRequest), false);
        }
    }

    if !server.shared.stopped() {
        close_gently(&mut reader, &writer);
    }
}

/// Сколько дочитываем за телефоном после ответа.
const LINGER: Duration = Duration::from_secs(2);

/// Закрывает подключение так, чтобы ответ не потерялся.
///
/// Просто бросить сокет нельзя, и это молчаливый отказ (Правило 6): если во
/// входящем буфере к этому мгновению что-то лежит, Windows закрывает
/// подключение не FIN, а RST, и клиент теряет **уже отправленный** ответ —
/// у него чтение кончается «connection reset», а не данными. Лежать там
/// бывает что угодно: хвост тела, от которого мы отказались (чужой ключ,
/// 411), или просто поздний пакет. Проверено вживую: под нагрузкой полного
/// прогона тестов живой тест раздачи терял ответ через раз, на разных
/// запросах.
///
/// Отсюда приём, которым закрываются HTTP-серверы: сперва закрыть запись
/// (клиент получает FIN и видит конец ответа), потом дочитать входящее до
/// конца — клиент, получив ответ, закрывает свою сторону сам, — и только
/// потом бросить сокет. Дочитываем с потолком по времени: иначе клиент,
/// который не закрывается, держал бы поток.
fn close_gently(reader: &mut impl Read, writer: &TcpStream) {
    // Таймаут до `shutdown`, а не после: macOS на `setsockopt` у подключения,
    // которое уже закрыто, отвечает «Invalid argument» (так упал тест в CI
    // 0.27.0), и дочитывание осталось бы без потолка.
    let _ = writer.set_read_timeout(Some(LINGER));
    let _ = writer.shutdown(Shutdown::Write);
    let started = Instant::now();
    let mut sink = [0u8; 16 * 1024];
    while started.elapsed() < LINGER {
        match reader.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

struct Registered<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(&self.shared.connections).remove(&self.id);
    }
}

fn route(
    server: &Server,
    head: &Head,
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    peer: Option<SocketAddr>,
) {
    let (raw_path, query) = split_target(&head.target);
    let Some(path) = percent_decode(raw_path) else {
        let _ = respond_text(writer, 400, server.text(Key::ShareBadPath), false);
        return;
    };
    let head_only = head.method == "HEAD";
    let reading = head.method == "GET" || head_only;

    // Значок вкладки браузер просит сам и без ключа. Отвечать на это отказом
    // «ссылка устарела» было бы неправдой.
    if path == "/favicon.ico" {
        let _ = respond_text(writer, 404, server.text(Key::ShareNothingHere), head_only);
        return;
    }

    let given = query_param(query, "k").unwrap_or_default();
    if !same_key(&given, &server.shared.key) {
        let _ = respond(
            writer,
            Reply::new(403, "text/html; charset=utf-8", head_only),
            stale_page(server.lang).as_bytes(),
        );
        return;
    }

    if path == "/" {
        if !reading {
            let _ = respond_text(writer, 405, server.text(Key::ShareMethodNotAllowed), head_only);
            return;
        }
        if let Some(peer) = peer {
            let device = device_name(head.header("user-agent").unwrap_or_default(), server.lang);
            server.send(ShareEvent::Visitor(format!("{device} · {}", peer.ip())));
        }
        let reply = Reply::new(200, "text/html; charset=utf-8", head_only).header(
            "Content-Security-Policy",
            "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
             img-src 'self' blob:; media-src 'self'",
        );
        let body = page(server.lang, &folder_name(&server.dir));
        let _ = respond(writer, reply, body.as_bytes());
    } else if path == "/api/files" {
        if !reading {
            let _ = respond_text(writer, 405, server.text(Key::ShareMethodNotAllowed), head_only);
            return;
        }
        let body = list_files(&server.dir).to_string();
        let _ = respond(
            writer,
            Reply::new(200, "application/json; charset=utf-8", head_only),
            body.as_bytes(),
        );
    } else if let Some(name) = path.strip_prefix("/files/") {
        if !reading {
            let _ = respond_text(writer, 405, server.text(Key::ShareMethodNotAllowed), head_only);
            return;
        }
        let download = query_param(query, "dl").is_some();
        send_file(server, head, name, download, head_only, writer);
    } else if let Some(name) = path.strip_prefix("/upload/") {
        if head.method != "PUT" {
            let _ = respond_text(writer, 405, server.text(Key::ShareMethodNotAllowed), head_only);
            return;
        }
        receive_file(server, head, name, reader, writer);
    } else {
        let _ = respond_text(writer, 404, server.text(Key::ShareNoSuchPage), head_only);
    }
}

/// Что показать тому, кто пришёл без ключа или со старым.
///
/// Своя крошечная разметка, а не `PAGE`: на той странице живёт скрипт, а
/// объяснять человеку тут нечего, кроме двух строк.
fn stale_page(lang: Lang) -> String {
    format!(
        "<!doctype html><html lang=\"{}\"><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>Savio</title>\
         <body style=\"font:17px system-ui,sans-serif;background:#100e0c;\
         color:#f9f4ed;padding:24px\">\
         <h1 style=\"font-size:22px\">{}</h1><p>{}</p>",
        i18n::t(lang, Key::PageLangTag),
        i18n::t(lang, Key::ShareStaleTitle),
        i18n::t(lang, Key::ShareStaleNote),
    )
}

/// Места подстановки в [`PAGE`] и что в них кладётся.
///
/// Подстановка, а не три файла разметки: страница одна, а разъехаться трём
/// её копиям — вопрос первой же правки вёрстки. Токены вида `{{Имя}}`
/// в самой разметке больше нигде не встречаются, и это проверяет тест:
/// забытый токен виден на телефоне как `{{Имя}}` — молча и только у того,
/// кто открыл страницу.
const PAGE_SLOTS: [(&str, Key); 33] = [
    ("{{LangTag}}", Key::PageLangTag),
    ("{{LocaleTag}}", Key::PageLocaleTag),
    ("{{Title}}", Key::PageTitle),
    ("{{Subtitle}}", Key::PageSubtitle),
    ("{{StaleTitle}}", Key::ShareStaleTitle),
    ("{{StaleNote}}", Key::ShareStaleNote),
    ("{{ToComputer}}", Key::TransferToComputer),
    ("{{ToComputerNote}}", Key::PageToComputerNote),
    ("{{UploadNote}}", Key::PageUploadNote),
    ("{{PickFiles}}", Key::PagePickFiles),
    ("{{IosNote}}", Key::PageIosNote),
    ("{{AwakeNote}}", Key::PageAwakeNote),
    ("{{FromComputer}}", Key::PageFromComputer),
    ("{{FromComputerNote}}", Key::PageFromComputerNote),
    ("{{Refresh}}", Key::PageRefresh),
    ("{{LoadingList}}", Key::PageLoadingList),
    ("{{Offline}}", Key::PageOffline),
    ("{{ByteUnits}}", Key::PageByteUnits),
    ("{{PerSecond}}", Key::UnitPerSecond),
    ("{{AmountOfTotal}}", Key::AmountOfTotal),
    ("{{Queued}}", Key::PageQueued),
    ("{{Sending}}", Key::PageSending),
    ("{{DoneWithSize}}", Key::PageDoneWithSize),
    ("{{SavedAs}}", Key::PageSavedAs),
    ("{{NotAccepted}}", Key::PageNotAccepted),
    ("{{TransferBroke}}", Key::PageTransferBroke),
    ("{{SendAgain}}", Key::PageSendAgain),
    ("{{ListFailedRetry}}", Key::PageListFailedRetry),
    ("{{ListFailed}}", Key::PageListFailed),
    ("{{FolderContents}}", Key::PageFolderContents),
    ("{{FolderEmpty}}", Key::PageFolderEmpty),
    ("{{Open}}", Key::PageOpen),
    ("{{Download}}", Key::PageDownload),
];

/// Страница для телефона на выбранном языке.
///
/// `folder` — имя раздаваемой папки, **только имя**, без пути: страницу
/// видит любой, у кого есть адрес, и полный путь рассказал бы ему имя
/// пользователя и устройство диска. Имя нужно затем, чтобы человек с
/// телефона видел, куда именно уедут его файлы, — «в папку компьютера»
/// без названия ничего не объясняет.
fn page(lang: Lang, folder: &str) -> String {
    let mut out = PAGE.to_owned();
    for (slot, key) in PAGE_SLOTS {
        out = out.replace(slot, i18n::t(lang, key));
    }
    // Имя папки — не строка из таблицы, а данные пользователя, и в разметку
    // его можно класть только экранированным. Папка по имени `<b>` или
    // `Tom & Jerry` иначе ломала бы страницу на телефоне, а проверка
    // `the_phone_page_strings_are_safe_to_paste` про неё не знает: она
    // смотрит только таблицу строк.
    let line = i18n::fill(i18n::t(lang, Key::PageFolderLine), &[&escape_html(folder)]);
    out.replace("{{FolderLine}}", &line)
}

/// Экранирует текст для вставки в HTML.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Имя папки для страницы: последний сегмент пути, без самого пути.
fn folder_name(dir: &std::path::Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        // У корня диска имени нет — тогда показываем путь целиком: это
        // и есть имя, другого у него не бывает.
        .unwrap_or_else(|| dir.display().to_string())
}

// ---------------------------------------------------------------------------
// Список, отдача и приём файлов
// ---------------------------------------------------------------------------

/// Файлы папки раздачи, свежие сверху.
///
/// Только сама папка, без вложенных: раздают её, а не всё, что под ней.
/// Имена, которые не пережили бы `clean_name`, не показываются — забрать
/// такой файл по адресу всё равно нельзя.
fn list_files(dir: &Path) -> serde_json::Value {
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if clean_name(&name).as_deref() != Some(name.as_str()) || is_system_file(&name) {
                continue;
            }
            // `metadata` у записи идёт по ссылке, а не за неё: ссылку наружу
            // папки покажем, но `resolve` её потом всё равно не отдаст.
            let Ok(meta) = fs::metadata(entry.path()) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_secs());
            files.push((name, meta.len(), modified));
        }
    }

    files.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    files.truncate(LIST_LIMIT);

    serde_json::Value::Array(
        files
            .into_iter()
            .map(|(name, size, modified)| {
                serde_json::json!({ "name": name, "size": size, "modified": modified })
            })
            .collect(),
    )
}

/// Служебные файлы оболочки Windows, которые человек в своей папке не видит.
///
/// Скрыты они атрибутом, а не точкой в имени, поэтому фильтр по точке их
/// пропускал: на рабочем столе первым делом в списке на телефоне стоял
/// `desktop.ini` (проверено глазами). По именам, а не по атрибуту: атрибут
/// читается только на Windows, а эти файлы приезжают в папку и с флешки
/// на любой системе.
fn is_system_file(name: &str) -> bool {
    name.eq_ignore_ascii_case("desktop.ini") || name.eq_ignore_ascii_case("thumbs.db")
}

/// Находит файл папки раздачи по имени из адреса.
///
/// Два заслона, и оба нужны. Имя обязано быть чистым (`clean_name` его не
/// меняет) — значит, в нём нет ни разделителей, ни `..`. Но чистое имя
/// бывает ссылкой, ведущей наружу папки, поэтому настоящий путь сверяется
/// ещё и после разворота ссылок.
fn resolve(dir: &Path, name: &str) -> Option<PathBuf> {
    if clean_name(name).as_deref() != Some(name) {
        return None;
    }
    let real = dir.join(name).canonicalize().ok()?;
    let root = dir.canonicalize().ok()?;
    (real.parent() == Some(root.as_path()) && real.is_file()).then_some(real)
}

fn send_file(
    server: &Server,
    head: &Head,
    name: &str,
    download: bool,
    head_only: bool,
    writer: &mut TcpStream,
) {
    let Some(path) = resolve(&server.dir, name) else {
        let _ = respond_text(writer, 404, server.text(Key::ShareNoSuchFile), head_only);
        return;
    };
    let Ok(mut file) = File::open(&path) else {
        let _ = respond_text(writer, 404, server.text(Key::ShareFileNotOpened), head_only);
        return;
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        let _ = respond_text(writer, 500, server.text(Key::ShareSizeUnknown), head_only);
        return;
    };

    let range = head.header("range").map_or(Range::Full, |value| parse_range(value, len));
    let (status, start, count) = match range {
        Range::Full => (200, 0, len),
        Range::Part { start, end } => (206, start, end - start + 1),
        Range::Unsatisfiable => {
            let reply = Reply::new(416, "text/plain; charset=utf-8", head_only)
                .header("Content-Range", format!("bytes */{len}"));
            let _ = respond(writer, reply, b"");
            return;
        }
    };

    let mut reply = Reply::new(status, content_type(name), head_only)
        .length(count)
        .header("Accept-Ranges", "bytes")
        .header("Content-Disposition", disposition(name, download || forced_download(name)))
        // Файл из папки — чужие данные. HTML или SVG, открытые прямо на нашем
        // адресе, видели бы ключ в адресной строке; песочница лишает их этого.
        .header("Content-Security-Policy", "sandbox");
    if status == 206 {
        reply = reply.header("Content-Range", format!("bytes {start}-{}/{len}", start + count - 1));
    }

    if respond_head(writer, &reply).is_err() || head_only {
        return;
    }
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return;
    }

    // Про кусочные запросы не рассказываем: видео, которое смотрят прямо
    // в браузере, спрашивает их десятками при каждой перемотке, и список
    // передач утонул бы в обрывках одного и того же ролика. Кнопка
    // «Скачать» на странице просит файл целиком (`dl=1`) — это и есть
    // передача, о которой человек хочет знать.
    let reported = download || status == 200;
    let id = server.shared.next_id();
    if reported {
        server.send(ShareEvent::Started {
            id,
            direction: TransferDirection::ToPhone,
            name: name.to_owned(),
            total: Some(count),
        });
    }

    match pump(server, &mut file, writer, count, reported.then_some(id)) {
        Ok(()) => {
            if reported {
                server.send(ShareEvent::Finished { id, name: name.to_owned() });
            }
        }
        Err(broken) => {
            if reported {
                let key = if server.shared.stopped() {
                    Key::ShareStoppedSending
                } else {
                    match broken {
                        Broken::Read | Broken::Short => Key::ShareReadFailed,
                        Broken::Write(_) | Broken::Stopped => Key::SharePhoneStoppedReceiving,
                    }
                };
                let message = i18n::fill(server.text(key), &[name]);
                server.send(ShareEvent::Failed { id, message });
            }
        }
    }
}

fn receive_file(
    server: &Server,
    head: &Head,
    raw_name: &str,
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
) {
    let Some(name) = clean_name(raw_name) else {
        let _ = respond_text(writer, 400, server.text(Key::ShareNoFileName), false);
        return;
    };
    // Кусочную передачу браузер для файла с известным размером не выбирает,
    // а принимать тело неизвестной длины — значит не знать, дошло ли оно.
    if head.header("transfer-encoding").is_some() {
        let _ = respond_text(writer, 411, server.text(Key::ShareNeedLength), false);
        return;
    }
    let len = match head.content_length {
        Some(len) => len,
        None => {
            let _ = respond_text(writer, 411, server.text(Key::ShareNeedLength), false);
            return;
        }
    };

    let id = server.shared.next_id();
    let temp = server.dir.join(format!("{TEMP_PREFIX}{id}.part"));
    let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
        Ok(file) => file,
        Err(error) => {
            let message = i18n::fill(
                server.text(Key::ShareCannotAcceptDir),
                &[&name, &error.to_string()],
            );
            server.send(ShareEvent::Failed { id, message });
            let _ = respond_text(writer, 500, server.text(Key::ShareCannotWriteDir), false);
            return;
        }
    };

    server.send(ShareEvent::Started {
        id,
        direction: TransferDirection::ToComputer,
        name: name.clone(),
        total: Some(len),
    });

    let pumped = pump(server, reader, &mut file, len, Some(id)).and_then(|()| {
        file.flush().map_err(Broken::Write)
    });
    drop(file);

    if let Err(broken) = pumped {
        let _ = fs::remove_file(&temp);
        let message = if server.shared.stopped() {
            i18n::fill(server.text(Key::ShareStoppedReceiving), &[&name])
        } else {
            match broken {
                Broken::Write(error) if error.kind() == io::ErrorKind::StorageFull => {
                    i18n::fill(server.text(Key::ShareDiskFull), &[&name])
                }
                Broken::Write(error) => i18n::fill(
                    server.text(Key::ShareWriteFailed),
                    &[&name, &error.to_string()],
                ),
                Broken::Read | Broken::Short | Broken::Stopped => {
                    i18n::fill(server.text(Key::SharePhoneStoppedSending), &[&name])
                }
            }
        };
        server.send(ShareEvent::Failed { id, message });
        let _ = respond_text(writer, 500, server.text(Key::ShareFileRejected), false);
        return;
    }

    match place(&server.dir, &name, &temp, server.lang) {
        Ok(saved) => {
            server.send(ShareEvent::Finished { id, name: saved.clone() });
            let body = serde_json::json!({ "name": saved }).to_string();
            let _ = respond(
                writer,
                Reply::new(200, "application/json; charset=utf-8", false),
                body.as_bytes(),
            );
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            server.send(ShareEvent::Failed {
                id,
                message: i18n::fill(
                    server.text(Key::ShareNotMoved),
                    &[&name, &error.to_string()],
                ),
            });
            let _ = respond_text(writer, 500, server.text(Key::ShareFileNotSaved), false);
        }
    }
}

/// Кладёт принятый файл под своим именем, не затирая чужой.
///
/// Имя сначала занимается пустым файлом через `create_new` — это атомарно:
/// два телефона, одновременно отправившие «IMG_0001.jpg», получат «(2)»
/// и «(3)», а не затрут друг друга. Потом временный файл переезжает поверх
/// занятого места: `rename` заменяет файл и на Windows, и на Unix.
fn place(dir: &Path, name: &str, temp: &Path, lang: Lang) -> io::Result<String> {
    for n in 1..=999 {
        let candidate = numbered(name, n);
        let target = dir.join(&candidate);
        match OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(_) => {
                return match fs::rename(temp, &target) {
                    Ok(()) => Ok(candidate),
                    Err(error) => {
                        let _ = fs::remove_file(&target);
                        Err(error)
                    }
                };
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(i18n::t(lang, Key::ShareNamesExhausted)))
}

/// Удаляет недопринятые файлы, оставшиеся от прошлой раздачи.
///
/// Свои временные файлы поток приёма удаляет сам, но только если успевает:
/// выключенный компьютер или убитый процесс успеть не дают. Трогаем только
/// своё — имя с `TEMP_PREFIX` и `.part` на конце.
fn sweep_temp_files(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(TEMP_PREFIX) && name.ends_with(".part") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Чем кончилась перекачка.
enum Broken {
    /// Источник отказал.
    Read,
    /// Источник кончился раньше обещанного.
    Short,
    /// Приёмник отказал.
    Write(io::Error),
    /// Раздачу остановили.
    Stopped,
}

/// Перекачивает ровно `len` байт кусками, глядя на остановку между ними.
///
/// Ничего не копит в памяти: ролик с телефона — это гигабайты (Правило 1).
/// `id` — о какой передаче рассказывать; `None` — молча.
fn pump(
    server: &Server,
    from: &mut impl Read,
    to: &mut impl Write,
    len: u64,
    id: Option<u64>,
) -> Result<(), Broken> {
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut told = Instant::now();

    while done < len {
        if server.shared.stopped() {
            return Err(Broken::Stopped);
        }
        let want = usize::try_from(len - done).map_or(CHUNK, |left| left.min(CHUNK));
        let read = match from.read(&mut buf[..want]) {
            Ok(0) => return Err(Broken::Short),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Broken::Read),
        };
        to.write_all(&buf[..read]).map_err(Broken::Write)?;
        done += read as u64;

        if let Some(id) = id
            && told.elapsed() >= PROGRESS_EVERY
        {
            server.send(ShareEvent::Progress { id, done });
            told = Instant::now();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Разобранный заголовок запроса.
#[derive(Debug)]
struct Head {
    method: String,
    target: String,
    /// Имена в нижнем регистре: в HTTP они регистра не различают.
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Дочитывает заголовок до пустой строки. `Ok(None)` — подключение закрыли,
/// ничего не спросив; `Err` — код ответа.
///
/// Байты тела, приехавшие в том же пакете, остаются в буфере `reader` — тело
/// потом читается из него же.
fn read_head(reader: &mut impl BufRead) -> Result<Option<String>, u16> {
    let mut head = Vec::new();
    let mut line = Vec::new();

    loop {
        line.clear();
        let room = (HEAD_LIMIT - head.len()) as u64;
        let read = reader
            .by_ref()
            .take(room)
            .read_until(b'\n', &mut line)
            .map_err(|_| 400u16)?;
        if read == 0 {
            return if head.is_empty() { Ok(None) } else { Err(400) };
        }
        if !line.ends_with(b"\n") {
            return Err(431);
        }

        let blank = line == b"\r\n" || line == b"\n";
        if blank {
            // Пустые строки перед самим запросом RFC 9112 велит пропускать.
            if head.is_empty() {
                continue;
            }
            break;
        }
        head.extend_from_slice(&line);
    }

    String::from_utf8(head).map(Some).map_err(|_| 400)
}

/// Разбирает заголовок: строку запроса и поля.
fn parse_head(text: &str) -> Result<Head, u16> {
    let mut lines = text.lines();
    let request = lines.next().ok_or(400u16)?;
    let mut parts = request.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(400);
    };
    if method.is_empty() || !target.starts_with('/') || !version.starts_with("HTTP/1.") {
        return Err(400);
    }

    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(400);
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim().to_owned();

        if name == "content-length" {
            let parsed: u64 = value.parse().map_err(|_| 400u16)?;
            // Два разных `Content-Length` — классика контрабанды запросов:
            // тот, кто стоит между нами, и мы сами поняли бы тело по-разному.
            if content_length.is_some_and(|known| known != parsed) {
                return Err(400);
            }
            content_length = Some(parsed);
        }
        headers.push((name, value));
    }

    Ok(Head {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
        content_length,
    })
}

/// Путь и запрос из цели: `/files/a.mp4?k=x` — `("/files/a.mp4", "k=x")`.
fn split_target(target: &str) -> (&str, &str) {
    target.split_once('?').unwrap_or((target, ""))
}

/// Раскрывает `%XX`. `None` — битая запись или не UTF-8.
///
/// `+` не трогаем: пробелом он бывает только в формах, а страница кодирует
/// имена через `encodeURIComponent`, который пишет пробел как `%20`.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Значение параметра запроса. `None` — такого нет или оно битое.
fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| percent_decode(value))
}

/// Сравнивает ключ за время, не зависящее от того, где первое расхождение.
fn same_key(given: &str, key: &str) -> bool {
    given.len() == key.len()
        && given
            .bytes()
            .zip(key.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Какой кусок файла просят.
#[derive(Debug, PartialEq, Eq)]
enum Range {
    Full,
    /// Границы включительно.
    Part { start: u64, end: u64 },
    Unsatisfiable,
}

/// Разбирает `Range`.
///
/// Без него телефон не докачает оборвавшийся файл и не станет проматывать
/// видео, пока не скачает его целиком. Всё, чего не понимаем (несколько
/// кусков, чужие единицы), отдаём целым файлом — стандарт это разрешает,
/// и лучше отдать лишнее, чем отказать.
fn parse_range(value: &str, len: u64) -> Range {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Range::Full;
    };
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((from, to)) = spec.trim().split_once('-') else {
        return Range::Full;
    };
    let (from, to) = (from.trim(), to.trim());

    if from.is_empty() {
        // `bytes=-500` — последние 500 байт.
        let Ok(suffix) = to.parse::<u64>() else {
            return Range::Full;
        };
        if suffix == 0 || len == 0 {
            return Range::Unsatisfiable;
        }
        return Range::Part { start: len.saturating_sub(suffix), end: len - 1 };
    }

    let Ok(start) = from.parse::<u64>() else {
        return Range::Full;
    };
    let end = if to.is_empty() {
        len.saturating_sub(1)
    } else {
        match to.parse::<u64>() {
            Ok(end) if end >= start => end.min(len.saturating_sub(1)),
            _ => return Range::Full,
        }
    };
    if start >= len {
        return Range::Unsatisfiable;
    }
    Range::Part { start, end }
}

/// Ответ без тела: статус и поля.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    length: Option<u64>,
    head_only: bool,
}

impl Reply {
    fn new(status: u16, content_type: &str, head_only: bool) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", content_type.to_owned())],
            length: None,
            head_only,
        }
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    fn length(mut self, length: u64) -> Self {
        self.length = Some(length);
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// Пишет статус и поля. Длина — из `reply.length`.
fn respond_head(writer: &mut impl Write, reply: &Reply) -> io::Result<()> {
    let mut text = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (name, value) in &reply.headers {
        text.push_str(name);
        text.push_str(": ");
        text.push_str(value);
        text.push_str("\r\n");
    }
    text.push_str(&format!(
        "Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\r\n",
        reply.length.unwrap_or(0)
    ));
    writer.write_all(text.as_bytes())
}

/// Пишет ответ с телом из памяти.
fn respond(writer: &mut impl Write, reply: Reply, body: &[u8]) -> io::Result<()> {
    let reply = reply.length(body.len() as u64);
    respond_head(writer, &reply)?;
    if !reply.head_only {
        writer.write_all(body)?;
    }
    writer.flush()
}

fn respond_text(writer: &mut impl Write, status: u16, text: &str, head_only: bool) -> io::Result<()> {
    respond(writer, Reply::new(status, "text/plain; charset=utf-8", head_only), text.as_bytes())
}

/// Тип содержимого по расширению.
///
/// HTML, SVG и прочее, что браузер исполнил бы, сюда не входит намеренно:
/// такие файлы уходят как «просто байты» и скачиванием (`forced_download`).
fn content_type(name: &str) -> &'static str {
    let ext = extension(name);
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Файлы, которые браузер открыл бы как страницу, отдаются только скачиванием.
fn forced_download(name: &str) -> bool {
    content_type(name) == "application/octet-stream"
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// `Content-Disposition` с именем в UTF-8.
///
/// Два имени, а не одно: `filename*` понимают все живые браузеры, но старые
/// берут `filename`, а в кавычки кириллицу и кавычки класть нельзя.
fn disposition(name: &str, download: bool) -> String {
    let kind = if download { "attachment" } else { "inline" };
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

// ---------------------------------------------------------------------------
// Имена, ключ, адреса
// ---------------------------------------------------------------------------

/// Имя файла с телефона — чужие данные, а не имя. Приводит его к безопасному.
///
/// Что убирается и почему:
/// * всё до последнего `/` или `\` — иначе имя стало бы путём;
/// * управляющие знаки и запретные для Windows `<>:"|?*`;
/// * знаки смены направления письма (U+202E и соседи): поставленный перед
///   `gpj.exe`, такой знак показывает `.exe` как `.jpg`;
/// * точки и пробелы в начале (`..`, скрытые файлы) и в конце (Windows их
///   молча отрезает, и имя на диске разошлось бы с показанным);
/// * имена устройств Windows (`CON`, `NUL`, `COM1`…) — и с расширением тоже:
///   `nul.txt` на Windows — это всё то же устройство.
///
/// Правила Windows действуют на всех системах: папку раздачи потом копируют
/// куда угодно, и имя, законное только на Linux, сломалось бы там.
///
/// `None` — от имени ничего не осталось.
pub fn clean_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let replaced: String = last
        .chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();

    let trimmed = replaced
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return None;
    }

    let mut name = shorten(trimmed, NAME_LIMIT);
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    if is_reserved(stem) {
        name.insert(0, '_');
    }
    Some(name)
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn is_reserved(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let bytes = upper.as_bytes();
    bytes.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && (b'1'..=b'9').contains(&bytes[3])
}

/// Укорачивает имя до `limit` байт, сохраняя расширение.
fn shorten(name: &str, limit: usize) -> String {
    if name.len() <= limit {
        return name.to_owned();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, Some(ext)),
        _ => (name, None),
    };
    let room = limit - ext.map_or(0, |ext| ext.len() + 1);
    let mut cut = room.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = stem[..cut].trim_end_matches(['.', ' ']);
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    }
}

/// Имя с номером: `IMG.jpg`, `IMG (2).jpg`, `IMG (3).jpg`…
fn numbered(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

/// Ключ раздачи.
///
/// Генератора случайных чисел в стандартной библиотеке нет, а ключ хеширования
/// у `RandomState` — есть: он берётся из случайности системы, и без знания
/// ключа выход SipHash не угадать. Время и номер процесса подмешаны, чтобы два
/// ключа одного потока не опирались на соседние значения одного счётчика.
fn generate_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut bits = 0u128;
    for round in 0..2u8 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_u8(round);
        bits = (bits << 64) | u128::from(hasher.finish());
    }

    let mut key = String::with_capacity(KEY_LEN);
    for _ in 0..KEY_LEN {
        key.push(KEY_ALPHABET[(bits & 31) as usize] as char);
        bits >>= 5;
    }
    key
}

/// Что за устройство, по `User-Agent`. Порядок важен: у iPad в строке есть
/// «Mac», у Android — «Linux».
/// Имена систем — названия, а не слова: «Android» и «Windows» одинаковы на
/// любом языке. Перевода просит только подпись «неизвестно какое».
fn device_name(agent: &str, lang: Lang) -> &'static str {
    const KNOWN: [(&str, &str); 7] = [
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("Windows", "Windows"),
        ("Macintosh", "Mac"),
        ("CrOS", "Chromebook"),
        ("Linux", "Linux"),
    ];
    KNOWN
        .iter()
        .find(|(mark, _)| agent.contains(mark))
        .map_or_else(
            || i18n::t(lang, Key::ShareUnknownDevice),
            |(_, name)| *name,
        )
}

/// Адреса компьютера в своей сети, самый вероятный первым.
fn find_addresses(port: u16, key: &str) -> Vec<ShareAddress> {
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let mut found = Vec::new();
    for (name, data) in &networks {
        for network in data.ip_networks() {
            if let IpAddr::V4(ip) = network.addr {
                found.push((name.clone(), ip));
            }
        }
    }

    rank_addresses(primary_ipv4(), found)
        .into_iter()
        .map(|(name, ip)| ShareAddress::new(ip, port, key, &name))
        .collect()
}

/// Через какой адрес ушёл бы пакет в интернет.
///
/// `connect` у UDP ничего не отправляет: система только выбирает маршрут и
/// называет свой адрес на нём. Это почти всегда Wi-Fi или кабель — но не
/// всегда: с включённым VPN маршрут идёт через него. Поэтому это подсказка
/// для порядка, а не единственный ответ.
fn primary_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}

/// Сколько адресов показываем на выбор.
const ADDRESS_LIMIT: usize = 6;

/// Отбирает и упорядочивает адреса для показа.
///
/// Только частные (`10.*`, `172.16–31.*`, `192.168.*`): на них телефон в той
/// же сети достучится. Первым — тот, через который идёт маршрут, дальше —
/// обычные интерфейсы, в хвосте — виртуальные: VPN, WSL, Hyper-V, Docker,
/// VirtualBox заводят свои адреса, и телефону до них не добраться.
fn rank_addresses(primary: Option<Ipv4Addr>, found: Vec<(String, Ipv4Addr)>) -> Vec<(String, Ipv4Addr)> {
    let mut list: Vec<(String, Ipv4Addr)> = Vec::new();
    for (name, ip) in found {
        if ip.is_private() && !list.iter().any(|(_, known)| *known == ip) {
            list.push((name, ip));
        }
    }
    // Маршрут назвал частный адрес, которого нет в списке интерфейсов, —
    // список неполон, и адрес маршрута всё равно стоит показать.
    if let Some(ip) = primary
        && ip.is_private()
        && !list.iter().any(|(_, known)| *known == ip)
    {
        list.push((String::new(), ip));
    }

    // Сортировка устойчивая: при равенстве остаётся порядок системы.
    list.sort_by_key(|(name, ip)| (Some(*ip) != primary, is_virtual(name)));
    list.truncate(ADDRESS_LIMIT);
    list
}

/// Похоже ли имя интерфейса на виртуальный.
fn is_virtual(name: &str) -> bool {
    const MARKS: [&str; 14] = [
        "vethernet", "virtualbox", "vboxnet", "vmware", "vmnet", "docker", "br-", "veth", "wsl",
        "hyper-v", "tailscale", "zerotier", "vpn", "utun",
    ];
    let lower = name.to_lowercase();
    MARKS.iter().any(|mark| lower.contains(mark))
}

#[cfg(test)]
mod tests;
