//! Личное внутри XMP: место, время, владелец камеры и серийные номера.
//!
//! Строка «XMP: присутствует, 4.0 КБ» говорит только об объёме, а Lightroom,
//! Photoshop и программы телефонов кладут в XMP те же координаты и дату
//! съёмки, что и в EXIF. Опаснее всего снимок, у которого программа EXIF
//! срезала, а XMP оставила: без разбора таблица честно писала бы «места нет»,
//! и человек отправлял бы файл, не стирая, — вместе с координатами.
//!
//! # Почему текстом, а не XML-библиотекой
//!
//! Нужно полтора десятка свойств, а XML-библиотека — новая зависимость ради
//! них (CLAUDE.md, «Общее»). Пакет XMP — ограниченное подмножество RDF/XML:
//! свойство пишется либо атрибутом (`exif:GPSLatitude="40,10.632N"`), либо
//! элементом, иногда с `rdf:Seq`/`rdf:Alt` внутри. Этого хватает одного
//! прохода по тегам.
//!
//! # Префикс — не имя пространства
//!
//! `exif:` и `photoshop:` — только привычка: у XMP префикс объявляется в самом
//! пакете (`xmlns:e="http://ns.adobe.com/exif/1.0/"`), и программа вправе
//! назвать его как угодно. Поиск по привычному префиксу молча пропускал бы
//! часть файлов, а под тот же префикс, привязанный к чужому пространству,
//! находил бы чужие свойства. Поэтому свойство узнаётся по адресу
//! пространства, к которому привязан его префикс.

use std::borrow::Cow;

use crate::i18n::{self, Key, Lang};
use crate::model::{Tag, TagRole};

use super::{Moment, PLACE_JOIN, be_u32, exif_moment, place_value, text_value};

/// Пространства имён, в которых живут нужные свойства.
///
/// Адреса сравниваются целиком, а не по началу: адрес `aux` начинается
/// с адреса `exif`, и сравнение по началу приписало бы серийный номер
/// объектива пространству EXIF.
const EXIF: &str = "http://ns.adobe.com/exif/1.0/";
const EXIF_AUX: &str = "http://ns.adobe.com/exif/1.0/aux/";
/// Пространство CIPA для записей EXIF 2.3 — там живут номера камеры
/// и объектива и владелец.
const EXIF_EX: &str = "http://cipa.jp/exif/1.0/";
const PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
const XMP_BASIC: &str = "http://ns.adobe.com/xap/1.0/";

const SPACES: [&str; 5] = [EXIF, EXIF_AUX, EXIF_EX, PHOTOSHOP, XMP_BASIC];

/// Что свойство значит для таблицы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Latitude,
    Longitude,
    Altitude,
    /// «1» — ниже уровня моря, как 0x0005 в каталоге GPS.
    AltitudeRef,
    /// Дата съёмки — пара EXIF 0x9003.
    Taken,
    /// Дата оцифровки — пара EXIF 0x9004.
    Digitized,
    /// Когда создан файл: у снимка с камеры это та же съёмка, у картинки
    /// из редактора — время, когда её начали.
    Created,
    /// Время по часам GPS, по Гринвичу.
    GpsTime,
    Owner,
    CameraSerial,
    LensSerial,
}

/// Свойства, которые разбираются, — те же, что EXIF-сторона отмечает личными
/// (`TagRole::personal`): место, время, владелец и оба серийных номера.
/// Держать стороны симметричными обязательно: запись, которую EXIF красит
/// жёлтым, а её двойник в XMP проходит молча, — ровно та беда, ради которой
/// этот разбор заведён.
const FIELDS: [(&str, &str, Field); 15] = [
    (EXIF, "GPSLatitude", Field::Latitude),
    (EXIF, "GPSLongitude", Field::Longitude),
    (EXIF, "GPSAltitude", Field::Altitude),
    (EXIF, "GPSAltitudeRef", Field::AltitudeRef),
    (EXIF, "DateTimeOriginal", Field::Taken),
    // Photoshop кладёт сюда дату съёмки, а не создания файла: так её
    // сопоставляет с EXIF и Metadata Working Group.
    (PHOTOSHOP, "DateCreated", Field::Taken),
    (EXIF, "DateTimeDigitized", Field::Digitized),
    (XMP_BASIC, "CreateDate", Field::Created),
    (EXIF, "GPSTimeStamp", Field::GpsTime),
    (EXIF_AUX, "OwnerName", Field::Owner),
    (EXIF_EX, "CameraOwnerName", Field::Owner),
    (EXIF_AUX, "SerialNumber", Field::CameraSerial),
    (EXIF_EX, "BodySerialNumber", Field::CameraSerial),
    (EXIF_AUX, "LensSerialNumber", Field::LensSerial),
    (EXIF_EX, "LensSerialNumber", Field::LensSerial),
];

/// Сколько найденных свойств держать и сколько объявлений пространств
/// помнить. Настоящему пакету хватает по десятку; предел — от враждебного
/// файла, который повторяет одно свойство сто тысяч раз.
const FOUND_LIMIT: usize = 64;

/// Длиннее этого значения не бывает ни у координаты, ни у даты, ни у
/// серийного номера. Всё, что длиннее, — не то, что мы ищем, и копировать
/// его незачем.
const VALUE_BYTES_LIMIT: usize = 1024;

/// Как далеко от открывающего тега свойства искать закрывающий. Значение со
/// всеми `rdf:Alt` и отступами занимает сотню байт; без предела пакет из
/// тысяч незакрытых свойств заставил бы искать конец каждого до конца
/// мегабайта.
const ELEMENT_WINDOW: usize = 4096;

/// Найденное в одном или нескольких пакетах файла.
#[derive(Default)]
pub(super) struct Facts {
    found: Vec<(Field, String)>,
}

impl Facts {
    /// Разбирает пакет: UTF-8 — прямо по срезу, UTF-16 — одной перекодировкой.
    pub(super) fn scan(&mut self, packet: &[u8]) {
        let text = decode(packet);
        let spaces = namespaces(&text);
        if spaces.is_empty() {
            return;
        }
        walk(&text, &spaces, |field, value| self.keep(field, value));
    }

    fn keep(&mut self, field: Field, value: &str) {
        let value = unescape(value.trim());
        // Пустое свойство — не запись: жёлтая строка ни о чём обесценила бы
        // жёлтые строки по делу.
        if value.is_empty() || value.len() > VALUE_BYTES_LIMIT || self.found.len() >= FOUND_LIMIT
        {
            return;
        }
        self.found.push((field, value.into_owned()));
    }

    fn values(&self, field: Field) -> impl Iterator<Item = &str> {
        self.found
            .iter()
            .filter(move |(f, _)| *f == field)
            .map(|(_, value)| value.as_str())
    }

    /// Личные записи XMP, которых нет среди `known`.
    ///
    /// Значение доводится до числа или даты: «нашли слово GPSLatitude» — ещё
    /// не место, и неразборчивое свойство записью не становится. Совпавшее
    /// с уже прочитанным (обычно с EXIF) не задваивается — сравниваются
    /// значения, приведённые к одному виду, а не сырые строки: EXIF хранит
    /// координаты тремя дробями, XMP — строкой «40,10.632N».
    pub(super) fn tags(&self, known: &[Tag], lang: Lang) -> Vec<Tag> {
        let mut out: Vec<Tag> = Vec::new();
        let add = |out: &mut Vec<Tag>, role: TagRole, key: Key, value: String| {
            let tag = Tag::new(role, from_xmp(key, lang), value);
            if says_more(&tag, known, out) {
                out.push(tag);
            }
        };

        let latitude = self
            .values(Field::Latitude)
            .find_map(|value| coordinate(value, 'N', 'S', 90.0));
        let longitude = self
            .values(Field::Longitude)
            .find_map(|value| coordinate(value, 'E', 'W', 180.0));
        let below = self.values(Field::AltitudeRef).any(|value| value == "1");
        let metres = self
            .values(Field::Altitude)
            .find_map(rational)
            .map(|metres| if below { -metres } else { metres });
        if let Some(value) = place_value([latitude, longitude], metres, lang) {
            add(&mut out, TagRole::Place, Key::TagPlace, value);
        }

        for (field, key) in [
            (Field::Taken, Key::TagDateTaken),
            (Field::Digitized, Key::TagDateDigitized),
            (Field::Created, Key::TagDateCreated),
        ] {
            for value in self.values(field).filter_map(|value| date(value, lang)) {
                add(&mut out, TagRole::Taken, key, value);
            }
        }
        // Время по часам GPS — то же мгновение по Гринвичу. Как и у EXIF,
        // оно отвечает на «когда снято» только там, где другого ответа нет.
        if !known.iter().chain(&out).any(|tag| tag.role == TagRole::Taken)
            && let Some(value) = self
                .values(Field::GpsTime)
                .find_map(|value| exif_moment(strip_zone(value).0))
                .map(|moment| format!("{} UTC", moment.words(lang)))
        {
            add(&mut out, TagRole::Taken, Key::TagDateTaken, value);
        }

        for (field, role, key) in [
            (Field::Owner, TagRole::Owner, Key::TagCameraOwner),
            (Field::CameraSerial, TagRole::Serial, Key::TagCameraSerial),
            (Field::LensSerial, TagRole::Serial, Key::TagLensSerial),
        ] {
            for value in self.values(field).map(|value| text_value(value.as_bytes())) {
                if !value.is_empty() {
                    add(&mut out, role, key, value);
                }
            }
        }
        out
    }
}

/// «Координаты места (XMP)».
///
/// Помета стоит всегда, а не только рядом с разошедшимся EXIF: у снимка,
/// с которого EXIF срезала другая программа, она объясняет, откуда место
/// взялось, — человек был уверен, что его там уже нет.
fn from_xmp(key: Key, lang: Lang) -> String {
    i18n::fill(i18n::t(lang, Key::TagFromXmp), &[i18n::t(lang, key)])
}

/// Говорит ли запись то, чего среди уже прочитанного нет.
fn says_more(tag: &Tag, known: &[Tag], added: &[Tag]) -> bool {
    !known
        .iter()
        .chain(added)
        .filter(|other| other.role == tag.role)
        .any(|other| covers(&other.value, tag))
}

fn covers(shown: &str, tag: &Tag) -> bool {
    if shown == tag.value {
        return true;
    }
    match tag.role {
        // «40.1772, 44.5035» уже сказано строкой «40.1772, 44.5035 · 1180 м».
        // Обратное неверно: высота, которой в EXIF нет, — новость.
        TagRole::Place => tag
            .value
            .split(PLACE_JOIN)
            .all(|part| shown.split(PLACE_JOIN).any(|seen| seen == part)),
        // «14 сен 2025» уже сказано строкой «14 сен 2025, 19:42»: дату без
        // времени Photoshop пишет рядом с полной датой EXIF постоянно.
        TagRole::Taken => shown
            .strip_prefix(tag.value.as_str())
            .is_some_and(|rest| rest.starts_with(',')),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Значения
// ---------------------------------------------------------------------------

/// Координата XMP в градусах: юг и запад — с минусом.
///
/// XMP пишет её не тремя дробями, как EXIF, а строкой в одной из двух форм:
/// «градусы,минуты,секунды и полушарие» (`40,10,38N`) или «градусы,минуты.доли
/// и полушарие» (`40,10.632N`). Встречаются и десятичные градусы со знаком
/// без буквы — их понимаем тоже. Всё, что вне диапазона, — не координата.
fn coordinate(text: &str, positive: char, negative: char, limit: f64) -> Option<f64> {
    let text = text.trim();
    let last = text.chars().next_back()?;
    let (number, sign) = if last.eq_ignore_ascii_case(&positive) {
        (&text[..text.len() - 1], Some(1.0))
    } else if last.eq_ignore_ascii_case(&negative) {
        (&text[..text.len() - 1], Some(-1.0))
    } else {
        (text, None)
    };

    let mut parts = number.split(',').map(str::trim);
    let degrees = finite(parts.next()?)?;
    let minutes = parts.next().map_or(Some(0.0), finite)?;
    let seconds = parts.next().map_or(Some(0.0), finite)?;
    if parts.next().is_some()
        || !(0.0..60.0).contains(&minutes)
        || !(0.0..60.0).contains(&seconds)
        // Буква полушария уже несёт знак, второй минус — противоречие.
        || (sign.is_some() && degrees < 0.0)
    {
        return None;
    }
    let value = degrees.abs() + minutes / 60.0 + seconds / 3600.0;
    let value = match sign {
        Some(sign) => sign * value,
        None if degrees.is_sign_negative() => -value,
        None => value,
    };
    (value.abs() <= limit).then_some(value)
}

/// Число из строки — только конечное: `f64::from_str` охотно понимает
/// «NaN» и «inf», а градусов «inf» не бывает.
fn finite(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// Дробь XMP («1180/1») или просто число.
fn rational(text: &str) -> Option<f64> {
    match text.split_once('/') {
        Some((n, d)) => {
            let (n, d) = (finite(n.trim())?, finite(d.trim())?);
            (d != 0.0).then(|| n / d)
        }
        None => finite(text.trim()),
    }
}

/// Дата XMP словами — тем же видом, что у EXIF: «14 сен 2025, 19:42».
///
/// XMP пишет даты по ISO 8601, и не всегда полностью: без секунд
/// (`2025-09-14T19:42+04:00`), без времени, а у Photoshop бывает один год.
/// Пояс отбрасывается — местное время читается так же, как у EXIF, и
/// совпавшие даты совпадают строкой. Кроме «Z»: время по Гринвичу без
/// пометы разошлось бы с местным на часы, поэтому ему приписывается «UTC».
fn date(text: &str, lang: Lang) -> Option<String> {
    let (text, utc) = strip_zone(text);
    if let Some(moment) = exif_moment(text) {
        return Some(words(moment, utc, lang));
    }
    partial_date(text)
}

fn words(moment: Moment, utc: bool, lang: Lang) -> String {
    let words = moment.words(lang);
    if utc { format!("{words} UTC") } else { words }
}

/// Отрезает часовой пояс: «Z», «+04:00» или «-05:00». Дата сама пишется
/// через дефисы, поэтому пояс ищется только во времени, после «T».
fn strip_zone(text: &str) -> (&str, bool) {
    let text = text.trim();
    let Some(at) = text.find(['T', ' ']) else {
        return (text, false);
    };
    let time = &text[at + 1..];
    if let Some(clock) = time.strip_suffix(['Z', 'z']) {
        return (&text[..at + 1 + clock.len()], true);
    }
    match time.find(['+', '-']) {
        Some(zone) => (&text[..at + 1 + zone], false),
        None => (text, false),
    }
}

/// Год или год с месяцем — так, как записаны: «2025», «2025-09».
///
/// Разобрать их до дня нельзя, но и неразборчивыми они не являются: это
/// законная дата XMP, и о времени съёмки она говорит.
fn partial_date(text: &str) -> Option<String> {
    let digits = |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = text.split('-');
    let year = parts.next()?;
    let month = parts.next();
    if parts.next().is_some() || !digits(year, 4) || year == "0000" {
        return None;
    }
    if let Some(month) = month
        && !(digits(month, 2) && (1..=12).contains(&month.parse::<u32>().ok()?))
    {
        return None;
    }
    Some(text.to_owned())
}

// ---------------------------------------------------------------------------
// Разбор пакета
// ---------------------------------------------------------------------------

/// Текст пакета. UTF-8 (с BOM или без) отдаётся срезом, без копии, если он
/// корректен; UTF-16 узнаётся по BOM или, без него, по нулевому байту
/// в первой паре: пакет начинается с «<» или пробела, и у UTF-16 один
/// из двух первых байт — ноль.
///
/// UTF-32, который спецификация тоже разрешает, не разбирается: ни одна
/// из известных программ им пакет не пишет.
fn decode(packet: &[u8]) -> Cow<'_, str> {
    match packet {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest),
        [0xFE, 0xFF, rest @ ..] => Cow::Owned(utf16(rest, true)),
        [0xFF, 0xFE, rest @ ..] => Cow::Owned(utf16(rest, false)),
        [0, second, ..] if *second != 0 => Cow::Owned(utf16(packet, true)),
        [first, 0, ..] if *first != 0 => Cow::Owned(utf16(packet, false)),
        _ => String::from_utf8_lossy(packet),
    }
}

fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|&pair| {
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Префиксы, привязанные в пакете к нужным пространствам.
///
/// Область видимости объявлений не учитывается: у XMP они стоят на
/// `rdf:Description` или выше, и один префикс, привязанный в разных местах
/// к разным адресам, — случай, которого в настоящих пакетах не бывает.
fn namespaces(text: &str) -> Vec<(&str, &'static str)> {
    let mut spaces: Vec<(&str, &'static str)> = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("xmlns:") {
        rest = &rest[at + "xmlns:".len()..];
        let Some(eq) = rest.find('=') else {
            break;
        };
        let prefix = rest[..eq].trim();
        let value = rest[eq + 1..].trim_start();
        let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            continue;
        };
        let Some(end) = value[1..].find(quote) else {
            break;
        };
        let uri = &value[1..1 + end];
        let named = !prefix.is_empty()
            && !prefix
                .bytes()
                .any(|b| b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'"' | b'\''));
        if let Some(&space) = SPACES.iter().find(|space| **space == uri)
            && named
            && spaces.len() < FOUND_LIMIT
            && !spaces.contains(&(prefix, space))
        {
            spaces.push((prefix, space));
        }
    }
    spaces
}

/// Что значит свойство с таким полным именем («e:GPSLatitude»).
fn field_of(qname: &str, spaces: &[(&str, &'static str)]) -> Option<Field> {
    let (prefix, local) = qname.split_once(':')?;
    spaces
        .iter()
        .filter(|(bound, _)| *bound == prefix)
        .find_map(|(_, uri)| {
            FIELDS
                .iter()
                .find(|(space, name, _)| space == uri && *name == local)
                .map(|(_, _, field)| *field)
        })
}

/// Один проход по тегам пакета: нужные свойства — и атрибутами, и
/// элементами — уходят в `found`.
///
/// Текст между тегами пропускается поиском следующего «<», поэтому
/// миниатюры в base64, занимающие большую часть мегабайтного пакета,
/// проходятся одним `memchr`.
fn walk(text: &str, spaces: &[(&str, &'static str)], mut found: impl FnMut(Field, &str)) {
    let mut hits = 0;
    let mut pos = 0;
    while let Some(at) = text[pos..].find('<') {
        let lt = pos + at;
        let rest = &text[lt + 1..];

        // Служебное: объявление пакета, комментарии, CDATA и закрывающие теги.
        let skip = if rest.starts_with('?') {
            Some("?>")
        } else if rest.starts_with("!--") {
            Some("-->")
        } else if rest.starts_with("![CDATA[") {
            Some("]]>")
        } else if rest.starts_with(['!', '/']) {
            Some(">")
        } else {
            None
        };
        if let Some(end) = skip {
            let Some(close) = rest.find(end) else {
                return;
            };
            pos = lt + 1 + close + end.len();
            continue;
        }

        let parsed = start_tag(rest, |name, value| {
            if let Some(field) = field_of(name, spaces) {
                hits += 1;
                found(field, value);
            }
        });
        let Some((name, len, empty)) = parsed else {
            pos = lt + 1;
            continue;
        };
        let after = lt + 1 + len;

        if !empty && let Some(field) = field_of(name, spaces) {
            hits += 1;
            let mut end = (after + ELEMENT_WINDOW).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let window = &text[after..end];
            // Закрывающий тег пишется тем же префиксом, что и открывающий,
            // и за именем сразу идёт «>» или пробел — иначе это другое
            // свойство, чьё имя начинается так же.
            if let Some(close) = window.match_indices("</").map(|(i, _)| i).find(|&i| {
                window[i + 2..].strip_prefix(name).is_some_and(|tail| {
                    tail.starts_with(|c: char| c == '>' || c.is_ascii_whitespace())
                })
            }) && let Some(value) = first_text(&window[..close])
            {
                found(field, value);
            }
        }
        if hits >= FOUND_LIMIT {
            return;
        }
        pos = after;
    }
}

/// Открывающий тег от имени до `>` (без самого `<`): атрибуты уходят
/// наблюдателю, наружу — имя, длина тега и то, закрыт ли он сам (`/>`).
/// `None` — тег сломан; тогда разбор идёт дальше со следующего «<».
fn start_tag<'t>(
    rest: &'t str,
    mut attribute: impl FnMut(&'t str, &'t str),
) -> Option<(&'t str, usize, bool)> {
    let b = rest.as_bytes();
    let stop = |c: u8| c.is_ascii_whitespace() || matches!(c, b'>' | b'/' | b'=');
    let name_end = b.iter().position(|&c| stop(c))?;
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let skip_space = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };

    let mut i = name_end;
    loop {
        i = skip_space(i);
        match *b.get(i)? {
            b'>' => return Some((name, i + 1, false)),
            b'/' => return (b.get(i + 1) == Some(&b'>')).then_some((name, i + 2, true)),
            _ => {}
        }
        let key_start = i;
        while b.get(i).is_some_and(|&c| !stop(c)) {
            i += 1;
        }
        let key = &rest[key_start..i];
        i = skip_space(i);
        if key.is_empty() || b.get(i) != Some(&b'=') {
            return None;
        }
        i = skip_space(i + 1);
        let quote = *b.get(i)?;
        if !matches!(quote, b'"' | b'\'') {
            return None;
        }
        let start = i + 1;
        let len = rest[start..].find(quote as char)?;
        attribute(key, &rest[start..start + len]);
        i = start + len + 1;
    }
}

/// Первый непустой текст внутри элемента: у простого свойства это оно само,
/// у `rdf:Seq` и `rdf:Alt` — первый `rdf:li`.
fn first_text(inner: &str) -> Option<&str> {
    let mut rest = inner;
    loop {
        let (text, tag) = match rest.find('<') {
            Some(at) => (&rest[..at], Some(&rest[at..])),
            None => (rest, None),
        };
        let text = text.trim();
        if !text.is_empty() {
            return Some(text);
        }
        let tag = tag?;
        rest = &tag[tag.find('>')? + 1..];
    }
}

/// Расшифровывает сущности XML: `&amp;`, `&lt;`, `&#x41;` и соседей.
/// Без сущностей — тот же срез, без копии.
fn unescape(value: &str) -> Cow<'_, str> {
    if !value.contains('&') {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let entity = tail
            .find(';')
            .filter(|&semi| semi <= 10)
            .and_then(|semi| Some((entity(&tail[1..semi])?, semi + 1)));
        match entity {
            Some((ch, len)) => {
                out.push(ch);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let number = name.strip_prefix('#')?;
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => number.parse().ok()?,
            };
            char::from_u32(code)?
        }
    })
}

// ---------------------------------------------------------------------------
// Где пакет лежит
// ---------------------------------------------------------------------------

/// Продолжения большого XMP в JPEG (сегменты `http://ns.adobe.com/xmp/extension/`).
///
/// Больше 64 КБ в один сегмент не влезает, и остаток пакета режется на
/// куски: у каждого 32 байта GUID (MD5 полного продолжения в шестнадцатеричном
/// виде), 4 байта полной длины, 4 байта смещения куска и сам кусок. Свойство
/// бывает разрезано посередине, поэтому куски собираются по смещению в один
/// пакет и разбираются вместе.
///
/// Разбираются все продолжения, а не только то, на которое ссылается
/// основной пакет (`xmpNote:HasExtendedXMP`): осиротевшее продолжение от
/// прежней правки уезжает с файлом так же, как и живое.
#[derive(Default)]
pub(super) struct Extensions<'a> {
    groups: Vec<Group<'a>>,
}

struct Group<'a> {
    guid: &'a [u8],
    total: usize,
    parts: Vec<(usize, &'a [u8])>,
}

impl<'a> Extensions<'a> {
    /// `part` — тело сегмента после подписи продолжения.
    pub(super) fn add(&mut self, part: &'a [u8]) {
        let (Some(guid), Some(total), Some(offset), Some(data)) = (
            part.get(..32),
            be_u32(part, 32),
            be_u32(part, 36),
            part.get(40..),
        ) else {
            return;
        };
        let (total, offset) = (total as usize, offset as usize);
        let at = match self.groups.iter().position(|group| group.guid == guid) {
            Some(at) => at,
            None => {
                self.groups.push(Group {
                    guid,
                    total,
                    parts: Vec::new(),
                });
                self.groups.len() - 1
            }
        };
        self.groups[at].parts.push((offset, data));
    }

    /// Собирает и разбирает каждое продолжение.
    ///
    /// `budget` — сколько всего байт можно отвести под сборку: не больше
    /// самого файла. Полную длину заявляет сам файл, и без предела он
    /// заказывал бы гигабайты одним полем.
    pub(super) fn scan_into(self, facts: &mut Facts, mut budget: usize) {
        for group in self.groups {
            // Кусок, вылезающий за заявленную длину, — битый, и места ему
            // в сборке нет.
            let end = |offset: usize, data: &[u8]| {
                offset
                    .checked_add(data.len())
                    .filter(|&end| end <= group.total)
            };
            let size = group
                .parts
                .iter()
                .filter_map(|&(offset, data)| end(offset, data))
                .max()
                .unwrap_or(0);
            if size == 0 || size > budget {
                continue;
            }
            budget -= size;
            let mut whole = vec![0u8; size];
            for &(offset, data) in &group.parts {
                if let Some(end) = end(offset, data) {
                    whole[offset..end].copy_from_slice(data);
                }
            }
            facts.scan(&whole);
        }
    }
}

/// Пакет XMP из расширения приложения GIF, если это оно.
///
/// XMP лежит в GIF **не подблоками**, как комментарий: байты пакета идут
/// подряд сразу за заголовком расширения, а за ними — «магический хвост»
/// из 258 байт (0x01, затем 0xFF, 0xFE … 0x00, затем 0x00). Хвост устроен
/// так, что читатель, принимающий байты пакета за длины подблоков, из
/// любого места пакета допрыгивает до его конца, — поэтому `gif_blocks`
/// и проходит такое расширение целиком. Но склеить его как подблоки
/// (`unblock`) нельзя: каждый «байт длины» — это буква пакета, и склейка
/// выбросила бы из него буквы через случайные промежутки.
///
/// `body` — тело расширения от байта длины заголовка (11) до конца.
pub(super) fn gif_packet(body: &[u8]) -> Option<&[u8]> {
    const TRAILER: usize = 258;
    if body.first() != Some(&11) || body.get(1..12) != Some(b"XMP DataXMP".as_slice()) {
        return None;
    }
    let raw = &body[12..];
    let magic = |tail: &[u8]| {
        tail[0] == 0x01
            && tail[1..257]
                .iter()
                .enumerate()
                .all(|(i, &b)| usize::from(b) == 255 - i)
            && tail[257] == 0
    };
    Some(match raw.len().checked_sub(TRAILER) {
        Some(cut) if magic(&raw[cut..]) => &raw[..cut],
        // Хвост испорчен — разбираем как есть: мусор после пакета разбору
        // не мешает, он ищет теги.
        _ => raw,
    })
}

/// Текст записи iTXt, если он не сжат.
///
/// Сжатый текст — это zlib, а распаковщика среди зависимостей Savio нет:
/// заводить его ради одного XMP в PNG — зависимость на то, что пишут редко
/// (ExifTool и Photoshop кладут XMP несжатым). Сжатый XMP остаётся строкой
/// «XMP» без разбора, и под таблицей без жёлтых строк так и сказано: «среди
/// прочитанного».
pub(super) fn itxt_plain(body: &[u8]) -> Option<&[u8]> {
    let zero = |bytes: &[u8]| bytes.iter().position(|&b| b == 0);
    let rest = &body[zero(body)? + 1..];
    let [compressed, _method, rest @ ..] = rest else {
        return None;
    };
    if *compressed != 0 {
        return None;
    }
    let rest = &rest[zero(rest)? + 1..]; // язык
    Some(&rest[zero(rest)? + 1..]) // переведённое ключевое слово
}
