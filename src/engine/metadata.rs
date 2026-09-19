//! Чтение и удаление метаданных локальных файлов.
//!
//! Про UI не знает ничего: наружу торчит тот же канал `Event`, что и у загрузки.
//!
//! # Почему не ffmpeg
//!
//! Для MP3 связка `ffmpeg -map_metadata -1 -c:a copy` работает и действительно
//! вычищает теги. **Для изображений она не работает вообще.** Проверено вживую
//! на JPEG, PNG и WebP: ffmpeg завершается с кодом 0, размер файла до байта
//! совпадает с исходным, а EXIF остаётся на месте. Причина в том, что для
//! одиночной картинки демультиплексор отдаёт весь файл одним пакетом, и `-c copy`
//! честно переписывает его дословно — вместе с секцией метаданных, которую
//! `-map_metadata -1` в этом режиме даже не видит. Ошибки нет ни при сборке, ни
//! в коде возврата: функция просто молча не работает (Правило 6).
//!
//! Для инструмента, который обещает удалить геометку из фотографии, молчаливый
//! отказ — худший из возможных исходов: пользователь считает файл очищенным и
//! отдаёт его дальше. Поэтому и чтение, и очистка изображений сделаны здесь
//! разбором самого контейнера, без внешних программ.
//!
//! # Почему это заведомо lossless
//!
//! Мы не трогаем сжатые данные: у JPEG переписываются только маркерные сегменты,
//! у PNG и WebP — только служебные чанки, у MP3 отрезаются блоки тегов в начале
//! и в конце. Сами пиксели и звуковые кадры копируются побайтово, поэтому
//! «ухудшиться» им негде — это свойство способа, а не аккуратности настроек.
//! Заодно операция получается мгновенной: перекодировать нечего.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::i18n::{self, Key, Lang};
use crate::model::{MetaKind, Tag, meta_kind};

/// Сколько байт значения показываем. XMP-пакет занимает килобайты, и целиком
/// он в списке не нужен — важно, что он есть, а не его содержимое.
const VALUE_LIMIT: usize = 200;

/// Ограничение на число записей в одном каталоге EXIF и на глубину вложенности.
/// Файл может быть битым или враждебным: каталог, ссылающийся сам на себя,
/// без такого предела увёл бы разбор в бесконечный цикл.
const IFD_ENTRY_LIMIT: usize = 512;

// ---------------------------------------------------------------------------
// Чтение
// ---------------------------------------------------------------------------

/// Читает метаданные файла.
///
/// `ffprobe` нужен только для MP3: у него мы спрашиваем битрейт и длительность,
/// которых в самих тегах нет. Без него теги всё равно прочитаются — разбором
/// ID3 напрямую, просто без технической справки.
pub fn read(path: &Path, ffprobe: Option<&Path>, lang: Lang) -> Result<Vec<Tag>, String> {
    let kind = meta_kind(path);
    if !kind.readable() {
        return Err(unsupported_message(kind, lang));
    }

    match kind {
        MetaKind::Mp3 => read_mp3(path, ffprobe, lang),
        _ => {
            let data = read_file(path, lang)?;
            match kind {
                MetaKind::Jpeg => read_jpeg(&data, lang),
                MetaKind::Png => read_png(&data, lang),
                MetaKind::WebP => read_webp(&data, lang),
                MetaKind::Gif => read_gif(&data, lang),
                MetaKind::Tiff => Ok(read_tiff(&data, lang)),
                MetaKind::Mp3 | MetaKind::Video | MetaKind::Unsupported => unreachable!(),
            }
        }
    }
}

pub fn unsupported_message(kind: MetaKind, lang: Lang) -> String {
    i18n::t(
        lang,
        match kind {
            MetaKind::Video => Key::MetaVideoUnsupported,
            MetaKind::Tiff => Key::MetaTiffReadOnly,
            _ => Key::MetaFormatUnsupported,
        },
    )
    .to_owned()
}

/// Ошибка от системы, пересказанная человеку: `{}` — её собственный текст.
fn trouble(key: Key, lang: Lang, err: impl std::fmt::Display) -> String {
    i18n::fill(i18n::t(lang, key), &[&err.to_string()])
}

fn read_file(path: &Path, lang: Lang) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| trouble(Key::MetaReadFileFailed, lang, e))
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

/// Разобранный маркерный сегмент JPEG.
struct Segment {
    marker: u8,
    /// Границы полезной нагрузки сегмента, без маркера и поля длины.
    body: std::ops::Range<usize>,
    /// Границы сегмента целиком — то, что копируется при очистке.
    whole: std::ops::Range<usize>,
}

/// Обходит маркерные сегменты JPEG до начала сжатых данных.
///
/// Возвращает список сегментов и смещение, с которого начинается `SOS`
/// (сжатое изображение). Всё от `SOS` и до конца файла при очистке копируется
/// дословно: там лежат сами пиксели, и разбирать их нам незачем.
fn jpeg_segments(data: &[u8], lang: Lang) -> Result<(Vec<Segment>, usize), String> {
    let broken = |key| i18n::t(lang, key).to_owned();
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return Err(broken(Key::MetaNotJpeg));
    }

    let mut segments = Vec::new();
    let mut pos = 2;

    loop {
        // Между сегментами допустимы байты-заполнители 0xFF.
        while pos < data.len() && data[pos] == 0xFF && data.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        if pos + 1 >= data.len() || data[pos] != 0xFF {
            // Сегменты кончились, а SOS не встретился: файл обрезан.
            return Ok((segments, data.len().min(pos)));
        }

        let marker = data[pos + 1];

        // Маркеры без тела: они не несут длины, и читать её нельзя.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            pos += 2;
            continue;
        }

        // SOS — дальше идут сжатые данные до конца файла.
        if marker == 0xDA {
            return Ok((segments, pos));
        }

        let len =
            be_u16(data, pos + 2).ok_or_else(|| broken(Key::MetaJpegLengthBroken))? as usize;
        if len < 2 {
            return Err(broken(Key::MetaJpegZeroSegment));
        }
        let body = pos + 4;
        let end = pos + 2 + len;
        if end > data.len() {
            return Err(broken(Key::MetaJpegSegmentOverrun));
        }

        segments.push(Segment {
            marker,
            body: body..end,
            whole: pos..end,
        });
        pos = end;
    }
}

/// Несёт ли сегмент метаданные, то есть подлежит ли удалению.
///
/// Удаляем: APP1 (EXIF и XMP), APP3…APP13 (в том числе IPTC/Photoshop в APP13)
/// и APP15, а также COM — текстовый комментарий.
///
/// **Оставляем намеренно** три сегмента, которые формально тоже «служебные»,
/// но влияют на то, как изображение выглядит:
/// APP0 (JFIF — плотность и миниатюра), APP2 (профиль ICC) и APP14 (Adobe —
/// признак цветового преобразования, без него CMYK-файл декодируется с
/// перевёрнутыми цветами). Личных данных в них нет, а их удаление изменило бы
/// картинку — ровно то, чего требование lossless и запрещает.
fn jpeg_is_metadata(marker: u8) -> bool {
    marker == 0xE1 || (0xE3..=0xED).contains(&marker) || marker == 0xEF || marker == 0xFE
}

fn read_jpeg(data: &[u8], lang: Lang) -> Result<Vec<Tag>, String> {
    let (segments, _) = jpeg_segments(data, lang)?;
    let mut tags = Vec::new();

    for seg in &segments {
        let body = &data[seg.body.clone()];
        match seg.marker {
            0xE1 => {
                if let Some(exif) = body.strip_prefix(b"Exif\x00\x00") {
                    tags.extend(read_tiff(exif, lang));
                } else if body.starts_with(b"http://ns.adobe.com/xap/1.0/\x00") {
                    tags.push(Tag::new("XMP", present_bytes(body.len(), lang)));
                }
            }
            // «IPTC / Photoshop» — названия форматов, а не слова: переводу
            // они не подлежат.
            0xED => tags.push(Tag::new(
                "IPTC / Photoshop",
                present_bytes(body.len(), lang),
            )),
            0xFE => tags.push(Tag::new(
                i18n::t(lang, Key::TagComment),
                text_value(body),
            )),
            _ => {}
        }
    }

    Ok(tags)
}

/// «присутствует, 4096 Б» — общее для XMP, IPTC и прочих блоков, которые
/// показываются фактом наличия, а не содержимым.
fn present_bytes(len: usize, lang: Lang) -> String {
    i18n::fill(i18n::t(lang, Key::MetaPresentBytes), &[&len.to_string()])
}

fn strip_jpeg(data: &[u8], lang: Lang) -> Result<Vec<u8>, String> {
    let (segments, sos) = jpeg_segments(data, lang)?;

    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]); // SOI

    for seg in &segments {
        if jpeg_is_metadata(seg.marker) {
            continue;
        }
        out.extend_from_slice(&data[seg.whole.clone()]);
    }

    // Всё от SOS и до конца — сжатое изображение, копируется дословно.
    out.extend_from_slice(&data[sos..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Чанки, которые обязаны пережить очистку.
///
/// Список — «что оставить», а не «что удалить», и это осознанно: неизвестный
/// чанк для инструмента приватности безопаснее выбросить, чем сохранить.
/// Цена ошибки несимметрична — уцелевшая геометка хуже потерянного украшения.
///
/// Сюда входят критические чанки (`IHDR`, `PLTE`, `IDAT`, `IEND`), всё, что
/// влияет на отрисовку (прозрачность, гамма, цветовой профиль, разрешение),
/// и чанки анимации APNG: без них анимированный PNG превратился бы в один кадр.
const PNG_KEEP: [&[u8; 4]; 14] = [
    b"IHDR", b"PLTE", b"IDAT", b"IEND", // структура
    b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"bKGD",
    b"pHYs", // отрисовка
    b"acTL",
    b"fcTL", // APNG (fdAT обрабатывается отдельно — см. ниже)
];

/// Обходит чанки PNG, отдавая каждому наблюдателю тип и границы.
fn png_chunks(
    data: &[u8],
    lang: Lang,
    mut visit: impl FnMut(&[u8; 4], &[u8], std::ops::Range<usize>),
) -> Result<(), String> {
    let broken = |key| i18n::t(lang, key).to_owned();
    if data.len() < 8 || data[..8] != PNG_SIGNATURE {
        return Err(broken(Key::MetaNotPng));
    }

    let mut pos = 8;
    while pos + 8 <= data.len() {
        let len = be_u32(data, pos).ok_or_else(|| broken(Key::MetaPngLengthBroken))? as usize;
        let kind: [u8; 4] = data[pos + 4..pos + 8]
            .try_into()
            .map_err(|_| broken(Key::MetaPngTypeBroken))?;
        let body = pos + 8;
        // 4 байта контрольной суммы после тела.
        let end = body
            .checked_add(len)
            .and_then(|e| e.checked_add(4))
            .ok_or_else(|| broken(Key::MetaPngImplausibleLength))?;
        if end > data.len() {
            return Err(broken(Key::MetaPngChunkOverrun));
        }

        visit(&kind, &data[body..body + len], pos..end);

        if &kind == b"IEND" {
            break;
        }
        pos = end;
    }
    Ok(())
}

fn read_png(data: &[u8], lang: Lang) -> Result<Vec<Tag>, String> {
    let mut tags = Vec::new();
    png_chunks(data, lang, |kind, body, _| match kind {
        // tEXt: ключ, ноль, значение — обе части в Latin-1.
        b"tEXt" => {
            let mut parts = body.splitn(2, |b| *b == 0);
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            tags.push(Tag::new(text_value(key), text_value(value)));
        }
        // zTXt и iTXt держат значение сжатым или в UTF-8 со служебными полями.
        // Распаковывать ради показа незачем — важно, что запись есть.
        b"zTXt" => {
            let key = body.split(|b| *b == 0).next().unwrap_or_default();
            tags.push(Tag::new(
                text_value(key),
                i18n::t(lang, Key::MetaTextCompressed),
            ));
        }
        b"iTXt" => {
            let key = body.split(|b| *b == 0).next().unwrap_or_default();
            tags.push(Tag::new(text_value(key), i18n::t(lang, Key::MetaTextUtf8)));
        }
        b"eXIf" => tags.extend(read_tiff(body, lang)),
        b"tIME" if body.len() >= 7 => {
            let y = be_u16(body, 0).unwrap_or(0);
            tags.push(Tag::new(
                i18n::t(lang, Key::TagModified),
                format!(
                    "{y:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    body[2], body[3], body[4], body[5], body[6]
                ),
            ));
        }
        _ => {}
    })?;
    Ok(tags)
}

fn strip_png(data: &[u8], lang: Lang) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&PNG_SIGNATURE);

    png_chunks(data, lang, |kind, _, whole| {
        // fdAT — кадры APNG. В списке их нет отдельной строкой только потому,
        // что проверять префикс дешевле, чем держать оба варианта.
        if PNG_KEEP.contains(&kind) || kind == b"fdAT" {
            out.extend_from_slice(&data[whole]);
        }
    })?;

    Ok(out)
}

// ---------------------------------------------------------------------------
// WebP (RIFF)
// ---------------------------------------------------------------------------

fn read_webp(data: &[u8], lang: Lang) -> Result<Vec<Tag>, String> {
    let mut tags = Vec::new();
    riff_chunks(data, lang, |kind, body| {
        match kind {
            b"EXIF" => tags.extend(read_tiff(body, lang)),
            b"XMP " => tags.push(Tag::new("XMP", present_bytes(body.len(), lang))),
            _ => {}
        }
        true
    })?;
    Ok(tags)
}

fn strip_webp(data: &[u8], lang: Lang) -> Result<Vec<u8>, String> {
    let mut body = Vec::with_capacity(data.len());

    riff_chunks(data, lang, |kind, chunk| {
        if kind == b"EXIF" || kind == b"XMP " {
            return true;
        }

        body.extend_from_slice(kind);
        body.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        let start = body.len();
        body.extend_from_slice(chunk);
        // Чанки RIFF выравниваются по чётной границе.
        if chunk.len() % 2 == 1 {
            body.push(0);
        }

        // VP8X объявляет флагами, что в файле есть EXIF и XMP. Вырезать чанки
        // и оставить флаги нельзя: декодер пойдёт искать то, чего больше нет,
        // и часть просмотрщиков сочтёт файл битым. Ошибки при этом не будет
        // ни у нас, ни у ffmpeg — картинка просто перестанет открываться там,
        // куда её отправят.
        if kind == b"VP8X" && !chunk.is_empty() {
            body[start] &= !0b0000_1100; // сбрасываем биты EXIF и XMP
        }
        true
    })?;

    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"RIFF");
    // Размер RIFF считает от поля формата, то есть включает "WEBP".
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Ok(out)
}

fn riff_chunks(
    data: &[u8],
    lang: Lang,
    mut visit: impl FnMut(&[u8; 4], &[u8]) -> bool,
) -> Result<(), String> {
    let broken = |key| i18n::t(lang, key).to_owned();
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err(broken(Key::MetaNotWebp));
    }

    let mut pos = 12;
    while pos + 8 <= data.len() {
        let kind: [u8; 4] = data[pos..pos + 4]
            .try_into()
            .map_err(|_| broken(Key::MetaWebpTypeBroken))?;
        let len =
            le_u32(data, pos + 4).ok_or_else(|| broken(Key::MetaWebpLengthBroken))? as usize;
        let body = pos + 8;
        let end = body
            .checked_add(len)
            .ok_or_else(|| broken(Key::MetaWebpImplausibleLength))?;
        if end > data.len() {
            return Err(broken(Key::MetaWebpChunkOverrun));
        }

        if !visit(&kind, &data[body..end]) {
            break;
        }
        pos = end + (len % 2); // выравнивание по чётной границе
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GIF
// ---------------------------------------------------------------------------

/// Обходит блоки GIF после заголовка и таблицы цветов.
///
/// `visit` получает метку расширения (или `None` для кадра) и границы блока.
fn gif_blocks(
    data: &[u8],
    lang: Lang,
    mut visit: impl FnMut(Option<u8>, &[u8], std::ops::Range<usize>),
) -> Result<(), String> {
    let broken = |key| i18n::t(lang, key).to_owned();
    if data.len() < 13 || (&data[..6] != b"GIF87a" && &data[..6] != b"GIF89a") {
        return Err(broken(Key::MetaNotGif));
    }

    let mut pos = 13;
    // Глобальная таблица цветов, если объявлена флагом в дескрипторе экрана.
    if data[10] & 0x80 != 0 {
        pos += 3 * (1 << ((data[10] & 0x07) + 1));
    }

    while pos < data.len() {
        match data[pos] {
            0x3B => break, // конец файла
            0x21 => {
                // Расширение: метка, затем цепочка подблоков.
                let label = *data
                    .get(pos + 1)
                    .ok_or_else(|| broken(Key::MetaGifExtensionBroken))?;
                let start = pos;
                let mut p = pos + 2;
                let body_start = p;
                while let Some(&size) = data.get(p) {
                    p += 1 + size as usize;
                    if size == 0 {
                        break;
                    }
                }
                if p > data.len() {
                    return Err(broken(Key::MetaGifExtensionOverrun));
                }
                visit(Some(label), &data[body_start.min(p)..p], start..p);
                pos = p;
            }
            0x2C => {
                // Дескриптор кадра: 10 байт, затем таблица цветов и данные.
                let start = pos;
                let flags = *data
                    .get(pos + 9)
                    .ok_or_else(|| broken(Key::MetaGifFrameBroken))?;
                let mut p = pos + 10;
                if flags & 0x80 != 0 {
                    p += 3 * (1 << ((flags & 0x07) + 1));
                }
                p += 1; // минимальный размер кода LZW
                while let Some(&size) = data.get(p) {
                    p += 1 + size as usize;
                    if size == 0 {
                        break;
                    }
                }
                if p > data.len() {
                    return Err(broken(Key::MetaGifFrameOverrun));
                }
                visit(None, &[], start..p);
                pos = p;
            }
            _ => return Err(broken(Key::MetaGifUnknownBlock)),
        }
    }
    Ok(())
}

fn read_gif(data: &[u8], lang: Lang) -> Result<Vec<Tag>, String> {
    let mut tags = Vec::new();
    gif_blocks(data, lang, |label, body, _| match label {
        // Подблоки идут с байтом длины перед каждым — для показа его убираем.
        Some(0xFE) => tags.push(Tag::new(
            i18n::t(lang, Key::TagComment),
            text_value(&unblock(body)),
        )),
        Some(0xFF) => tags.push(Tag::new(
            i18n::t(lang, Key::TagAppExtension),
            text_value(body.get(1..12).unwrap_or_default()),
        )),
        Some(0x01) => tags.push(Tag::new(
            i18n::t(lang, Key::TagTextBlock),
            i18n::t(lang, Key::WordPresent),
        )),
        _ => {}
    })?;
    Ok(tags)
}

fn strip_gif(data: &[u8], lang: Lang) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len());
    // Заголовок и глобальная таблица цветов — до первого блока.
    let mut header_end = 13;
    if data.len() > 10 && data[10] & 0x80 != 0 {
        header_end += 3 * (1 << ((data[10] & 0x07) + 1));
    }
    if header_end > data.len() {
        return Err(i18n::t(lang, Key::MetaGifPaletteBroken).to_owned());
    }
    out.extend_from_slice(&data[..header_end]);

    gif_blocks(data, lang, |label, _, whole| {
        // Выбрасываем комментарий, текстовый блок и расширения приложения
        // (в них живёт XMP). Управляющее расширение 0xF9 оставляем: в нём
        // задержка кадра и прозрачность — без него анимация встанет.
        let drop = matches!(label, Some(0xFE) | Some(0xFF) | Some(0x01));
        if !drop {
            out.extend_from_slice(&data[whole]);
        }
    })?;

    out.push(0x3B);
    Ok(out)
}

/// Склеивает цепочку подблоков GIF в сплошные данные.
fn unblock(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut p = 0;
    while let Some(&size) = body.get(p) {
        if size == 0 {
            break;
        }
        let end = (p + 1 + size as usize).min(body.len());
        out.extend_from_slice(&body[p + 1..end]);
        p = end;
    }
    out
}

// ---------------------------------------------------------------------------
// EXIF (TIFF)
// ---------------------------------------------------------------------------

/// Читает блок EXIF в формате TIFF: заголовок, затем цепочка каталогов.
///
/// Ошибки не возвращает намеренно: испорченный EXIF — не повод отказать в
/// показе остального. Что разобралось, то и покажем.
fn read_tiff(data: &[u8], lang: Lang) -> Vec<Tag> {
    let Some(be) = tiff_endian(data) else {
        return Vec::new();
    };
    let Some(ifd0) = read_u32(data, 4, be) else {
        return Vec::new();
    };

    let mut tags = Vec::new();
    let mut seen = Vec::new();
    read_ifd(data, ifd0 as usize, be, &mut tags, &mut seen, 0, lang);
    tags
}

fn tiff_endian(data: &[u8]) -> Option<bool> {
    if data.len() < 8 {
        return None;
    }
    let be = match &data[..2] {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    // Магическое число 42 подтверждает, что порядок байт определён верно.
    (read_u16(data, 2, be)? == 42).then_some(be)
}

fn read_ifd(
    data: &[u8],
    offset: usize,
    be: bool,
    tags: &mut Vec<Tag>,
    seen: &mut Vec<usize>,
    depth: usize,
    lang: Lang,
) {
    // Каталог, ссылающийся на уже пройденный, — верный признак битого файла.
    // Без этой проверки разбор ушёл бы в бесконечный цикл.
    if depth > 4 || seen.contains(&offset) || offset + 2 > data.len() {
        return;
    }
    seen.push(offset);

    let Some(count) = read_u16(data, offset, be) else {
        return;
    };
    let count = (count as usize).min(IFD_ENTRY_LIMIT);

    for i in 0..count {
        let entry = offset + 2 + i * 12;
        if entry + 12 > data.len() {
            return;
        }
        let (Some(tag), Some(format), Some(components)) = (
            read_u16(data, entry, be),
            read_u16(data, entry + 2, be),
            read_u32(data, entry + 4, be),
        ) else {
            return;
        };

        // Вложенные каталоги: собственно EXIF и GPS.
        if tag == 0x8769 || tag == 0x8825 {
            if let Some(sub) = read_u32(data, entry + 8, be) {
                read_ifd(data, sub as usize, be, tags, seen, depth + 1, lang);
            }
            continue;
        }

        let Some(name) = tag_name(tag, depth > 0, lang) else {
            continue;
        };
        if let Some(value) = tiff_value(data, entry + 8, format, components as usize, be, lang) {
            tags.push(Tag::new(name, value));
        }
    }

    // Следующий каталог в цепочке — там обычно лежит миниатюра.
    let next = offset + 2 + count * 12;
    if let Some(link) = read_u32(data, next, be)
        && link != 0
    {
        read_ifd(data, link as usize, be, tags, seen, depth + 1, lang);
    }
}

/// Размер одного элемента для каждого типа TIFF.
fn tiff_unit(format: u16) -> usize {
    match format {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 11 => 4,    // LONG, SLONG, FLOAT
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        _ => 0,
    }
}

fn tiff_value(
    data: &[u8],
    value_at: usize,
    format: u16,
    components: usize,
    be: bool,
    lang: Lang,
) -> Option<String> {
    let unit = tiff_unit(format);
    if unit == 0 || components == 0 || components > 1_000_000 {
        return None;
    }
    let total = unit.checked_mul(components)?;

    // Значение до четырёх байт лежит прямо в записи, длиннее — по смещению.
    let start = if total <= 4 {
        value_at
    } else {
        read_u32(data, value_at, be)? as usize
    };
    let bytes = data.get(start..start.checked_add(total)?)?;

    Some(match format {
        2 => text_value(bytes),
        5 | 10 => {
            // Рациональное: числитель и знаменатель.
            let parts: Vec<String> = (0..components)
                .filter_map(|i| {
                    let n = read_u32(bytes, i * 8, be)?;
                    let d = read_u32(bytes, i * 8 + 4, be)?;
                    Some(if d == 0 {
                        "0".to_owned()
                    } else if n % d == 0 {
                        (n / d).to_string()
                    } else {
                        format!("{:.4}", n as f64 / d as f64)
                    })
                })
                .collect();
            parts.join(", ")
        }
        3 | 8 => (0..components)
            .filter_map(|i| read_u16(bytes, i * 2, be).map(|v| v.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        4 | 9 => (0..components)
            .filter_map(|i| read_u32(bytes, i * 4, be).map(|v| v.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        _ => i18n::fill(i18n::t(lang, Key::MetaRawBytes), &[&total.to_string()]),
    })
}

/// Человекочитаемые имена тегов.
///
/// Список намеренно неполный: показываем то, ради чего инструмент и открывают, —
/// кто снимал, чем, когда и где. Полный справочник EXIF насчитывает сотни
/// записей, и вываливать их пользователю смысла нет.
fn tag_name(tag: u16, nested: bool, lang: Lang) -> Option<&'static str> {
    // Номера тегов GPS пересекаются с номерами основного каталога, поэтому
    // вложенные каталоги разбираем по отдельной таблице.
    if nested
        && let Some(key) = match tag {
            0x0001 => Some(Key::TagGpsLatitudeRef),
            0x0002 => Some(Key::TagGpsLatitude),
            0x0003 => Some(Key::TagGpsLongitudeRef),
            0x0004 => Some(Key::TagGpsLongitude),
            0x0006 => Some(Key::TagGpsAltitude),
            0x0007 => Some(Key::TagGpsTime),
            0x001D => Some(Key::TagGpsDate),
            _ => None,
        }
    {
        return Some(i18n::t(lang, key));
    }

    // «ISO» — обозначение из стандарта, а не слово: переводу не подлежит.
    if tag == 0x8827 {
        return Some("ISO");
    }

    let key = match tag {
        0x010E => Key::TagDescription,
        0x010F => Key::TagMaker,
        0x0110 => Key::TagCameraModel,
        0x0112 => Key::TagOrientation,
        0x0131 => Key::TagSoftware,
        0x0132 => Key::TagDateModified,
        0x013B => Key::TagAuthor,
        0x8298 => Key::TagCopyright,
        0x829A => Key::TagExposure,
        0x829D => Key::TagAperture,
        0x9003 => Key::TagDateTaken,
        0x9004 => Key::TagDateDigitized,
        0x920A => Key::TagFocalLength,
        0xA002 => Key::TagWidth,
        0xA003 => Key::TagHeight,
        0xA430 => Key::TagCameraOwner,
        0xA433 => Key::TagLensMaker,
        0xA434 => Key::TagLensModel,
        0xA435 => Key::TagLensSerial,
        0xC62F => Key::TagCameraSerial,
        _ => return None,
    };
    Some(i18n::t(lang, key))
}

// ---------------------------------------------------------------------------
// MP3
// ---------------------------------------------------------------------------

/// Границы полезных данных MP3: всё, что вне их, — теги.
struct Mp3Bounds {
    start: usize,
    end: usize,
}

/// Находит блоки тегов в начале и в конце файла.
///
/// Теги MP3 не перемешаны со звуком: ID3v2 лежит сплошным блоком в начале,
/// ID3v1 и APEv2 — в конце. Поэтому «удалить теги» здесь означает буквально
/// скопировать середину файла, не заглядывая в звуковые кадры.
fn mp3_bounds(head: &[u8], tail: &[u8], size: usize) -> Mp3Bounds {
    let mut start = 0;

    // ID3v2 в начале: "ID3", версия, флаги, затем размер в synchsafe-виде —
    // по 7 значащих бит в байте, старший всегда ноль.
    if head.len() >= 10 && &head[..3] == b"ID3" {
        let flags = head[5];
        let size_bytes = &head[6..10];
        if size_bytes.iter().all(|b| b & 0x80 == 0) {
            let tag_size = size_bytes
                .iter()
                .fold(0usize, |acc, b| (acc << 7) | (*b as usize & 0x7F));
            start = 10 + tag_size;
            // Флаг 0x10 — наличие копии заголовка в конце тега.
            if flags & 0x10 != 0 {
                start += 10;
            }
        }
    }

    // В конце теги могут стоять друг за другом: ID3v1 после APEv2 — обычное дело.
    let mut end = size;
    loop {
        let cut = end;

        // ID3v1: ровно 128 байт, начинается с "TAG".
        if end >= 128 && tail_at(tail, size, end - 128, 3) == Some(b"TAG".as_slice()) {
            end -= 128;
            // Расширенный блок ID3v1 на 227 байт стоит прямо перед ним.
            if end >= 227 && tail_at(tail, size, end - 227, 4) == Some(b"TAG+".as_slice()) {
                end -= 227;
            }
        }

        // APEv2: 32-байтный завершитель "APETAGEX".
        if end >= 32
            && tail_at(tail, size, end - 32, 8) == Some(b"APETAGEX".as_slice())
            && let Some(footer) = tail_at(tail, size, end - 32, 32)
        {
            let tag_size = le_u32(footer, 12).unwrap_or(0) as usize;
            let flags = le_u32(footer, 20).unwrap_or(0);
            // Размер в завершителе не включает заголовок — он учитывается флагом.
            let total = tag_size + if flags & 0x8000_0000 != 0 { 32 } else { 0 };
            if total <= end {
                end -= total;
            }
        }

        if end == cut {
            break;
        }
    }

    Mp3Bounds {
        start: start.min(size),
        end: end.max(start.min(size)),
    }
}

/// Достаёт кусок файла из «хвостового» буфера, пересчитывая абсолютное
/// смещение в смещение внутри буфера.
fn tail_at(tail: &[u8], size: usize, at: usize, len: usize) -> Option<&[u8]> {
    let tail_start = size.checked_sub(tail.len())?;
    let local = at.checked_sub(tail_start)?;
    tail.get(local..local + len)
}

/// Сколько байт с конца файла нужно, чтобы увидеть все возможные теги.
/// APEv2 бывает большим, но его завершитель всегда в последних 32 байтах,
/// а размер мы читаем оттуда же — так что хватает небольшого окна.
const MP3_TAIL_WINDOW: u64 = 512;

fn mp3_read_edges(path: &Path, lang: Lang) -> Result<(Vec<u8>, Vec<u8>, u64), String> {
    let mut file = File::open(path).map_err(|e| trouble(Key::MetaOpenFileFailed, lang, e))?;
    let size = file
        .metadata()
        .map_err(|e| trouble(Key::MetaReadSizeFailed, lang, e))?
        .len();

    let mut head = vec![0u8; 10.min(size as usize)];
    file.read_exact(&mut head)
        .map_err(|e| trouble(Key::MetaReadHeadFailed, lang, e))?;

    let tail_len = MP3_TAIL_WINDOW.min(size);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(size - tail_len))
        .map_err(|e| trouble(Key::MetaSeekEndFailed, lang, e))?;
    file.read_exact(&mut tail)
        .map_err(|e| trouble(Key::MetaReadTailFailed, lang, e))?;

    Ok((head, tail, size))
}

/// Читает теги MP3.
///
/// Сами теги разбирает `ffprobe`: он уже поставляется с Savio, знает все версии
/// ID3 и заодно отдаёт битрейт с длительностью, которых в тегах нет. Если его
/// нет на месте, честно говорим об этом — но факт наличия тегов всё равно
/// показываем, он виден по одним только границам блоков.
fn read_mp3(path: &Path, ffprobe: Option<&Path>, lang: Lang) -> Result<Vec<Tag>, String> {
    let mut tags = Vec::new();

    if let Some(ffprobe) = ffprobe {
        tags.extend(ffprobe_tags(path, ffprobe, lang)?);
    }

    // Обложка в ID3v2 занимает основную часть тега и в списке ffprobe
    // отдельной строкой не видна — показываем её по размеру блока.
    let (head, tail, size) = mp3_read_edges(path, lang)?;
    let bounds = mp3_bounds(&head, &tail, size as usize);
    let tag_bytes = bounds.start as u64 + (size - bounds.end as u64);
    if tag_bytes > 0 {
        tags.push(Tag::new(
            i18n::t(lang, Key::MetaTagBytes),
            i18n::fill(
                i18n::t(lang, Key::MetaTagBytesValue),
                &[&tag_bytes.to_string()],
            ),
        ));
    }

    if ffprobe.is_none() && tags.is_empty() && tag_bytes > 0 {
        tags.push(Tag::new(
            i18n::t(lang, Key::TagTags),
            i18n::t(lang, Key::MetaTagsUnreadable),
        ));
    }

    Ok(tags)
}

/// Спрашивает у ffprobe теги, битрейт и длительность.
fn ffprobe_tags(path: &Path, ffprobe: &Path, lang: Lang) -> Result<Vec<Tag>, String> {
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(path)
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .stdin(Stdio::null());
    crate::engine::ytdlp::hide_console(&mut cmd);

    let output = cmd
        .output()
        .map_err(|e| trouble(Key::MetaFfprobeLaunchFailed, lang, e))?;
    if !output.status.success() {
        return Err(i18n::t(lang, Key::MetaFfprobeFailed).to_owned());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| i18n::t(lang, Key::MetaFfprobeGarbage).to_owned())?;

    let mut tags = Vec::new();

    if let Some(format) = json.get("format") {
        if let Some(duration) = format.get("duration").and_then(|v| v.as_str())
            && let Ok(secs) = duration.parse::<f64>()
        {
            tags.push(Tag::new(
                i18n::t(lang, Key::TagDuration),
                crate::model::human_duration(secs as u64),
            ));
        }
        if let Some(rate) = format.get("bit_rate").and_then(|v| v.as_str())
            && let Ok(bps) = rate.parse::<u64>()
        {
            tags.push(Tag::new(
                i18n::t(lang, Key::TagBitrate),
                i18n::fill(
                    i18n::t(lang, Key::MetaKbpsValue),
                    &[&(bps / 1000).to_string()],
                ),
            ));
        }
        if let Some(map) = format.get("tags").and_then(|v| v.as_object()) {
            for (key, value) in map {
                let text = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                tags.push(Tag::new(id3_name(key, lang), truncate(&text)));
            }
        }
    }

    // Обложка приходит отдельным видеопотоком, а не тегом.
    if let Some(streams) = json.get("streams").and_then(|v| v.as_array())
        && streams
            .iter()
            .any(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some("video"))
    {
        tags.push(Tag::new(
            i18n::t(lang, Key::TagCover),
            i18n::t(lang, Key::MetaCoverEmbedded),
        ));
    }

    Ok(tags)
}

/// Переводит имена тегов ID3. Незнакомые оставляем как есть: произвольные
/// пользовательские поля тоже надо показать, а перевода у них нет.
fn id3_name(key: &str, lang: Lang) -> String {
    let key = match key.to_ascii_lowercase().as_str() {
        "title" => Key::TagTitle,
        "artist" => Key::TagArtist,
        "album" => Key::TagAlbum,
        "album_artist" => Key::TagAlbumArtist,
        "date" => Key::TagYear,
        "track" => Key::TagTrack,
        "genre" => Key::TagGenre,
        "comment" => Key::TagComment,
        "composer" => Key::TagComposer,
        "encoder" => Key::TagEncoder,
        "copyright" => Key::TagCopyright,
        "publisher" => Key::TagPublisher,
        "language" => Key::TagLanguage,
        "lyrics" => Key::TagLyrics,
        other => return other.to_owned(),
    };
    i18n::t(lang, key).to_owned()
}

// ---------------------------------------------------------------------------
// Удаление
// ---------------------------------------------------------------------------

/// Удаляет все метаданные, перезаписывая исходный файл.
///
/// Возвращает, сколько байт освободилось. Ноль означает, что чистить было
/// нечего — файл при этом не переписывается вовсе.
pub fn strip(path: &Path, lang: Lang) -> Result<u64, String> {
    let kind = meta_kind(path);
    if !kind.cleanable() {
        return Err(unsupported_message(kind, lang));
    }

    let before = file_size(path, lang)?;

    // MP3 обрабатываем потоком: он бывает в сотню мегабайт, а всё, что нужно
    // сделать, — скопировать середину файла. Держать её целиком в памяти
    // незачем. Изображения читаем в буфер: разбор контейнера требует
    // произвольного доступа, а снимок на пару десятков мегабайт — разовый
    // буфер в рабочем потоке, а не накопитель.
    if kind == MetaKind::Mp3 {
        let (head, tail, size) = mp3_read_edges(path, lang)?;
        let bounds = mp3_bounds(&head, &tail, size as usize);
        if bounds.start == 0 && bounds.end as u64 == size {
            return Ok(0);
        }
        let written =
            replace_atomically(path, lang, |out| copy_range(path, &bounds, out, lang))?;
        return Ok(before.saturating_sub(written));
    }

    let data = read_file(path, lang)?;
    let cleaned = match kind {
        MetaKind::Jpeg => strip_jpeg(&data, lang)?,
        MetaKind::Png => strip_png(&data, lang)?,
        MetaKind::WebP => strip_webp(&data, lang)?,
        MetaKind::Gif => strip_gif(&data, lang)?,
        MetaKind::Mp3 | MetaKind::Tiff | MetaKind::Video | MetaKind::Unsupported => unreachable!(),
    };

    if cleaned.len() as u64 == before {
        return Ok(0);
    }
    if cleaned.is_empty() {
        return Err(i18n::t(lang, Key::MetaEmptyResultKeepsOriginal).to_owned());
    }

    let written = replace_atomically(path, lang, |out| {
        out.write_all(&cleaned)
            .map_err(|e| trouble(Key::MetaWriteFileFailed, lang, e))?;
        Ok(cleaned.len() as u64)
    })?;
    Ok(before.saturating_sub(written))
}

fn file_size(path: &Path, lang: Lang) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| trouble(Key::MetaReadSizeFailed, lang, e))
}

fn copy_range(
    path: &Path,
    bounds: &Mp3Bounds,
    out: &mut impl Write,
    lang: Lang,
) -> Result<u64, String> {
    let file = File::open(path).map_err(|e| trouble(Key::MetaOpenFileFailed, lang, e))?;
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(bounds.start as u64))
        .map_err(|e| trouble(Key::MetaSeekAudioFailed, lang, e))?;

    let length = (bounds.end - bounds.start) as u64;
    std::io::copy(&mut reader.take(length), out)
        .map_err(|e| trouble(Key::MetaCopyAudioFailed, lang, e))
}

/// Записывает результат во временный файл рядом с исходным и подменяет его.
///
/// Временный файл кладётся **в тот же каталог**, а не в системный temp:
/// переименование атомарно только в пределах одного тома, а через границу
/// диска оно превращается в копирование — то есть ровно в ту небезопасную
/// перезапись, которой мы избегаем.
///
/// Перед подменой данные сбрасываются на диск и проверяется, что файл не пуст.
/// При любой ошибке временный файл удаляется, а исходный остаётся нетронутым.
fn replace_atomically(
    path: &Path,
    lang: Lang,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<u64, String>,
) -> Result<u64, String> {
    let tmp = temp_path(path);

    let result = (|| {
        let file =
            File::create(&tmp).map_err(|e| trouble(Key::MetaTempCreateFailed, lang, e))?;
        let mut writer = BufWriter::new(file);
        let written = write(&mut writer)?;

        if written == 0 {
            return Err(i18n::t(lang, Key::MetaEmptyResult).to_owned());
        }

        let file = writer
            .into_inner()
            .map_err(|e| trouble(Key::MetaTempFlushFailed, lang, e))?;
        // Без sync_all содержимое может остаться в кеше ОС: при отключении
        // питания сразу после переименования на диске оказался бы пустой файл
        // на месте исходного.
        file.sync_all()
            .map_err(|e| trouble(Key::MetaTempSyncFailed, lang, e))?;
        drop(file);

        // Проверяем то, что реально легло на диск, а не то, что мы намеревались
        // записать: «столько-то байт отправлено в буфер» доказательством не
        // является (Правило 6).
        let actual = file_size(&tmp, lang)?;
        if actual == 0 {
            return Err(i18n::t(lang, Key::MetaTempEmpty).to_owned());
        }

        // rename в пределах тома атомарен и на Windows тоже заменяет
        // существующий файл — отдельного удаления не требуется.
        std::fs::rename(&tmp, path)
            .map_err(|e| trouble(Key::MetaReplaceFailed, lang, e))?;
        Ok(actual)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".savio-tmp");
    path.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Мелкие помощники
// ---------------------------------------------------------------------------

fn be_u16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn le_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn read_u16(data: &[u8], at: usize, be: bool) -> Option<u16> {
    let raw: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
    Some(if be {
        u16::from_be_bytes(raw)
    } else {
        u16::from_le_bytes(raw)
    })
}

fn read_u32(data: &[u8], at: usize, be: bool) -> Option<u32> {
    let raw: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
    Some(if be {
        u32::from_be_bytes(raw)
    } else {
        u32::from_le_bytes(raw)
    })
}

/// Приводит сырые байты к показываемой строке: обрезает по нулю, чистит
/// управляющие символы и укорачивает до разумной длины.
fn text_value(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text: String = String::from_utf8_lossy(&bytes[..end])
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate(text.trim())
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= VALUE_LIMIT {
        return text.to_owned();
    }
    let cut: String = text.chars().take(VALUE_LIMIT).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests;
