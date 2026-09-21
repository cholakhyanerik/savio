use std::io::Cursor;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};

use super::*;

/// Свой каталог на диске под каждый тест.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("savio-share-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("создать временный каталог");
    dir
}

// ---------------------------------------------------------------------------
// Страница для телефона
// ---------------------------------------------------------------------------

/// После подстановки в разметке не должно остаться ни одного места.
///
/// Забытое место видно только на экране телефона — буквальным `{{Имя}}`
/// посреди текста, — и только тому, кто открыл страницу. Ни сборка, ни
/// `clippy` такого не ловят: для них это обычная строка.
#[test]
fn the_phone_page_has_no_slots_left() {
    for lang in Lang::ALL {
        let html = page(lang, "Загрузки");
        assert!(
            !html.contains("{{"),
            "{lang:?}: в странице осталось место подстановки"
        );
        // Заодно проверка, что подставили не пустоту: заголовок обязан быть.
        assert!(
            html.contains(i18n::t(lang, Key::PageTitle)),
            "{lang:?}: заголовок не подставился"
        );
    }
}

/// Строки страницы попадают и в разметку, и внутрь строковых литералов
/// JavaScript. Кавычка-лапка закрыла бы литерал раньше времени, `<` —
/// открыл бы тег, обратная косая съела бы следующий знак. Скрипт при этом
/// не «испортился бы немного»: он перестал бы выполняться целиком, и страница
/// на телефоне осталась бы без отправки и без списка. Ни сборка, ни `clippy`,
/// ни глаза на русском такого не увидят — промах приезжает вместе с чужим
/// переводом.
#[test]
fn the_phone_page_strings_are_safe_to_paste() {
    for (_, key) in PAGE_SLOTS {
        for lang in Lang::ALL {
            let text = i18n::t(lang, key);
            for bad in ['"', '\\', '<', '>', '&'] {
                assert!(
                    !text.contains(bad),
                    "{lang:?}/{key:?}: знак «{bad}» сломает разметку или скрипт: {text}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Разбор запроса
// ---------------------------------------------------------------------------

#[test]
fn a_browser_request_head_is_parsed() {
    let head = parse_head(
        "PUT /upload/a.jpg?k=abc HTTP/1.1\r\nHost: 192.168.1.42:8080\r\nContent-Length: 5\r\nUser-Agent: Mozilla/5.0 (Linux; Android 14)\r\n",
    )
    .expect("обычный запрос");
    assert_eq!(head.method, "PUT");
    assert_eq!(head.target, "/upload/a.jpg?k=abc");
    assert_eq!(head.content_length, Some(5));
    // Имена полей регистра не различают — браузер вправе прислать любой.
    assert_eq!(head.header("user-agent"), Some("Mozilla/5.0 (Linux; Android 14)"));
    assert_eq!(head.header("host"), Some("192.168.1.42:8080"));
}

#[test]
fn broken_request_heads_are_refused() {
    for text in [
        "",
        "GET /\r\n",
        "GET / HTTP/1.1 extra\r\n",
        "GET http://evil/ HTTP/1.1\r\n",
        "GET / SPDY/3\r\n",
        "GET / HTTP/1.1\r\nno colon here\r\n",
        "GET / HTTP/1.1\r\nBad Name: x\r\n",
        "PUT /upload/a HTTP/1.1\r\nContent-Length: -1\r\n",
    ] {
        assert!(parse_head(text).is_err(), "принят {text:?}");
    }
}

/// Два разных `Content-Length` — отказ, а не выбор одного из них.
#[test]
fn conflicting_lengths_are_refused() {
    assert_eq!(
        parse_head("PUT /upload/a HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n").err(),
        Some(400)
    );
    // Повтор того же числа законен.
    assert!(parse_head("PUT /upload/a HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 5\r\n").is_ok());
}

/// Тело, приехавшее в одном пакете с заголовком, не теряется: его читают
/// из того же буфера.
#[test]
fn the_body_stays_in_the_reader_after_the_head() {
    let mut reader = BufReader::new(Cursor::new(
        b"\r\nPUT /upload/a HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
    ));
    let head = read_head(&mut reader).expect("заголовок").expect("не пустой");
    assert!(head.starts_with("PUT /upload/a"), "{head:?}");

    let mut body = String::new();
    reader.read_to_string(&mut body).expect("тело");
    assert_eq!(body, "hello");
}

#[test]
fn an_empty_connection_is_not_an_error() {
    let mut reader = BufReader::new(Cursor::new(Vec::new()));
    assert_eq!(read_head(&mut reader), Ok(None));
}

/// Бесконечный заголовок не копится в памяти.
#[test]
fn a_huge_head_is_cut_off() {
    let mut text = b"GET / HTTP/1.1\r\nX: ".to_vec();
    text.extend(std::iter::repeat_n(b'a', HEAD_LIMIT * 2));
    let mut reader = BufReader::new(Cursor::new(text));
    assert_eq!(read_head(&mut reader), Err(431));
}

#[test]
fn percent_escapes_are_decoded() {
    assert_eq!(percent_decode("/files/%D0%A4%D0%BE%D1%82%D0%BE%20(2).jpg").as_deref(), Some("/files/Фото (2).jpg"));
    assert_eq!(percent_decode("plain").as_deref(), Some("plain"));
    // Плюс в пути — это плюс.
    assert_eq!(percent_decode("a+b").as_deref(), Some("a+b"));
    assert_eq!(percent_decode("%zz"), None);
    assert_eq!(percent_decode("%4"), None);
    // Не UTF-8 — не имя.
    assert_eq!(percent_decode("%FF"), None);
}

#[test]
fn query_parameters_are_found_by_name() {
    assert_eq!(query_param("k=abc&dl=1", "k").as_deref(), Some("abc"));
    assert_eq!(query_param("k=abc&dl=1", "dl").as_deref(), Some("1"));
    assert_eq!(query_param("k=abc&dl", "dl").as_deref(), Some(""));
    assert_eq!(query_param("kk=abc", "k"), None);
    assert_eq!(query_param("", "k"), None);
    assert_eq!(split_target("/files/a?k=1"), ("/files/a", "k=1"));
    assert_eq!(split_target("/"), ("/", ""));
}

#[test]
fn keys_are_compared_whole() {
    assert!(same_key("abc", "abc"));
    assert!(!same_key("abd", "abc"));
    assert!(!same_key("ab", "abc"));
    assert!(!same_key("", "abc"));
}

#[test]
fn ranges_cover_what_players_ask_for() {
    // Видео в браузере начинает с этого.
    assert_eq!(parse_range("bytes=0-", 1000), Range::Part { start: 0, end: 999 });
    assert_eq!(parse_range("bytes=100-199", 1000), Range::Part { start: 100, end: 199 });
    // Конец за пределами файла урезается, а не отвергается.
    assert_eq!(parse_range("bytes=900-5000", 1000), Range::Part { start: 900, end: 999 });
    // Хвост файла.
    assert_eq!(parse_range("bytes=-100", 1000), Range::Part { start: 900, end: 999 });
    assert_eq!(parse_range("bytes=-5000", 1000), Range::Part { start: 0, end: 999 });
    // Докачка того, что уже целиком скачано.
    assert_eq!(parse_range("bytes=1000-", 1000), Range::Unsatisfiable);
    assert_eq!(parse_range("bytes=-0", 1000), Range::Unsatisfiable);
    assert_eq!(parse_range("bytes=0-", 0), Range::Unsatisfiable);
    // Непонятное — целиком.
    assert_eq!(parse_range("bytes=0-1,5-6", 1000), Range::Full);
    assert_eq!(parse_range("items=0-1", 1000), Range::Full);
    assert_eq!(parse_range("bytes=5-1", 1000), Range::Full);
    assert_eq!(parse_range("bytes=x-", 1000), Range::Full);
}

// ---------------------------------------------------------------------------
// Имена файлов
// ---------------------------------------------------------------------------

#[test]
fn file_names_from_the_phone_cannot_become_paths() {
    assert_eq!(clean_name("IMG_0001.JPG").as_deref(), Some("IMG_0001.JPG"));
    assert_eq!(clean_name("Фото с отпуска.jpg").as_deref(), Some("Фото с отпуска.jpg"));
    // Всё до последнего разделителя отрезается — любого из двух.
    assert_eq!(clean_name("../../etc/passwd").as_deref(), Some("passwd"));
    assert_eq!(clean_name("..\\..\\Windows\\win.ini").as_deref(), Some("win.ini"));
    assert_eq!(clean_name("C:\\x\\y.txt").as_deref(), Some("y.txt"));
    // От имени ничего не осталось.
    for raw in ["", ".", "..", "dir/", " . ", "/"] {
        assert_eq!(clean_name(raw), None, "{raw:?}");
    }
}

#[test]
fn file_names_lose_what_windows_forbids() {
    assert_eq!(clean_name("a<b>c:d\"e|f?g*h.txt").as_deref(), Some("a_b_c_d_e_f_g_h.txt"));
    assert_eq!(clean_name("line\nbreak\u{0}.txt").as_deref(), Some("line_break_.txt"));
    // Точки и пробелы в конце Windows молча отрезает — имя разошлось бы.
    assert_eq!(clean_name("report.pdf. . ").as_deref(), Some("report.pdf"));
    // Точка в начале — скрытый файл, а ещё и наш временный.
    assert_eq!(clean_name(".savio-upload-1.part").as_deref(), Some("savio-upload-1.part"));
    // Знак смены направления письма показал бы `.exe` как `.jpg`.
    assert_eq!(clean_name("photo\u{202E}gpj.exe").as_deref(), Some("photogpj.exe"));
}

#[test]
fn windows_device_names_are_defused() {
    for (raw, clean) in [
        ("CON", "_CON"),
        ("nul.txt", "_nul.txt"),
        ("Com1.mp4", "_Com1.mp4"),
        ("LPT9", "_LPT9"),
        ("aux .tar.gz", "_aux .tar.gz"),
    ] {
        assert_eq!(clean_name(raw).as_deref(), Some(clean), "{raw}");
    }
    // Похожие, но законные имена не трогаются.
    for name in ["CONSOLE.txt", "COM10", "COM0", "nullable.rs", "LPT"] {
        assert_eq!(clean_name(name).as_deref(), Some(name));
    }
}

#[test]
fn long_names_keep_their_extension() {
    let long = format!("{}.mp4", "я".repeat(200));
    let cleaned = clean_name(&long).expect("имя есть");
    assert!(cleaned.len() <= NAME_LIMIT, "{} байт", cleaned.len());
    assert!(cleaned.ends_with(".mp4"), "{cleaned}");
    // Двухбайтная буква не разрезана пополам — иначе это не строка.
    assert!(cleaned.trim_end_matches(".mp4").chars().all(|c| c == 'я'));

    let no_ext = "a".repeat(400);
    assert_eq!(clean_name(&no_ext).map(|name| name.len()), Some(NAME_LIMIT));
}

#[test]
fn numbered_names_go_before_the_extension() {
    assert_eq!(numbered("IMG.jpg", 1), "IMG.jpg");
    assert_eq!(numbered("IMG.jpg", 2), "IMG (2).jpg");
    assert_eq!(numbered("archive.tar.gz", 3), "archive.tar (3).gz");
    assert_eq!(numbered("README", 2), "README (2)");
}

/// Совпавшее имя не затирает лежащий файл, а получает номер.
#[test]
fn an_upload_never_overwrites_a_file() {
    let dir = scratch("place");
    fs::write(dir.join("IMG.jpg"), b"old").expect("старый файл");

    for (content, expected) in [(b"new1", "IMG (2).jpg"), (b"new2", "IMG (3).jpg")] {
        let temp = dir.join(".savio-upload-x.part");
        fs::write(&temp, content).expect("временный файл");
        assert_eq!(
            place(&dir, "IMG.jpg", &temp, Lang::Ru).expect("лёг"),
            expected
        );
        assert!(!temp.exists(), "временный файл остался");
        assert_eq!(fs::read(dir.join(expected)).expect("прочитать"), content);
    }
    assert_eq!(fs::read(dir.join("IMG.jpg")).expect("прочитать"), b"old");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn only_files_of_the_folder_itself_are_served() {
    let dir = scratch("resolve");
    let shared = dir.join("shared");
    fs::create_dir_all(shared.join("inner")).expect("вложенная папка");
    fs::write(shared.join("clip.mp4"), b"x").expect("файл");
    fs::write(dir.join("secret.txt"), b"x").expect("файл снаружи");

    assert!(resolve(&shared, "clip.mp4").is_some());
    for name in ["../secret.txt", "..\\secret.txt", "..", "inner", "missing.mp4", "", "inner/../clip.mp4"] {
        assert!(resolve(&shared, name).is_none(), "отдан {name:?}");
    }

    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Ключ, устройства, адреса, отдача
// ---------------------------------------------------------------------------

#[test]
fn keys_are_long_readable_and_different() {
    let first = generate_key();
    let second = generate_key();
    for key in [&first, &second] {
        assert_eq!(key.len(), KEY_LEN);
        assert!(key.bytes().all(|b| KEY_ALPHABET.contains(&b)), "{key}");
    }
    assert_ne!(first, second);
}

#[test]
fn devices_are_named_by_user_agent() {
    let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15";
    let android = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/126.0 Mobile";
    let ipad = "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X)";
    assert_eq!(device_name(iphone, Lang::Ru), "iPhone");
    assert_eq!(device_name(android, Lang::Ru), "Android");
    assert_eq!(device_name(ipad, Lang::Ru), "iPad");
    assert_eq!(device_name("", Lang::Ru), "Устройство");
    // Имена систем — названия, а не слова: на другом языке они те же.
    assert_eq!(device_name(android, Lang::En), "Android");
    assert_eq!(device_name("", Lang::En), "Device");
}

#[test]
fn the_route_address_comes_first_and_virtual_ones_last() {
    let lan = Ipv4Addr::new(192, 168, 1, 42);
    let wsl = Ipv4Addr::new(172, 24, 160, 1);
    let found = vec![
        ("vEthernet (WSL)".to_owned(), wsl),
        ("Loopback Pseudo-Interface 1".to_owned(), Ipv4Addr::LOCALHOST),
        ("Ethernet 2".to_owned(), Ipv4Addr::new(10, 0, 0, 5)),
        ("Wi-Fi".to_owned(), lan),
        ("Wi-Fi".to_owned(), Ipv4Addr::new(169, 254, 3, 4)),
        ("Wi-Fi".to_owned(), Ipv4Addr::new(8, 8, 8, 8)),
        ("Wi-Fi copy".to_owned(), lan),
    ];

    let ranked = rank_addresses(Some(lan), found.clone());
    let ips: Vec<Ipv4Addr> = ranked.iter().map(|(_, ip)| *ip).collect();
    // Петля, link-local и внешние отсеяны, повтор схлопнут.
    assert_eq!(ips, vec![lan, Ipv4Addr::new(10, 0, 0, 5), wsl]);
    assert_eq!(ranked[0].0, "Wi-Fi");

    // Маршрута нет — первым встаёт настоящий интерфейс, а не WSL.
    let ranked = rank_addresses(None, found);
    assert_eq!(ranked.last().map(|(_, ip)| *ip), Some(wsl));

    // Список интерфейсов пуст, но маршрут назвал адрес — показываем его.
    assert_eq!(rank_addresses(Some(lan), Vec::new()), vec![(String::new(), lan)]);
    // Маршрут через внешний адрес (VPN с белым IP) в показ не идёт.
    assert!(rank_addresses(Some(Ipv4Addr::new(8, 8, 8, 8)), Vec::new()).is_empty());
}

/// Всё, что браузер исполнил бы, уходит только скачиванием.
#[test]
fn pages_from_the_folder_are_never_shown_inline() {
    for name in ["page.html", "image.svg", "x.htm", "x.xml", "noext"] {
        assert!(forced_download(name), "{name}");
    }
    for name in ["clip.MP4", "photo.jpeg", "song.mp3", "doc.pdf"] {
        assert!(!forced_download(name), "{name}");
    }
    assert_eq!(content_type("clip.MP4"), "video/mp4");
}

/// Служебные файлы Windows в список на телефоне не попадают — в том числе
/// с рабочего стола, где `desktop.ini` есть всегда.
#[test]
fn shell_files_are_not_listed() {
    let dir = scratch("list");
    for name in ["clip.mp4", "desktop.ini", "Thumbs.db", ".hidden", ".savio-upload-3.part"] {
        fs::write(dir.join(name), b"x").expect("файл");
    }
    let list = list_files(&dir);
    let names: Vec<&str> = list
        .as_array()
        .expect("массив")
        .iter()
        .map(|file| file["name"].as_str().expect("имя"))
        .collect();
    assert_eq!(names, vec!["clip.mp4"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn disposition_carries_a_cyrillic_name() {
    assert_eq!(
        disposition("Фото 1.jpg", true),
        "attachment; filename=\"____ 1.jpg\"; filename*=UTF-8''%D0%A4%D0%BE%D1%82%D0%BE%201.jpg"
    );
    assert!(disposition("a\"b.jpg", false).starts_with("inline; filename=\"a_b.jpg\""));
}

// ---------------------------------------------------------------------------
// Живой сервер на петле
// ---------------------------------------------------------------------------

/// Адрес для тестов: петля, а не сеть. Слушать всю сеть в тесте нельзя —
/// каждый прогон нового бинарника спрашивал бы брандмауэр Windows.
fn loopback(port: u16, key: &str) -> Vec<ShareAddress> {
    vec![ShareAddress::new(Ipv4Addr::LOCALHOST, port, key, "петля")]
}

struct Live {
    handle: Handle,
    rx: Receiver<Event>,
    port: u16,
    key: String,
}

fn live(dir: &Path) -> Live {
    let (tx, rx) = channel();
    let handle = start_with(
        Listen {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            ports: &[0],
            find: loopback,
        },
        dir.to_owned(),
        Lang::Ru,
        tx,
        || {},
    );

    let Ok(Event::Share(ShareEvent::Ready(addresses))) = rx.recv_timeout(Duration::from_secs(5)) else {
        panic!("раздача не запустилась");
    };
    // `http://127.0.0.1:PORT/?k=KEY`
    let url = &addresses[0].url;
    let rest = url.strip_prefix("http://127.0.0.1:").expect("адрес петли");
    let (port, key) = rest.split_once("/?k=").expect("ключ в адресе");
    Live {
        handle,
        rx,
        port: port.parse().expect("порт"),
        key: key.to_owned(),
    }
}

/// Шлёт запрос целиком и читает ответ до закрытия подключения.
fn http(port: u16, request: &[u8]) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("подключиться");
    stream.set_read_timeout(Some(Duration::from_secs(10))).expect("таймаут");
    stream.write_all(request).expect("отправить");
    let mut raw = Vec::new();
    if let Err(error) = stream.read_to_end(&mut raw) {
        let line = String::from_utf8_lossy(request);
        panic!("ответ на «{}» не прочитан: {error}", line.lines().next().unwrap_or_default());
    }

    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("заголовок ответа");
    let head = String::from_utf8(raw[..split].to_vec()).expect("заголовок текстом");
    let status = head[9..12].parse().expect("код ответа");
    (status, head, raw[split + 4..].to_vec())
}

/// Ждёт событие раздачи, пропуская прогресс.
fn next_share(rx: &Receiver<Event>) -> ShareEvent {
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Share(ShareEvent::Progress { .. })) => {}
            Ok(Event::Share(event)) => return event,
            Ok(other) => panic!("чужое событие: {other:?}"),
            Err(error) => panic!("события нет: {error:?}"),
        }
    }
}

#[test]
fn a_phone_sends_lists_and_takes_files() {
    let dir = scratch("live");
    let outside = dir.join("secret.txt");
    let shared = dir.join("shared");
    fs::create_dir_all(&shared).expect("папка раздачи");
    fs::write(&outside, b"secret").expect("файл снаружи");
    let server = live(&shared);
    let (port, key) = (server.port, server.key.as_str());

    // Без ключа и с чужим — отказ, и страницы нет.
    let (status, _, body) = http(port, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status, 403);
    assert!(String::from_utf8_lossy(&body).contains("Ссылка устарела"));
    assert_eq!(http(port, b"GET /api/files?k=wrongwrongwr HTTP/1.1\r\n\r\n").0, 403);

    // Со своим — страница, и о посетителе сказано.
    let request = format!("GET /?k={key} HTTP/1.1\r\nUser-Agent: Mozilla/5.0 (Linux; Android 14)\r\n\r\n");
    let (status, head, body) = http(port, request.as_bytes());
    assert_eq!(status, 200);
    assert!(head.contains("text/html"));
    // Сверяем с собранной страницей, а не с шаблоном `PAGE`: в нём ещё
    // стоят `{{Слоты}}`, и равенство с ним значило бы, что подстановка не
    // сработала вовсе. Язык — тот, с каким запущен сервер (`live`).
    assert_eq!(body, page(Lang::Ru, &folder_name(&shared)).as_bytes());
    // И отдельно: слотов в отданном теле не осталось ни одного. Без этой
    // строки забытый слот выглядел бы как исправная страница — `page`
    // и сервер брали бы его из одного места и сошлись бы на `{{…}}`.
    assert!(
        !String::from_utf8_lossy(&body).contains("{{"),
        "в отданной странице остался неподставленный слот"
    );
    match next_share(&server.rx) {
        ShareEvent::Visitor(who) => assert_eq!(who, "Android · 127.0.0.1"),
        other => panic!("ждали посетителя, пришло {other:?}"),
    }

    // Отправка с телефона: имя по-русски, дважды одно и то же.
    let name = "%D0%A4%D0%BE%D1%82%D0%BE.jpg";
    for (body, saved) in [(&b"hello world"[..], "Фото.jpg"), (&b"second"[..], "Фото (2).jpg")] {
        let mut request =
            format!("PUT /upload/{name}?k={key} HTTP/1.1\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        request.extend_from_slice(body);
        let (status, _, answer) = http(port, &request);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
        assert!(String::from_utf8_lossy(&answer).contains(saved));

        assert!(matches!(next_share(&server.rx), ShareEvent::Started { direction: TransferDirection::ToComputer, .. }));
        match next_share(&server.rx) {
            ShareEvent::Finished { name, .. } => assert_eq!(name, saved),
            other => panic!("ждали конца приёма, пришло {other:?}"),
        }
        assert_eq!(fs::read(shared.join(saved)).expect("файл лёг"), body);
    }

    // Список: оба файла, временных нет.
    let (status, _, body) = http(port, format!("GET /api/files?k={key} HTTP/1.1\r\n\r\n").as_bytes());
    assert_eq!(status, 200);
    let list: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let mut names: Vec<&str> = list
        .as_array()
        .expect("массив")
        .iter()
        .map(|file| file["name"].as_str().expect("имя"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Фото (2).jpg", "Фото.jpg"]);

    // Кусок файла — как его просит видеоплеер.
    let request = format!("GET /files/{name}?k={key} HTTP/1.1\r\nRange: bytes=6-10\r\n\r\n");
    let (status, head, body) = http(port, request.as_bytes());
    assert_eq!(status, 206);
    assert!(head.contains("Content-Range: bytes 6-10/11"), "{head}");
    assert_eq!(body, b"world");

    // Файл целиком, кнопкой «Скачать».
    let request = format!("GET /files/{name}?k={key}&dl=1 HTTP/1.1\r\n\r\n");
    let (status, head, body) = http(port, request.as_bytes());
    assert_eq!(status, 200);
    assert!(head.contains("attachment"), "{head}");
    assert_eq!(body, b"hello world");

    // Наружу папки не выбраться ни так, ни этак.
    for path in ["/files/..%2Fsecret.txt", "/files/..%5Csecret.txt", "/files/../secret.txt"] {
        let (status, _, body) = http(port, format!("GET {path}?k={key} HTTP/1.1\r\n\r\n").as_bytes());
        assert_eq!(status, 404, "{path}");
        assert!(!body.windows(6).any(|w| w == b"secret"), "{path}");
    }

    // Остановка закрывает порт — сама, без чужого подключения. Проверяем
    // занятием порта, а не подключением к нему: подключение разбудило бы
    // спящий `accept` и сделало бы работу будильника за него.
    server.handle.stop();
    let closed = Instant::now();
    while TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        assert!(closed.elapsed() < Duration::from_secs(5), "порт открыт после остановки");
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_dir_all(&dir);
}

/// Остановка обрывает идущий приём, а не ждёт его конца.
///
/// Телефон здесь, как настоящий, продолжает отправлять и после остановки:
/// объявлено 64 МБ — столько, что без обрыва передача успела бы доехать
/// и тест поймал бы «Готово» вместо обрыва.
///
/// Почему не проверяем молчащий телефон — хотя первая версия теста именно
/// это и делала. На Windows `shutdown` не прерывает уже идущий `recv`, и
/// поток спит до `IO_TIMEOUT`; на Linux и macOS просыпается сразу. Такой
/// тест был бы красным на одной системе из трёх, и не из-за ошибки кода,
/// а из-за Windows (см. `Shared`). Гарантировано везде другое: пришедшие
/// после остановки данные файл уже не пополнят.
#[test]
fn stopping_cuts_an_upload_in_flight() {
    let dir = scratch("cut");
    let server = live(&dir);

    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port)).expect("подключиться");
    // Таймаут — сразу, пока подключение живо. После обрыва macOS отвечает на
    // `setsockopt` «Invalid argument», и тест падал на подготовке проверки,
    // а не на ней самой (CI 0.27.0; Windows и Linux это пропускают).
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("таймаут");
    let head = format!(
        "PUT /upload/big.mp4?k={} HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        server.key,
        64 * 1024 * 1024
    );
    stream.write_all(head.as_bytes()).expect("заголовок");
    stream.write_all(&vec![7u8; 1024 * 1024]).expect("первый мегабайт");

    assert!(matches!(next_share(&server.rx), ShareEvent::Started { .. }));
    let temp_exists = || {
        fs::read_dir(&dir)
            .expect("папка")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
    };
    assert!(temp_exists(), "приём не начался");

    server.handle.stop();

    // Остальные 63 МБ — после остановки. Ошибка записи здесь ожидаема:
    // оборванное подключение и есть то, что проверяем.
    let mut sender = stream.try_clone().expect("вторая ручка");
    let sending = std::thread::spawn(move || {
        let chunk = vec![7u8; 1024 * 1024];
        for _ in 0..63 {
            if sender.write_all(&chunk).is_err() {
                return;
            }
        }
    });

    match next_share(&server.rx) {
        ShareEvent::Failed { message, .. } => assert!(message.contains("остановлена"), "{message}"),
        other => panic!("ждали обрыва, пришло {other:?}"),
    }
    // Подключение закрыто сервером: чтение не висит до таймаута.
    let mut rest = Vec::new();
    let cut = Instant::now();
    let _ = stream.read_to_end(&mut rest);
    assert!(cut.elapsed() < Duration::from_secs(4), "подключение осталось открытым");
    let _ = sending.join();

    assert!(!temp_exists(), "недопринятый файл остался");
    assert!(!dir.join("big.mp4").exists(), "недопринятый файл выглядит готовым");
    assert!(matches!(server.rx.recv_timeout(Duration::from_millis(200)), Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected)));

    let _ = fs::remove_dir_all(&dir);
}

/// Имя раздаваемой папки уходит на страницу экранированным.
///
/// Это не строка из таблицы, а данные пользователя, и проверка
/// `the_phone_page_strings_are_safe_to_paste` про него не знает: она смотрит
/// только таблицу строк. Папка по имени `<b>` иначе сломала бы разметку на
/// телефоне, а `Tom & Jerry` — превратилась бы в неверную сущность.
///
/// Проверено красным: без `escape_html` проверка падает на первом же имени.
#[test]
fn the_folder_name_is_escaped_on_the_phone_page() {
    let html = page(Lang::Ru, "<b>Tom & \"Jerry\"</b>");
    assert!(!html.contains("<b>Tom"), "имя папки попало в разметку как есть");
    assert!(
        html.contains("&lt;b&gt;Tom &amp; &quot;Jerry&quot;&lt;/b&gt;"),
        "имя папки не экранировано"
    );
}

/// На страницу уходит имя папки, а не путь к ней.
///
/// Страницу видит любой, у кого есть адрес, а полный путь рассказал бы ему
/// имя пользователя и букву диска. Имя нужно ровно затем, чтобы человек
/// с телефона видел, куда уедут его файлы.
#[test]
fn only_the_folder_name_reaches_the_phone() {
    let dir = std::path::Path::new("C:/Users/Вася/Загрузки");
    assert_eq!(folder_name(dir), "Загрузки");
    let html = page(Lang::Ru, &folder_name(dir));
    assert!(!html.contains("Вася"), "в страницу утёк путь целиком");
}
