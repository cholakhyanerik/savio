//! Тесты разбора и очистки контейнеров.
//!
//! Файлы собираются здесь же, побайтово: подкладывать в репозиторий бинарные
//! образцы не нужно, а собранный вручную файл заодно документирует формат.

use super::*;

// ---------------------------------------------------------------------------
// Сборка образцов
// ---------------------------------------------------------------------------

/// Блок EXIF с одним тегом «Производитель» = SECRETCAM.
fn exif_block() -> Vec<u8> {
    let make = b"SECRETCAM\x00";
    let mut out = Vec::new();
    out.extend_from_slice(b"MM\x00\x2a"); // порядок байт и магическое число
    out.extend_from_slice(&8u32.to_be_bytes()); // смещение IFD0
    out.extend_from_slice(&1u16.to_be_bytes()); // одна запись
    out.extend_from_slice(&0x010Fu16.to_be_bytes()); // тег Make
    out.extend_from_slice(&2u16.to_be_bytes()); // тип ASCII
    out.extend_from_slice(&(make.len() as u32).to_be_bytes());
    out.extend_from_slice(&26u32.to_be_bytes()); // значение лежит за IFD
    out.extend_from_slice(&0u32.to_be_bytes()); // следующего IFD нет
    out.extend_from_slice(make);
    out
}

fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, marker];
    out.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// JPEG с JFIF, EXIF, ICC, комментарием и «сжатыми данными».
fn sample_jpeg() -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    out.extend(segment(
        0xE0,
        b"JFIF\x00\x01\x02\x00\x00\x01\x00\x01\x00\x00",
    ));
    out.extend(segment(
        0xE1,
        &[b"Exif\x00\x00".as_slice(), &exif_block()].concat(),
    ));
    out.extend(segment(0xE2, b"ICC_PROFILE\x00PROFILEDATA"));
    out.extend(segment(0xED, b"Photoshop 3.0\x00IPTCDATA"));
    out.extend(segment(0xFE, b"SECRETCOMMENT"));
    out.extend(segment(0xDB, &[0u8; 64])); // таблица квантования
    out.extend(segment(0xDA, &[0x01, 0x00, 0x00, 0x00])); // SOS
    out.extend_from_slice(b"\xAA\xBB\xCC\xDD ENTROPY DATA \xFF\xD9");
    out
}

fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    // Контрольная сумма для разбора не проверяется — кладём заглушку.
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

fn sample_png() -> Vec<u8> {
    let mut out = Vec::from(PNG_SIGNATURE);
    out.extend(png_chunk(b"IHDR", &[0u8; 13]));
    out.extend(png_chunk(b"gAMA", &[0u8; 4]));
    out.extend(png_chunk(b"tEXt", b"Comment\x00SECRETPNGTEXT"));
    out.extend(png_chunk(b"eXIf", &exif_block()));
    out.extend(png_chunk(b"zTXt", b"Software\x00\x00compressed"));
    out.extend(png_chunk(b"IDAT", b"PIXELDATA"));
    out.extend(png_chunk(b"IEND", b""));
    out
}

fn riff_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn sample_webp() -> Vec<u8> {
    let mut body = Vec::new();
    // VP8X с выставленными флагами EXIF (0x08) и XMP (0x04).
    body.extend(riff_chunk(
        b"VP8X",
        &[0x0C, 0, 0, 0, 0x1F, 0, 0, 0x1F, 0, 0],
    ));
    body.extend(riff_chunk(b"VP8 ", b"COMPRESSED PIXELS"));
    body.extend(riff_chunk(b"EXIF", &exif_block()));
    body.extend(riff_chunk(b"XMP ", b"<x:xmpmeta>SECRET</x:xmpmeta>"));

    let mut out = Vec::from(b"RIFF".as_slice());
    out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    out
}

fn sample_gif() -> Vec<u8> {
    let mut out = Vec::from(b"GIF89a".as_slice());
    out.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]); // без таблицы цветов
    // Управляющее расширение — должно уцелеть.
    out.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    // Комментарий — должен исчезнуть.
    out.extend_from_slice(&[0x21, 0xFE, 0x06]);
    out.extend_from_slice(b"SECRET");
    out.push(0x00);
    // Кадр.
    out.extend_from_slice(&[0x2C, 0, 0, 0, 0, 0x01, 0, 0x01, 0, 0x00]);
    out.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00]);
    out.push(0x3B);
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

#[test]
fn jpeg_reads_exif_and_comment() {
    let tags = read_jpeg(&sample_jpeg()).unwrap();
    assert!(
        tags.iter()
            .any(|t| t.name == "Производитель" && t.value == "SECRETCAM"),
        "не прочитан EXIF: {tags:?}"
    );
    assert!(
        tags.iter()
            .any(|t| t.name == "Комментарий" && t.value == "SECRETCOMMENT")
    );
    assert!(tags.iter().any(|t| t.name == "IPTC / Photoshop"));
}

#[test]
fn jpeg_strip_removes_all_metadata() {
    let cleaned = strip_jpeg(&sample_jpeg()).unwrap();

    assert!(!contains(&cleaned, b"SECRETCAM"), "EXIF уцелел");
    assert!(!contains(&cleaned, b"SECRETCOMMENT"), "комментарий уцелел");
    assert!(!contains(&cleaned, b"IPTCDATA"), "IPTC уцелел");
    // Повторное чтение обязано показать пустой список — критерий приёмки.
    assert!(read_jpeg(&cleaned).unwrap().is_empty());
}

#[test]
fn jpeg_strip_keeps_image_and_rendering_segments() {
    let cleaned = strip_jpeg(&sample_jpeg()).unwrap();

    // Сжатые данные обязаны дойти байт в байт: в этом весь смысл lossless.
    assert!(contains(&cleaned, b"\xAA\xBB\xCC\xDD ENTROPY DATA"));
    // JFIF и ICC влияют на вид картинки и удалению не подлежат.
    assert!(contains(&cleaned, b"JFIF"), "JFIF удалён");
    assert!(contains(&cleaned, b"ICC_PROFILE"), "профиль ICC удалён");
    assert!(cleaned.starts_with(&[0xFF, 0xD8]), "потеряна сигнатура");
}

#[test]
fn jpeg_strip_is_idempotent() {
    let once = strip_jpeg(&sample_jpeg()).unwrap();
    let twice = strip_jpeg(&once).unwrap();
    assert_eq!(once, twice, "повторная очистка изменила файл");
}

#[test]
fn jpeg_rejects_foreign_file() {
    assert!(strip_jpeg(&sample_png()).is_err());
    assert!(jpeg_segments(b"not a jpeg at all").is_err());
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

#[test]
fn png_reads_text_and_exif() {
    let tags = read_png(&sample_png()).unwrap();
    assert!(
        tags.iter()
            .any(|t| t.name == "Comment" && t.value == "SECRETPNGTEXT")
    );
    assert!(tags.iter().any(|t| t.value == "SECRETCAM"));
}

#[test]
fn png_strip_removes_text_keeps_pixels() {
    let cleaned = strip_png(&sample_png()).unwrap();

    assert!(!contains(&cleaned, b"SECRETPNGTEXT"), "tEXt уцелел");
    assert!(!contains(&cleaned, b"SECRETCAM"), "eXIf уцелел");
    assert!(!contains(&cleaned, b"zTXt"), "zTXt уцелел");
    assert!(contains(&cleaned, b"PIXELDATA"), "потеряны пиксели");
    assert!(
        contains(&cleaned, b"gAMA"),
        "гамма влияет на вид, удалять её нельзя"
    );
    assert!(cleaned.starts_with(&PNG_SIGNATURE));
    assert!(read_png(&cleaned).unwrap().is_empty());
}

#[test]
fn png_strip_keeps_apng_animation() {
    let mut src = Vec::from(PNG_SIGNATURE);
    src.extend(png_chunk(b"IHDR", &[0u8; 13]));
    src.extend(png_chunk(b"acTL", &[0u8; 8]));
    src.extend(png_chunk(b"fcTL", &[0u8; 26]));
    src.extend(png_chunk(b"IDAT", b"FRAME1"));
    src.extend(png_chunk(b"fdAT", b"FRAME2"));
    src.extend(png_chunk(b"tEXt", b"k\x00SECRET"));
    src.extend(png_chunk(b"IEND", b""));

    let cleaned = strip_png(&src).unwrap();
    // Выбросить кадры APNG значило бы превратить анимацию в картинку —
    // это уже потеря содержимого, а не метаданных.
    assert!(contains(&cleaned, b"acTL"));
    assert!(contains(&cleaned, b"fdAT"));
    assert!(contains(&cleaned, b"FRAME2"));
    assert!(!contains(&cleaned, b"SECRET"));
}

// ---------------------------------------------------------------------------
// WebP
// ---------------------------------------------------------------------------

#[test]
fn webp_reads_exif_and_xmp() {
    let tags = read_webp(&sample_webp()).unwrap();
    assert!(tags.iter().any(|t| t.value == "SECRETCAM"));
    assert!(tags.iter().any(|t| t.name == "XMP"));
}

#[test]
fn webp_strip_removes_metadata_and_clears_flags() {
    let cleaned = strip_webp(&sample_webp()).unwrap();

    assert!(!contains(&cleaned, b"SECRETCAM"), "EXIF уцелел");
    assert!(!contains(&cleaned, b"<x:xmpmeta>"), "XMP уцелел");
    assert!(contains(&cleaned, b"COMPRESSED PIXELS"), "потеряны пиксели");

    // Флаги EXIF и XMP в VP8X обязаны погаснуть: иначе просмотрщик пойдёт
    // искать вырезанные чанки и сочтёт файл битым.
    let flags = cleaned[cleaned
        .windows(4)
        .position(|w| w == b"VP8X")
        .expect("VP8X потерян")
        + 8];
    assert_eq!(flags & 0b0000_1100, 0, "флаги EXIF/XMP не сброшены");

    // Заголовок RIFF обязан сойтись с новой длиной, иначе файл не откроется.
    let declared = u32::from_le_bytes(cleaned[4..8].try_into().unwrap()) as usize;
    assert_eq!(declared, cleaned.len() - 8, "размер RIFF разошёлся");

    assert!(read_webp(&cleaned).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// GIF
// ---------------------------------------------------------------------------

#[test]
fn gif_strip_removes_comment_keeps_frame() {
    let src = sample_gif();
    assert!(read_gif(&src).unwrap().iter().any(|t| t.value == "SECRET"));

    let cleaned = strip_gif(&src).unwrap();
    assert!(!contains(&cleaned, b"SECRET"), "комментарий уцелел");
    // Управляющее расширение задаёт задержку и прозрачность кадра.
    assert!(
        contains(&cleaned, &[0x21, 0xF9]),
        "потеряно управление кадром"
    );
    assert!(cleaned.ends_with(&[0x3B]), "потерян конец файла");
    assert!(read_gif(&cleaned).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// EXIF
// ---------------------------------------------------------------------------

#[test]
fn exif_reads_both_byte_orders() {
    let tags = read_tiff(&exif_block());
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].value, "SECRETCAM");

    // Тот же тег, но с обратным порядком байт.
    let make = b"SECRETCAM\x00";
    let mut le = Vec::new();
    le.extend_from_slice(b"II\x2a\x00");
    le.extend_from_slice(&8u32.to_le_bytes());
    le.extend_from_slice(&1u16.to_le_bytes());
    le.extend_from_slice(&0x010Fu16.to_le_bytes());
    le.extend_from_slice(&2u16.to_le_bytes());
    le.extend_from_slice(&(make.len() as u32).to_le_bytes());
    le.extend_from_slice(&26u32.to_le_bytes());
    le.extend_from_slice(&0u32.to_le_bytes());
    le.extend_from_slice(make);

    assert_eq!(read_tiff(&le), tags);
}

#[test]
fn exif_survives_garbage() {
    // Мусор вместо EXIF не должен ронять разбор: показать нечего — и ладно.
    assert!(read_tiff(b"").is_empty());
    assert!(read_tiff(b"MM\x00\x2a").is_empty());
    assert!(read_tiff(b"not exif at all, just bytes").is_empty());
    assert!(read_tiff(&[0xFF; 64]).is_empty());
}

#[test]
fn exif_does_not_loop_on_self_referencing_ifd() {
    // Каталог, ссылающийся сам на себя. Без защиты разбор завис бы намертво,
    // и повесил бы поток вместе с собой.
    let mut data = Vec::new();
    data.extend_from_slice(b"MM\x00\x2a");
    data.extend_from_slice(&8u32.to_be_bytes());
    data.extend_from_slice(&0u16.to_be_bytes()); // ноль записей
    data.extend_from_slice(&8u32.to_be_bytes()); // следующий IFD — он же сам

    assert!(read_tiff(&data).is_empty());
}

// ---------------------------------------------------------------------------
// MP3
// ---------------------------------------------------------------------------

/// Собирает MP3 из тега ID3v2, «звука» и хвостовых тегов.
fn sample_mp3(audio: &[u8], id3v2: usize, id3v1: bool, apev2: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if id3v2 > 0 {
        out.extend_from_slice(b"ID3\x03\x00\x00");
        // Размер записывается по 7 бит на байт.
        let size = id3v2 - 10;
        out.push(((size >> 21) & 0x7F) as u8);
        out.push(((size >> 14) & 0x7F) as u8);
        out.push(((size >> 7) & 0x7F) as u8);
        out.push((size & 0x7F) as u8);
        out.extend(std::iter::repeat_n(b'X', size));
    }
    out.extend_from_slice(audio);
    if apev2 > 0 {
        out.extend(std::iter::repeat_n(b'A', apev2 - 32));
        out.extend_from_slice(b"APETAGEX");
        out.extend_from_slice(&2000u32.to_le_bytes()); // версия
        // Размер в APEv2 считается вместе с завершителем, но без заголовка.
        // Записать сюда длину одного лишь содержимого — классическая ошибка:
        // хвост тега на 32 байта тогда остаётся в файле.
        out.extend_from_slice(&(apev2 as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // число записей
        out.extend_from_slice(&0u32.to_le_bytes()); // флаги: заголовка нет
        out.extend_from_slice(&[0u8; 8]);
    }
    if id3v1 {
        out.extend_from_slice(b"TAG");
        out.extend_from_slice(&[b'T'; 125]);
    }
    out
}

fn bounds_of(file: &[u8]) -> (usize, usize) {
    let head = &file[..10.min(file.len())];
    let tail_start = file.len().saturating_sub(MP3_TAIL_WINDOW as usize);
    let b = mp3_bounds(head, &file[tail_start..], file.len());
    (b.start, b.end)
}

#[test]
fn mp3_finds_id3v2_at_start() {
    let file = sample_mp3(b"AUDIOAUDIO", 60, false, 0);
    assert_eq!(bounds_of(&file), (60, file.len()));
    assert_eq!(&file[60..], b"AUDIOAUDIO");
}

#[test]
fn mp3_finds_id3v1_at_end() {
    let file = sample_mp3(b"AUDIOAUDIO", 0, true, 0);
    assert_eq!(bounds_of(&file), (0, 10));
}

#[test]
fn mp3_finds_ape_and_id3v1_together() {
    // Оба хвостовых тега сразу — обычное дело у файлов из старых качалок.
    let file = sample_mp3(b"AUDIOAUDIO", 40, true, 64);
    let (start, end) = bounds_of(&file);
    assert_eq!(start, 40);
    assert_eq!(&file[start..end], b"AUDIOAUDIO");
}

#[test]
fn mp3_without_tags_is_left_alone() {
    // Ни одного тега — границы совпадают с файлом, и переписывать его незачем.
    let file = sample_mp3(b"PURE AUDIO DATA", 0, false, 0);
    assert_eq!(bounds_of(&file), (0, file.len()));
}

#[test]
fn mp3_ignores_broken_id3v2_size() {
    // Старший бит в размере запрещён. Встретив его, размеру верить нельзя:
    // вычтя мусор, мы отрезали бы кусок звука.
    let mut file = sample_mp3(b"AUDIO", 40, false, 0);
    file[6] = 0xFF;
    assert_eq!(bounds_of(&file), (0, file.len()));
}

// ---------------------------------------------------------------------------
// Общее
// ---------------------------------------------------------------------------

#[test]
fn long_values_are_truncated() {
    let long = "x".repeat(VALUE_LIMIT * 2);
    let shown = text_value(long.as_bytes());
    assert_eq!(shown.chars().count(), VALUE_LIMIT + 1, "нет многоточия");
    assert!(shown.ends_with('…'));
}

#[test]
fn control_characters_do_not_break_layout() {
    // Перевод строки в теге растянул бы строку списка на весь экран.
    assert_eq!(text_value(b"line\x0aline\x09tab"), "line line tab");
    assert_eq!(text_value(b"trimmed\x00ignored"), "trimmed");
}

// ---------------------------------------------------------------------------
// Настоящие файлы
//
// Собранные вручную образцы выше проверяют разбор, но не отвечают на главный
// вопрос: доходят ли пиксели и звук до конца в неизменном виде. Здесь файлы
// делает ffmpeg, а после очистки они снова декодируются — и сравниваются
// хеши распакованного содержимого. Совпали хеши — перекодирования не было.
//
// Тест помечен `ignore`: он требует ffmpeg в PATH и потому не должен падать
// на машине, где его нет.
// ---------------------------------------------------------------------------

/// Декодирует файл во внутреннее представление и возвращает хеш результата.
/// Хеш именно распакованных данных: сравнивать сами файлы бессмысленно —
/// они и должны отличаться на вырезанные метаданные.
#[cfg(test)]
fn decoded_hash(path: &Path, audio: bool) -> String {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-v").arg("error").arg("-i").arg(path);
    if audio {
        cmd.args(["-f", "s16le", "-ac", "1"]);
    } else {
        cmd.args(["-f", "rawvideo", "-pix_fmt", "rgb24"]);
    }
    let out = cmd
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("ffmpeg не запустился");
    assert!(out.status.success(), "ffmpeg не смог декодировать {path:?}");
    assert!(!out.stdout.is_empty(), "ffmpeg вернул пустой поток");

    let mut hasher = crate::engine::sha256::Sha256::new();
    hasher.update(&out.stdout);
    crate::engine::sha256::hex(&hasher.finish())
}

fn ffmpeg_make(args: &[&str], out: &Path) {
    let status = Command::new("ffmpeg")
        .arg("-v")
        .arg("error")
        .args(args)
        .arg("-y")
        .arg(out)
        .status()
        .expect("ffmpeg не запустился");
    assert!(status.success(), "ffmpeg не собрал образец {out:?}");
}

#[test]
#[ignore = "требует ffmpeg в PATH"]
fn real_files_lose_metadata_but_keep_content() {
    let dir = std::env::temp_dir().join("savio-metadata-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    const SOURCE: &[&str] = &[
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:duration=1:rate=1",
        "-frames:v",
        "1",
    ];

    // --- JPEG: EXIF вставляем сегментом APP1, как это делает камера.
    let jpeg = dir.join("photo.jpg");
    ffmpeg_make(SOURCE, &jpeg);
    let mut data = std::fs::read(&jpeg).unwrap();
    let app1 = segment(0xE1, &[b"Exif\x00\x00".as_slice(), &exif_block()].concat());
    data.splice(2..2, app1);
    std::fs::write(&jpeg, &data).unwrap();

    // --- PNG: текстовый чанк сразу после IHDR.
    let png = dir.join("shot.png");
    ffmpeg_make(SOURCE, &png);
    let mut data = std::fs::read(&png).unwrap();
    let ihdr_end = 8 + 8 + 13 + 4;
    data.splice(
        ihdr_end..ihdr_end,
        png_chunk(b"tEXt", b"Comment\x00SECRETCAM"),
    );
    std::fs::write(&png, &data).unwrap();

    // --- WebP: чанк EXIF в конец контейнера, с поправкой размера RIFF.
    let webp = dir.join("pic.webp");
    ffmpeg_make(SOURCE, &webp);
    let mut data = std::fs::read(&webp).unwrap();
    data.extend(riff_chunk(b"EXIF", &exif_block()));
    let size = (data.len() - 8) as u32;
    data[4..8].copy_from_slice(&size.to_le_bytes());
    std::fs::write(&webp, &data).unwrap();

    // --- MP3: теги пишет сам ffmpeg, включая обложку.
    let mp3 = dir.join("song.mp3");
    ffmpeg_make(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "libmp3lame",
            "-metadata",
            "title=SECRETCAM",
            "-metadata",
            "artist=SECRETCAM",
        ],
        &mp3,
    );

    for (path, audio) in [(&jpeg, false), (&png, false), (&webp, false), (&mp3, true)] {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();

        // До очистки метаданные обязаны быть видны — иначе тест проверяет
        // пустоту и молча «проходит».
        let before = read(path, None).unwrap();
        assert!(
            !before.is_empty() || audio,
            "{name}: образец собран без метаданных, проверять нечего"
        );
        let hash_before = decoded_hash(path, audio);
        let size_before = std::fs::metadata(path).unwrap().len();

        let freed = strip(path).unwrap();

        // 1. Метаданных больше нет — ни в нашем разборе, ни в сырых байтах.
        let after = read(path, None).unwrap();
        assert!(after.is_empty(), "{name}: метаданные уцелели: {after:?}");
        assert!(
            !contains(&std::fs::read(path).unwrap(), b"SECRETCAM"),
            "{name}: значение тега осталось в файле"
        );

        // 2. Содержимое дошло байт в байт: ни один пиксель и ни один отсчёт
        //    звука не поменялся, то есть перекодирования не было.
        assert_eq!(
            hash_before,
            decoded_hash(path, audio),
            "{name}: содержимое изменилось — где-то произошло перекодирование"
        );

        // 3. Файл действительно похудел, и ровно на столько, сколько заявлено.
        let size_after = std::fs::metadata(path).unwrap().len();
        assert_eq!(size_before - size_after, freed, "{name}: не сходится счёт");
        assert!(freed > 0, "{name}: ничего не удалено");

        // 4. Дубликатов рядом не осталось — временный файл убран.
        assert!(
            !temp_path(path).exists(),
            "{name}: рядом остался временный файл"
        );

        // 5. Повторная очистка ничего не меняет.
        assert_eq!(
            strip(path).unwrap(),
            0,
            "{name}: повторная очистка не пуста"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn temp_file_lands_next_to_original() {
    // Временный файл обязан лежать на том же томе, иначе переименование
    // перестанет быть атомарным.
    let path = Path::new("C:/photos/IMG_0001.jpg");
    let tmp = temp_path(path);
    assert_eq!(tmp.parent(), path.parent());
    assert_ne!(tmp, path.to_path_buf());
}
