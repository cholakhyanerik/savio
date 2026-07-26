//! Сборка аргументов yt-dlp и разбор его вывода.
//!
//! Прогресс читается не из человекочитаемого вывода, а из машинного:
//! `--progress-template` заставляет yt-dlp печатать готовый JSON, который
//! остаётся только распарсить. Скрейпинг обычного вывода ломается при
//! каждом изменении форматирования, поэтому так делать не стоит.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::binaries::Tools;
use crate::model::{Format, MediaInfo, Progress, Quality, Request};

/// Каждое поле, способное прийти пустым, обязано иметь `|default`.
/// Иначе yt-dlp подставит голое `NA` без кавычек и сломает JSON —
/// конверсия `j` к null-полю не применяется.
const DOWNLOAD_TEMPLATE: &str = concat!(
    r#"download:{"status":%(progress.status)j,"#,
    r#""downloaded":%(progress.downloaded_bytes|0)f,"#,
    r#""total":%(progress.total_bytes,progress.total_bytes_estimate|0)f,"#,
    r#""speed":%(progress.speed|0)f,"#,
    r#""eta":%(progress.eta|0)f}"#,
);

const POSTPROCESS_TEMPLATE: &str = concat!(
    r#"postprocess:{"status":%(progress.status)j,"#,
    r#""pp":%(progress.postprocessor)j}"#,
);

/// `after_move` — единственная стадия, на которой путь уже окончательный.
/// На этой стадии `--print` не включает `--simulate`, поэтому загрузка идёт как обычно.
const DONE_TEMPLATE: &str = r#"after_move:{"event":"done","path":%(filepath)j}"#;

pub fn probe_args(url: &str) -> Vec<String> {
    vec![
        "-J".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        url.into(),
    ]
}

/// Собирает значение `-f` для видео.
///
/// Цепочка читается слева направо и разбирается по `/`: берётся первое звено,
/// под которое нашлись дорожки. Порядок звеньев прежний — чистый MP4/M4A
/// (склейка без перекодирования), затем любые лучшие дорожки, затем готовый
/// совмещённый файл, — а ограничение по высоте просто навешивается на каждое.
///
/// **Хвостовой `/b` без ограничения обязателен.** Без него ролик, у которого
/// нет ни одной дорожки нужной высоты (720p просят у записи, снятой в 480p),
/// не скачается вовсе: yt-dlp ответит «Requested format is not available» и
/// выйдет с ошибкой. С хвостом завышенное качество молча опускается до лучшего
/// доступного — а это ровно то, чего ждёт человек, выбравший «не больше 720p».
fn video_format(quality: Quality) -> String {
    let Some(height) = quality.max_height() else {
        // Без ограничения — строка ровно та же, что была до появления выбора.
        return "bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b".to_owned();
    };

    format!(
        "bv*[height<={height}][ext=mp4]+ba[ext=m4a]\
         /bv*[height<={height}]+ba\
         /b[height<={height}]\
         /b"
    )
}

pub fn download_args(request: &Request, out_dir: &Path, tools: &Tools) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--newline".into(),
        // --quiet глушит обычный вывод, --progress возвращает обратно
        // только прогресс. Вместе они дают чистый машинный поток.
        "--quiet".into(),
        "--progress".into(),
        "--progress-template".into(),
        DOWNLOAD_TEMPLATE.into(),
        "--progress-template".into(),
        POSTPROCESS_TEMPLATE.into(),
        "--print".into(),
        DONE_TEMPLATE.into(),
        "--no-playlist".into(),
        "--windows-filenames".into(),
        "-P".into(),
        out_dir.to_string_lossy().into_owned(),
        "-o".into(),
        "%(title)s.%(ext)s".into(),
    ];

    match request.format {
        Format::Mp4 => args.extend([
            "-f".into(),
            video_format(request.quality),
            "--merge-output-format".into(),
            "mp4".into(),
        ]),
        Format::Mp3 => args.extend([
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--audio-quality".into(),
            // `0` — не «ноль килобит», а верх шкалы VBR у LAME (V0). Так Savio
            // качал звук до появления выбора, и для «Макс.» это поведение
            // сохраняется дословно. Остальные ступени — обычный битрейт,
            // ffmpeg отличает одно от другого по букве «K» в конце.
            request.quality.audio_bitrate().unwrap_or("0").into(),
        ]),
    }

    if let Some(ffmpeg) = &tools.ffmpeg {
        args.push("--ffmpeg-location".into());
        args.push(ffmpeg.to_string_lossy().into_owned());

        // Вшивание навешиваем **только** при живом ffmpeg, и это не
        // перестраховка. Проверено вживую (yt-dlp 2026.07.04): с любым из этих
        // ключей и без ffmpeg yt-dlp скачивает ролик, а потом выходит с кодом 1
        // и `ERROR: Postprocessing: ffmpeg not found`, оставляя рядом с готовым
        // файлом ещё и скачанную обложку `.jpg`. То есть галочка не «просто не
        // сработала» — она превращает удавшуюся загрузку в ошибку без пути
        // к файлу (стадия `after_move` не наступает). Пропустить ключи и сказать
        // об этом словами (`Event::Warning` в `engine::start`) — единственный
        // исход, при котором пользователь получает и файл, и правду.
        let options = request.options;
        if options.embed_metadata {
            args.push("--embed-metadata".into());
        }
        if options.embed_thumbnail {
            args.push("--embed-thumbnail".into());
        }
        // Субтитры бывают только у видео: в MP3 их положить некуда, и yt-dlp
        // на такой просьбе ругается на пустом месте.
        //
        // `--write-subs` рядом не нужен, хотя его и тянет дописать «чтобы
        // субтитры точно скачались». Проверено: `--embed-subs` сам включает их
        // загрузку (без него yt-dlp субтитры даже не запрашивает, с ним —
        // сообщает «There are no subtitles for the requested languages»).
        // Единственное, что добавляет `--write-subs`, — это **сохранённый рядом
        // `.vtt`**: yt-dlp считает такой файл затребованным пользователем и
        // после вшивания не убирает. Просили вшить, а не положить рядом.
        if options.embed_subs && request.format == Format::Mp4 {
            args.push("--embed-subs".into());
        }
    }

    args.push(request.url.clone());
    args
}

/// На Windows GUI-приложение не должно моргать консолью при каждом запуске.
#[cfg(windows)]
pub fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_console(_cmd: &mut Command) {}

/// Разобранная строка вывода yt-dlp.
pub enum Line {
    Progress(Progress),
    Stage(String),
    Done(PathBuf),
    /// Не наш формат — отдаём в лог как есть.
    Other(String),
}

pub fn parse_line(line: &str) -> Line {
    let line = line.trim();
    if !line.starts_with('{') {
        return Line::Other(if line.is_empty() {
            String::new()
        } else {
            line.to_owned()
        });
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Line::Other(line.to_owned());
    };

    // Префикс в `--progress-template download:{…}` выбирает момент вывода
    // и самим yt-dlp съедается — в поток приходит голый JSON без него.
    // Поэтому шаблоны различаем по набору полей, а не по началу строки.
    if v.get("event").and_then(|x| x.as_str()) == Some("done") {
        if let Some(path) = v.get("path").and_then(|x| x.as_str()) {
            return Line::Done(PathBuf::from(path));
        }
        return Line::Other(line.to_owned());
    }

    // `pp` есть только у шаблона постобработки.
    if v.get("pp").is_some() {
        let pp = v.get("pp").and_then(|x| x.as_str()).unwrap_or("обработка");
        return Line::Stage(format!("Обработка: {pp}"));
    }

    if v.get("downloaded").is_some() {
        let num = |key: &str| v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let speed = num("speed");
        let eta = num("eta");
        return Line::Progress(Progress {
            downloaded: num("downloaded") as u64,
            total: num("total") as u64,
            speed_bps: (speed > 0.0).then_some(speed),
            eta_secs: (eta > 0.0).then_some(eta as u64),
        });
    }

    Line::Other(line.to_owned())
}

/// Ссылка на список сайтов, которые умеет yt-dlp.
///
/// Savio своего списка не ведёт и вести не должен: он меняется с каждым
/// выпуском yt-dlp, и любая наша копия устареет молча.
const SUPPORTED_SITES: &str =
    "https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md";

/// Узнаваемые причины отказа: приметы в stderr → объяснение для человека.
///
/// Сравниваем в нижнем регистре и держим по нескольку фраз на причину: одну
/// и ту же беду разные экстракторы называют по-разному («Private video» у
/// одного, «This video is private» у другого), а формулировки меняются между
/// выпусками yt-dlp. Промах подстроки не заметит ни компилятор, ни тест: он
/// не ломает ничего, а просто возвращает сырой английский хвост — поэтому
/// фраз на причину несколько, и каждая приметная.
///
/// Приметная — значит целая фраза, а не слово. На одном `age` под правило
/// попали бы и `Usage`, и `message`, и любая ссылка со `/page/`, и человек
/// получил бы уверенное объяснение не про свою беду. Ошибиться в сторону
/// сырого хвоста не страшно, в сторону чужой подсказки — стыдно.
///
/// Порядок важен: сверху причина конкретнее. Проверка «не робот» и возрастная
/// заглушка нередко приезжают в одном хвосте с 403, и сказать надо про них,
/// а не про код ответа.
///
/// Неподдерживаемого сайта в таблице нет: его объяснение подставляет ссылку
/// на список сайтов, а в константе форматировать нечем.
const FAILURE_HINTS: &[(&[&str], &str)] = &[
    (
        &["not a bot"],
        "Сайт требует подтвердить, что вы не робот.\n\n\
         Так отвечают, когда с вашего адреса приходит слишком много запросов. \
         Нажмите «Обновить движок» и попробуйте снова через несколько минут. \
         Если включён VPN — выключите его: одним адресом пользуются многие, \
         и проверка на нём срабатывает чаще.",
    ),
    (
        &[
            "confirm your age",
            "age-restricted",
            "age restricted",
            "inappropriate for some users",
        ],
        "Видео с возрастным ограничением.\n\n\
         Сайт отдаёт его только тем, кто вошёл в аккаунт, а Savio входить \
         не умеет. Иногда помогает «Обновить движок»: свежий yt-dlp обходит \
         часть таких проверок.",
    ),
    (
        &["private video", "is private"],
        "Доступ к видео закрыт.\n\n\
         Владелец сделал его приватным — оно отдаётся только тем, кому он \
         открыл доступ. Savio входить в аккаунт не умеет, поэтому скачать \
         не получится. Проверьте, нет ли открытой копии по другой ссылке.",
    ),
    (
        // «in your country» без отрицания: YouTube строит фразу тремя
        // способами — «is not available in your country», «has not made this
        // video available in your country», «has blocked it in your country».
        // Отрицание стоит в разных местах, общее у них только это.
        &[
            "geo restriction",
            "geo-restricted",
            "in your country",
            "from your location",
        ],
        "Видео недоступно в вашей стране.\n\n\
         Сайт закрыл его по региону — дело не в ссылке и не в Savio. \
         Помогает только смена региона: VPN или прокси, включённые до \
         начала загрузки.",
    ),
    (
        &["http error 403", "403: forbidden"],
        "Сервер отказал в доступе (ошибка 403).\n\n\
         Чаще всего это значит, что сайт сменил защиту и движок устарел — \
         нажмите «Обновить движок». Если не помогло, откройте страницу заново \
         и скопируйте ссылку: прежняя могла быть одноразовой и уже истечь.",
    ),
];

/// Превращает провал yt-dlp в сообщение для пользователя.
///
/// Сырой хвост stderr показывать можно не всегда: для самой частой причины —
/// сайт просто не поддерживается — строка `ERROR: Unsupported URL: …` ничего
/// не объясняет тому, кто не знает, что внутри Savio работает yt-dlp. Человек
/// видит «ошибку» и считает, что сломалось приложение, хотя ломаться нечему.
/// То же и с отказами по 403, приватности, возрасту и проверке «не робот»:
/// английский хвост не говорит ни что случилось, ни что делать.
///
/// Подменой диагностики это не становится: каждая строка stderr уходит ещё
/// и в журнал (`Event::Log`), откуда её можно скопировать целиком. Незнакомый
/// случай так и остаётся сырым хвостом — догадкой его подменять нельзя.
pub fn explain_failure(code: i32, tail: &str) -> String {
    // Признак ищем по подстроке, а не по началу строки: yt-dlp печатает
    // `ERROR: Unsupported URL: …`, но перед этим может идти префикс
    // экстрактора, а с `--no-warnings` — и вовсе другая раскладка.
    if tail.contains("Unsupported URL") {
        return format!(
            "Этот сайт не поддерживается.\n\n\
             Savio скачивает через yt-dlp, а он не умеет работать с этим адресом. \
             Дело не в ссылке и не в приложении — сайта просто нет в списке \
             поддерживаемых:\n{SUPPORTED_SITES}\n\n\
             Если сайт там есть, движок устарел — нажмите «Обновить движок»."
        );
    }

    // Регистр приводим один раз на всю таблицу. Аллокация здесь безобидна:
    // путь выполняется однажды на сорвавшуюся загрузку, а не в кадре.
    // `to_ascii_lowercase` вместо `to_lowercase`: приметы английские, а
    // трогать чужой юникод в чужой диагностике незачем.
    let lower = tail.to_ascii_lowercase();
    for (phrases, message) in FAILURE_HINTS {
        if phrases.iter().any(|phrase| lower.contains(phrase)) {
            return (*message).to_owned();
        }
    }

    let hint = match code {
        101 => "загрузка остановлена (лимит или файл уже есть)",
        2 => "yt-dlp не принял аргументы — это баг Savio",
        _ if tail.is_empty() => "yt-dlp завершился с ошибкой без подробностей",
        _ => "",
    };

    if tail.is_empty() {
        format!("Ошибка (код {code}): {hint}")
    } else {
        format!("Ошибка (код {code}):\n{tail}")
    }
}

pub fn parse_media_info(json: &str) -> MediaInfo {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return MediaInfo::default();
    };
    let text = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.to_owned())
    };
    MediaInfo {
        title: text("title"),
        uploader: text("uploader"),
        duration_secs: v.get("duration").and_then(|x| x.as_f64()),
        heights: parse_heights(v.get("formats")),
        thumbnail_url: parse_thumbnail_url(&v),
        has_subtitles: parse_has_subtitles(&v),
    }
}

/// Есть ли у ролика собственные субтитры.
///
/// Смотрим только `subtitles` — то, что выложил автор, — и намеренно не
/// заглядываем в `automatic_captions`: `--embed-subs` без `--write-auto-subs`
/// берёт ровно этот список, и посчитать автоматические значило бы обещать
/// субтитры, которых не будет.
///
/// `live_chat` из счёта выбрасываем: YouTube кладёт в `subtitles` запись чата
/// прошедшей трансляции, и по формату ответа она неотличима от дорожки
/// субтитров. Без этой проверки у любой записи стрима «субтитры есть» —
/// подсказка соврала бы ровно там, где нужна.
fn parse_has_subtitles(v: &serde_json::Value) -> bool {
    let Some(tracks) = v.get("subtitles").and_then(|x| x.as_object()) else {
        return false;
    };

    tracks.iter().any(|(lang, files)| {
        lang != "live_chat" && files.as_array().is_some_and(|files| !files.is_empty())
    })
}

/// Ширина обложки, которой нам достаточно. Превью в окне — около 240 точек.
///
/// Значение выбрано по настоящему ответу YouTube, а не на глаз, и трогать его
/// стоит только с таким же ответом перед глазами. Размеры там объявлены у
/// девяти вариантов: 120, 168, 196, 246, **320**, 336, 480, 640 и 1920 точек
/// в ширину — и среди них ровно два широкоэкранных, `mqdefault` (320×180) и
/// `maxresdefault` (1920×1080). Остальные — 4:3, то есть `hqdefault` 480×360
/// и `sddefault` 640×480 несут **чёрные полосы сверху и снизу, вжатые в саму
/// картинку**. Ни ошибкой, ни предупреждением это не оборачивается: превью
/// просто выглядит обрезанным огрызком в рамке.
///
/// 320 берёт `mqdefault` — самый дешёвый вариант без полос. Подними порог
/// до 640, и вернутся полосы; опусти до 240 — придёт 246×138, а это уже
/// заметно мыльно. Для сайтов, у которых мелких вариантов нет, порог работает
/// как раньше: возьмётся ближайший больший, а лишнее срежет `thumbnail::TARGET_WIDTH`.
const THUMBNAIL_TARGET_WIDTH: u64 = 320;

/// Выбирает адрес обложки из ответа `-J`.
///
/// Берём не самую большую картинку, а **самую маленькую из тех, что не меньше**
/// `THUMBNAIL_TARGET_WIDTH`. Причина в цене: `maxresdefault` у YouTube — это
/// 1920×1080, то есть лишние сотни килобайт из сети и восемь мегабайт RGBA
/// после разбора ради превью шириной 240 точек. Если такой нет — самая крупная
/// из мелких: растянутая мелкая картинка всё равно лучше пустого места.
///
/// Поэтому же список перебирается сам, а не берётся готовое поле `thumbnail`:
/// в нём лежит «лучшая» по мнению yt-dlp, то есть как раз самая большая. Полем
/// пользуемся, когда выбирать не из чего: часть экстракторов `thumbnails[]`
/// не заполняет вовсе.
fn parse_thumbnail_url(v: &serde_json::Value) -> Option<String> {
    let list = v.get("thumbnails").and_then(|x| x.as_array());

    let mut best: Option<(u64, &str)> = None;
    let mut last_unsized: Option<&str> = None;

    for item in list.into_iter().flatten() {
        let Some(url) = item
            .get("url")
            .and_then(|x| x.as_str())
            .filter(|url| !url.is_empty())
        else {
            continue;
        };

        // `as_f64`, а не `as_u64`, по той же причине, что и у высот дорожек:
        // часть экстракторов пишет размер дробным числом, и на `as_u64` такая
        // запись молча превращается в «размер неизвестен».
        let width = item
            .get("width")
            .and_then(|x| x.as_f64())
            .filter(|w| *w >= 1.0)
            .map(|w| w as u64);

        match width {
            Some(width) => {
                best = Some(match best {
                    Some(current) => better_thumbnail(current, (width, url)),
                    None => (width, url),
                });
            }
            // Список yt-dlp отсортирован от худшей картинки к лучшей, так что
            // последняя запись без размера — лучшая среди безразмерных.
            None => last_unsized = Some(url),
        }
    }

    let field = || {
        v.get("thumbnail")
            .and_then(|x| x.as_str())
            .filter(|url| !url.is_empty())
    };

    best.map(|(_, url)| url)
        .or_else(field)
        .or(last_unsized)
        .map(str::to_owned)
}

/// Какая из двух обложек нам нужнее. Вынесено из цикла, чтобы правило выбора
/// читалось целиком, а не собиралось из вложенных условий.
fn better_thumbnail<'a>(a: (u64, &'a str), b: (u64, &'a str)) -> (u64, &'a str) {
    let fits = |width: u64| width >= THUMBNAIL_TARGET_WIDTH;

    match (fits(a.0), fits(b.0)) {
        // Обе крупнее нужного — берём меньшую: разбирать 1920 точек ради 240 незачем.
        (true, true) => if b.0 < a.0 { b } else { a },
        (true, false) => a,
        (false, true) => b,
        // Ни одна не дотягивает — берём самую крупную из того, что есть.
        (false, false) => if b.0 > a.0 { b } else { a },
    }
}

/// Вытаскивает высоты видеодорожек из `formats[]` ответа `-J`.
///
/// Разбирается уже скачанный JSON — второго вызова yt-dlp здесь нет и быть
/// не должно: `probe` и так тянет полный ответ, а раньше просто выбрасывал
/// из него всё, кроме названия и длительности.
///
/// Дорожки без видео отсеиваем по `vcodec == "none"`, и это не перестраховка:
/// раскадровки предпросмотра (`sb0`, `sb1`, …) — тоже элементы `formats[]`,
/// со своими `width` и `height` в пару сотен точек. Без проверки в списке
/// доступных высот у любого ролика с YouTube появилось бы «180p», которого
/// там нет. Ни сборка, ни код возврата такого не ловят.
///
/// `as_f64`, а не `as_u64`: часть экстракторов отдаёт высоту дробным числом
/// (`1080.0`), и `as_u64` на таком молча возвращает `None` — список высот
/// оказался бы пустым без единого признака ошибки.
fn parse_heights(formats: Option<&serde_json::Value>) -> Vec<u32> {
    let Some(list) = formats.and_then(|x| x.as_array()) else {
        return Vec::new();
    };

    let mut heights: Vec<u32> = list
        .iter()
        .filter(|f| f.get("vcodec").and_then(|x| x.as_str()) != Some("none"))
        .filter_map(|f| f.get("height").and_then(|x| x.as_f64()))
        .filter(|h| *h >= 1.0)
        .map(|h| h as u32)
        .collect();

    // По убыванию: наверху то, что интереснее всего показать, — максимум.
    // `dedup` работает только на отсортированном, поэтому порядок сначала.
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    heights
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DownloadOptions;

    /// Строки взяты из реального вывода yt-dlp: префикса `download:` в них
    /// нет — он остаётся в аргументах, а не в потоке.
    const REAL_PROGRESS: &str = r#"{"status":"downloading","downloaded":195633173.000000,"total":712445280.000000,"speed":15943362.460976,"eta":32.000000}"#;

    #[test]
    fn progress_is_parsed_without_prefix() {
        let Line::Progress(p) = parse_line(REAL_PROGRESS) else {
            panic!("строка прогресса не распознана");
        };
        assert_eq!(p.downloaded, 195_633_173);
        assert_eq!(p.total, 712_445_280);
        assert_eq!(p.speed_bps, Some(15_943_362.460976));
        assert_eq!(p.eta_secs, Some(32));
        assert_eq!(p.fraction(), Some(195_633_173.0 / 712_445_280.0));
    }

    #[test]
    fn zero_speed_and_eta_become_none() {
        let line = r#"{"status":"downloading","downloaded":10.0,"total":0.0,"speed":0.0,"eta":0.0}"#;
        let Line::Progress(p) = parse_line(line) else {
            panic!("строка прогресса не распознана");
        };
        assert_eq!(p.total, 0);
        assert_eq!(p.speed_bps, None);
        assert_eq!(p.eta_secs, None);
        // Общий размер неизвестен — UI покажет неопределённый индикатор.
        assert_eq!(p.fraction(), None);
    }

    #[test]
    fn postprocess_becomes_stage() {
        let line = r#"{"status":"processing","pp":"Merger"}"#;
        let Line::Stage(stage) = parse_line(line) else {
            panic!("постобработка не распознана");
        };
        assert_eq!(stage, "Обработка: Merger");
    }

    #[test]
    fn done_carries_path() {
        let line = r#"{"event":"done","path":"C:\\Users\\me\\video.mp4"}"#;
        let Line::Done(path) = parse_line(line) else {
            panic!("завершение не распознано");
        };
        assert_eq!(path, PathBuf::from(r"C:\Users\me\video.mp4"));
    }

    #[test]
    fn junk_goes_to_log() {
        assert!(matches!(parse_line("[youtube] Extracting URL"), Line::Other(s) if !s.is_empty()));
        assert!(matches!(parse_line("   "), Line::Other(s) if s.is_empty()));
        // Оборванный JSON не должен ронять разбор.
        assert!(matches!(parse_line(r#"{"status":"#), Line::Other(_)));
    }

    /// Строка ровно та, что yt-dlp напечатал на kinobase.org — сайт, которого
    /// он не знает. Раньше она уходила в UI как есть, и человек видел
    /// английский дамп вместо объяснения.
    #[test]
    fn unsupported_site_is_explained_not_dumped() {
        let tail = "ERROR: Unsupported URL: https://kinobase.org/film/204049-menyu";
        let message = explain_failure(1, tail);

        assert!(
            message.contains("не поддерживается"),
            "нет объяснения: {message}"
        );
        assert!(
            message.contains(SUPPORTED_SITES),
            "нет ссылки на список сайтов: {message}"
        );
        // Про обновление сказать надо: сайт мог появиться в свежем выпуске.
        assert!(message.contains("Обновить движок"), "нет совета: {message}");
        // Сырую английскую строку показывать больше не нужно.
        assert!(
            !message.contains("Unsupported URL"),
            "утёк сырой вывод: {message}"
        );
    }

    /// Хвосты — в том виде, в каком их печатает yt-dlp: с префиксом
    /// экстрактора, английским текстом и советом про `--cookies`, которого
    /// в Savio всё равно нет.
    #[test]
    fn blocked_downloads_are_explained_in_russian() {
        let cases = [
            (
                "ERROR: [youtube] dQw4w9WgXcQ: Sign in to confirm you're not a bot. \
                 Use --cookies-from-browser or --cookies for the authentication.",
                "не робот",
            ),
            (
                "ERROR: [youtube] dQw4w9WgXcQ: Sign in to confirm your age. \
                 This video may be inappropriate for some users.",
                "возрастным ограничением",
            ),
            (
                "ERROR: [youtube] dQw4w9WgXcQ: Private video. \
                 Sign in if you've been granted access to this video",
                "приватным",
            ),
            (
                "ERROR: [youtube] dQw4w9WgXcQ: Video unavailable. The uploader \
                 has not made this video available in your country",
                "в вашей стране",
            ),
            (
                "ERROR: unable to download video data: HTTP Error 403: Forbidden",
                "403",
            ),
        ];

        for (tail, expected) in cases {
            let message = explain_failure(1, tail);
            assert!(
                message.contains(expected),
                "нет объяснения «{expected}»: {message}"
            );
            // Английский хвост в объяснение не подмешивается: он и так уходит
            // в журнал, а в баннере только мешал бы читать.
            assert!(!message.contains("ERROR"), "утёк сырой вывод: {message}");
            assert!(!message.contains(tail), "утёк сырой вывод: {message}");
        }
    }

    /// Приметы должны быть фразами, а не словами. Хвост ниже безобиден, но
    /// содержит `age` трижды — и на подстроке из одного слова человек получил
    /// бы уверенный рассказ про возрастное ограничение вместо своей беды.
    #[test]
    fn common_words_do_not_trigger_a_hint() {
        let tail = "ERROR: unable to rename file: Usage message from /page/1";
        let message = explain_failure(1, tail);
        assert!(message.contains(tail), "подсказка сработала зря: {message}");
    }

    /// Всё, что не опознано, обязано остаться как было: чужую диагностику
    /// лучше показать целиком, чем подменить догадкой.
    #[test]
    fn other_failures_keep_their_output() {
        let tail = "ERROR: unable to rename file: Permission denied";
        let message = explain_failure(1, tail);
        assert!(message.contains("код 1"), "{message}");
        assert!(message.contains(tail), "хвост stderr обязан остаться: {message}");

        // Коды с готовой подсказкой и пустой хвост — поведение прежнее.
        assert_eq!(
            explain_failure(101, ""),
            "Ошибка (код 101): загрузка остановлена (лимит или файл уже есть)"
        );
        assert_eq!(
            explain_failure(2, ""),
            "Ошибка (код 2): yt-dlp не принял аргументы — это баг Savio"
        );
        assert_eq!(
            explain_failure(-1, ""),
            "Ошибка (код -1): yt-dlp завершился с ошибкой без подробностей"
        );
    }

    /// Список обложек, снятый с настоящего ответа `yt-dlp -J` (YouTube,
    /// dQw4w9WgXcQ) и сокращённый по числу записей, но не по их устройству.
    ///
    /// Важно здесь ровно то, что в выдуманном образце угадать не выйдет:
    /// **у большинства записей размеров нет вовсе** (все `.webp` и все
    /// пронумерованные), один и тот же `hqdefault.jpg` объявлен пять раз
    /// с разными размерами, а поле `thumbnail` указывает на самый тяжёлый
    /// вариант. Дробная ширина у `mqdefault` — от других экстракторов: на
    /// `as_u64` такая запись молча теряется.
    const REAL_THUMBNAILS: &str = r#"{
        "title":"Ролик",
        "thumbnail":"https://i.ytimg.com/vi/abc/maxresdefault.jpg",
        "thumbnails":[
            {"url":"https://i.ytimg.com/vi/abc/3.jpg","preference":-37,"id":"0"},
            {"url":"https://i.ytimg.com/vi_webp/abc/mq3.webp","preference":-34,"id":"3"},
            {"url":"https://i.ytimg.com/vi/abc/default.jpg","height":90,"width":120,"id":"12"},
            {"url":"https://i.ytimg.com/vi/abc/mqdefault.jpg","height":180,"width":320.0,"id":"14"},
            {"url":"https://i.ytimg.com/vi_webp/abc/mqdefault.webp","id":"15"},
            {"url":"https://i.ytimg.com/vi/abc/hqdefault.jpg","height":94,"width":168,"id":"17"},
            {"url":"https://i.ytimg.com/vi/abc/hqdefault.jpg","height":188,"width":336,"id":"20"},
            {"url":"https://i.ytimg.com/vi/abc/hqdefault.jpg","height":360,"width":480,"id":"21"},
            {"url":"https://i.ytimg.com/vi/abc/sddefault.jpg","height":480,"width":640,"id":"23"},
            {"url":"https://i.ytimg.com/vi/abc/hq720.jpg","id":"25"},
            {"url":"https://i.ytimg.com/vi/abc/maxresdefault.jpg","height":1080,"width":1920,"id":"27"},
            {"url":"https://i.ytimg.com/vi_webp/abc/maxresdefault.webp","id":"28"}
        ]
    }"#;

    /// Из настоящего ответа YouTube обязан выбраться `mqdefault`.
    ///
    /// Он единственный дешёвый вариант без чёрных полос: `hqdefault` (480×360)
    /// и `sddefault` (640×480) — это 4:3, и полосы у них **впечатаны в саму
    /// картинку**. Проверено на живом ответе, потому что по одному только JSON
    /// этого не видно: поля `width` и `height` там честные, а то, что часть
    /// кадра занимает чернота, в них не написано.
    ///
    /// Второй промах, который стережёт этот тест, — `maxresdefault`: на него
    /// указывает поле `thumbnail`, и взять его проще всего. Ошибка была бы
    /// незаметной (картинка та же), но стоила бы сотен килобайт из сети и
    /// восьми мегабайт RGBA после разбора — ради превью шириной 240 точек.
    #[test]
    fn thumbnail_is_the_cheapest_one_without_black_bars() {
        let info = parse_media_info(REAL_THUMBNAILS);
        assert_eq!(
            info.thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/vi/abc/mqdefault.jpg"),
            "выбран не самый дешёвый широкоэкранный вариант"
        );
    }

    #[test]
    fn thumbnail_takes_the_largest_when_all_are_small() {
        // Ни одна не дотягивает до нужной ширины — берём самую крупную,
        // а не отказываемся от превью вовсе: мелкая картинка лучше пустоты.
        let json = r#"{"thumbnails":[
            {"url":"https://x/small.jpg","width":168},
            {"url":"https://x/medium.jpg","width":240},
            {"url":"https://x/tiny.jpg","width":48}
        ]}"#;
        assert_eq!(
            parse_media_info(json).thumbnail_url.as_deref(),
            Some("https://x/medium.jpg")
        );

        // А когда подходящие есть, порядок в списке роли не играет: правило
        // про «самую маленькую из достаточных» не зависит от того, встретилась
        // она первой или последней.
        let json = r#"{"thumbnails":[
            {"url":"https://x/huge.jpg","width":1920},
            {"url":"https://x/fits.jpg","width":320},
            {"url":"https://x/big.jpg","width":640},
            {"url":"https://x/small.jpg","width":120}
        ]}"#;
        assert_eq!(
            parse_media_info(json).thumbnail_url.as_deref(),
            Some("https://x/fits.jpg")
        );
    }

    #[test]
    fn thumbnail_falls_back_to_the_plain_field() {
        // Списка нет вовсе — у части экстракторов так и бывает.
        let json = r#"{"thumbnail":"https://x/cover.jpg"}"#;
        assert_eq!(
            parse_media_info(json).thumbnail_url.as_deref(),
            Some("https://x/cover.jpg")
        );

        // Список есть, но в нём нет ни одного адреса: поле надёжнее пустоты.
        let json = r#"{"thumbnail":"https://x/cover.jpg","thumbnails":[{"width":640}]}"#;
        assert_eq!(
            parse_media_info(json).thumbnail_url.as_deref(),
            Some("https://x/cover.jpg")
        );

        // Размеров не знает никто. Список yt-dlp отсортирован от худшего
        // к лучшему, но поле — это выбор самого yt-dlp, и оно вперёд.
        let json = r#"{"thumbnail":"https://x/cover.jpg","thumbnails":[
            {"url":"https://x/a.jpg"},{"url":"https://x/b.jpg"}
        ]}"#;
        assert_eq!(
            parse_media_info(json).thumbnail_url.as_deref(),
            Some("https://x/cover.jpg")
        );

        // Поля нет — тогда последняя безразмерная, то есть лучшая из них.
        let json = r#"{"thumbnails":[{"url":"https://x/a.jpg"},{"url":"https://x/b.jpg"}]}"#;
        assert_eq!(
            parse_media_info(json).thumbnail_url.as_deref(),
            Some("https://x/b.jpg")
        );
    }

    /// Обложки может не быть, и это законный случай, а не ошибка: превью
    /// просто не появится. Ронять на этом разбор метаданных нельзя.
    #[test]
    fn broken_thumbnails_do_not_break_the_probe() {
        for json in [
            "{}",
            r#"{"thumbnails":[]}"#,
            r#"{"thumbnails":"нет"}"#,
            r#"{"thumbnails":[1,2,3]}"#,
            r#"{"thumbnails":[{"url":""}]}"#,
            r#"{"thumbnail":""}"#,
            r#"{"thumbnail":42}"#,
            "не json",
        ] {
            assert_eq!(
                parse_media_info(json).thumbnail_url,
                None,
                "вход: {json}"
            );
        }
    }

    /// Обложка не должна ничего изменить в разборе остальных полей: список
    /// высот и название приезжают из того же ответа.
    #[test]
    fn thumbnail_does_not_disturb_the_rest_of_the_probe() {
        let info = parse_media_info(REAL_THUMBNAILS);
        assert_eq!(info.title.as_deref(), Some("Ролик"));
        // 180, 360, 480, 720 и 1080 в этом ответе — размеры картинок, а не
        // дорожек: `formats[]` здесь нет, и высотам взяться неоткуда.
        assert!(
            info.heights.is_empty(),
            "высоты обложек попали в качества: {:?}",
            info.heights
        );
    }

    #[test]
    fn media_info_survives_missing_fields() {
        let info = parse_media_info(r#"{"title":"Ролик","uploader":"Автор","duration":75.0}"#);
        assert_eq!(info.title.as_deref(), Some("Ролик"));
        assert_eq!(info.uploader.as_deref(), Some("Автор"));
        assert_eq!(info.duration_secs, Some(75.0));
        // Списка форматов в ответе нет — это не ошибка, просто показывать нечего.
        assert!(info.heights.is_empty());

        // Метаданные — украшение: их отсутствие не должно ничего ломать.
        let empty = parse_media_info("{}");
        assert_eq!(empty.title, None);
        assert_eq!(empty.duration_secs, None);

        let broken = parse_media_info("не json");
        assert_eq!(broken.title, None);
    }

    /// Заготовка `Tools` для проверки аргументов. Пути ненастоящие: ни один
    /// процесс здесь не запускается, важен только состав командной строки.
    fn fake_tools(ffmpeg: bool) -> Tools {
        Tools {
            ytdlp: PathBuf::from("yt-dlp"),
            ffmpeg: ffmpeg.then(|| PathBuf::from("ffmpeg")),
        }
    }

    /// Достаёт значение ключа из собранных аргументов.
    fn value_of<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == key)?;
        args.get(at + 1).map(String::as_str)
    }

    /// Есть ли ключ-переключатель среди аргументов.
    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    /// Аргументы одной загрузки: ссылка здесь любая, проверяется состав
    /// командной строки, а не сама ссылка.
    fn args_with(
        format: Format,
        quality: Quality,
        options: DownloadOptions,
        ffmpeg: bool,
    ) -> Vec<String> {
        let request = Request {
            url: "https://example.com/video".to_owned(),
            format,
            quality,
            options,
        };
        download_args(&request, &PathBuf::from("out"), &fake_tools(ffmpeg))
    }

    fn args_for(format: Format, quality: Quality) -> Vec<String> {
        args_with(format, quality, DownloadOptions::default(), true)
    }

    /// Все три флажка сразу — самый ходовой случай «поставил галочки и забыл».
    fn all_options() -> DownloadOptions {
        DownloadOptions {
            embed_metadata: true,
            embed_thumbnail: true,
            embed_subs: true,
        }
    }

    const EMBED_FLAGS: [&str; 3] = ["--embed-metadata", "--embed-thumbnail", "--embed-subs"];

    /// «Макс.» обязана давать ровно ту строку, что была до появления выбора:
    /// иначе новая возможность молча изменила бы старую.
    #[test]
    fn best_quality_keeps_the_old_arguments() {
        let args = args_for(Format::Mp4, Quality::Best);
        assert_eq!(
            value_of(&args, "-f"),
            Some("bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b")
        );
        assert_eq!(value_of(&args, "--merge-output-format"), Some("mp4"));

        let args = args_for(Format::Mp3, Quality::Best);
        assert_eq!(value_of(&args, "--audio-quality"), Some("0"));
        assert_eq!(value_of(&args, "--audio-format"), Some("mp3"));
        // Звук берётся `-x`, а не отбором формата: `-f` здесь лишний.
        assert_eq!(value_of(&args, "-f"), None);
    }

    #[test]
    fn height_is_pushed_into_every_link_of_the_chain() {
        let args = args_for(Format::Mp4, Quality::P720);
        assert_eq!(
            value_of(&args, "-f"),
            Some(
                "bv*[height<=720][ext=mp4]+ba[ext=m4a]\
                 /bv*[height<=720]+ba\
                 /b[height<=720]\
                 /b"
            )
        );
    }

    /// Главная ловушка задачи: без хвостового `/b` ролик, у которого нет
    /// дорожки нужной высоты, не скачается вовсе — yt-dlp ответит
    /// «Requested format is not available». Ни сборка, ни остальные тесты
    /// этого не увидят, поэтому проверяем каждую ступень отдельно.
    #[test]
    fn every_quality_keeps_the_unrestricted_fallback() {
        for quality in Quality::ALL {
            let args = args_for(Format::Mp4, quality);
            let selector = value_of(&args, "-f").expect("нет -f");
            assert!(
                selector.ends_with("/b"),
                "{quality:?}: цепочка без общего запасного варианта: {selector}"
            );
            // Именно голый `b`, а не `b[height<=…]`: последнее звено обязано
            // остаться без ограничения, иначе запас не работает.
            let last = selector.rsplit('/').next().unwrap_or_default();
            assert_eq!(last, "b", "{quality:?}: последнее звено с ограничением");
        }
    }

    #[test]
    fn audio_quality_follows_the_scale() {
        for (quality, expected) in [
            (Quality::Best, "0"),
            (Quality::P2160, "320K"),
            (Quality::P1080, "192K"),
            (Quality::P480, "96K"),
        ] {
            let args = args_for(Format::Mp3, quality);
            assert_eq!(value_of(&args, "--audio-quality"), Some(expected));
        }
    }

    /// Пока галочки не поставлены, командная строка обязана остаться прежней:
    /// новая возможность не имеет права менять то, что скачивают люди,
    /// ни разу её не включавшие.
    #[test]
    fn nothing_is_embedded_until_asked() {
        for format in [Format::Mp4, Format::Mp3] {
            let args = args_for(format, Quality::Best);
            for flag in EMBED_FLAGS {
                assert!(!has(&args, flag), "{format:?}: непрошеный {flag}");
            }
        }
    }

    #[test]
    fn every_checkbox_adds_its_own_flag() {
        let args = args_with(Format::Mp4, Quality::Best, all_options(), true);
        for flag in EMBED_FLAGS {
            assert!(has(&args, flag), "нет {flag}");
        }

        // По одной галочке за раз: соседние ключи появляться не должны.
        for (options, expected) in [
            (
                DownloadOptions {
                    embed_metadata: true,
                    ..DownloadOptions::default()
                },
                "--embed-metadata",
            ),
            (
                DownloadOptions {
                    embed_thumbnail: true,
                    ..DownloadOptions::default()
                },
                "--embed-thumbnail",
            ),
            (
                DownloadOptions {
                    embed_subs: true,
                    ..DownloadOptions::default()
                },
                "--embed-subs",
            ),
        ] {
            let args = args_with(Format::Mp4, Quality::Best, options, true);
            for flag in EMBED_FLAGS {
                assert_eq!(
                    has(&args, flag),
                    flag == expected,
                    "{options:?}: неверный состав ключей, споткнулись на {flag}"
                );
            }
        }
    }

    /// Главная ловушка задачи. Без ffmpeg yt-dlp с этими ключами скачивает
    /// ролик и **падает** на постобработке с кодом 1 — то есть галочка
    /// превращает удавшуюся загрузку в ошибку, да ещё и оставляет рядом
    /// скачанную обложку. Ни сборка, ни остальные тесты этого не видят.
    #[test]
    fn embedding_is_skipped_without_ffmpeg() {
        for format in [Format::Mp4, Format::Mp3] {
            let args = args_with(format, Quality::Best, all_options(), false);
            for flag in EMBED_FLAGS {
                assert!(
                    !has(&args, flag),
                    "{format:?}: {flag} без ffmpeg — загрузка сорвётся на постобработке"
                );
            }
            // Сама загрузка при этом обязана остаться на месте.
            assert!(has(&args, "https://example.com/video"), "потеряна ссылка");
        }
    }

    /// В MP3 субтитры класть некуда: просить их — значит получить ругань
    /// yt-dlp на ровном месте. Остальные две галочки у звука работают.
    #[test]
    fn audio_does_not_ask_for_subtitles() {
        let args = args_with(Format::Mp3, Quality::Best, all_options(), true);
        assert!(!has(&args, "--embed-subs"), "субтитры запрошены у MP3");
        assert!(has(&args, "--embed-metadata"));
        assert!(has(&args, "--embed-thumbnail"));
    }

    /// `--write-subs` выглядит обязательным спутником `--embed-subs` и первым
    /// просится в цепочку при доработке. На деле загрузку субтитров включает
    /// сам `--embed-subs`, а `--write-subs` только оставляет `.vtt` лежать
    /// рядом с роликом: просили вшить, а не положить рядом.
    #[test]
    fn subtitles_are_embedded_without_leaving_a_file() {
        let args = args_with(Format::Mp4, Quality::Best, all_options(), true);
        assert!(has(&args, "--embed-subs"));
        assert!(!has(&args, "--write-subs"), "рядом с роликом останется .vtt");
        assert!(!has(&args, "--write-auto-subs"));
    }

    /// Субтитры в ответе `-J` — те, что выложил автор. `live_chat` там же:
    /// это запись чата трансляции, и по форме она от дорожки субтитров
    /// не отличается.
    #[test]
    fn subtitles_are_detected_from_the_probe() {
        let cases = [
            (r#"{"subtitles":{"en":[{"ext":"vtt","url":"https://x/en"}]}}"#, true),
            (
                r#"{"subtitles":{"live_chat":[{"ext":"json","url":"https://x/chat"}]}}"#,
                false,
            ),
            (
                r#"{"subtitles":{"live_chat":[{"ext":"json"}],"ru":[{"ext":"vtt"}]}}"#,
                true,
            ),
            // Автоматических субтитров мало: `--embed-subs` их не берёт,
            // и обещать их пользователю нельзя.
            (r#"{"automatic_captions":{"en":[{"ext":"vtt"}]}}"#, false),
            (r#"{"subtitles":{}}"#, false),
            (r#"{"subtitles":{"en":[]}}"#, false),
            (r#"{"subtitles":"нет"}"#, false),
            ("{}", false),
            ("не json", false),
        ];

        for (json, expected) in cases {
            assert_eq!(
                parse_media_info(json).has_subtitles,
                expected,
                "вход: {json}"
            );
        }
    }

    /// Ответ `-J` в том виде, в каком его отдаёт yt-dlp: аудиодорожки без
    /// высоты, раскадровки предпросмотра с высотой и `vcodec: "none"`,
    /// повторы одной высоты в разных контейнерах и дробное `1080.0`.
    const REAL_FORMATS: &str = r#"{
        "title":"Ролик",
        "formats":[
            {"format_id":"sb0","vcodec":"none","acodec":"none","ext":"mhtml","height":180},
            {"format_id":"140","vcodec":"none","acodec":"mp4a.40.2","ext":"m4a"},
            {"format_id":"251","vcodec":"none","acodec":"opus","ext":"webm","height":null},
            {"format_id":"18","vcodec":"avc1.42001E","acodec":"mp4a.40.2","ext":"mp4","height":360},
            {"format_id":"136","vcodec":"avc1.4d401f","acodec":"none","ext":"mp4","height":720},
            {"format_id":"247","vcodec":"vp9","acodec":"none","ext":"webm","height":720},
            {"format_id":"137","vcodec":"avc1.640028","acodec":"none","ext":"mp4","height":1080.0}
        ]
    }"#;

    #[test]
    fn heights_come_from_formats_sorted_and_deduped() {
        let info = parse_media_info(REAL_FORMATS);
        assert_eq!(info.heights, vec![1080, 720, 360]);
        assert_eq!(info.max_height(), Some(1080));
    }

    #[test]
    fn storyboards_do_not_pretend_to_be_video() {
        let info = parse_media_info(REAL_FORMATS);
        // 180 — высота раскадровки предпросмотра, а не дорожки ролика.
        assert!(
            !info.heights.contains(&180),
            "раскадровка попала в список качеств: {:?}",
            info.heights
        );
    }

    #[test]
    fn broken_formats_do_not_break_the_probe() {
        // Не массив, не объекты, высота строкой — всё это должно молча
        // дать пустой список, а не панику.
        for json in [
            r#"{"formats":"нет"}"#,
            r#"{"formats":[]}"#,
            r#"{"formats":[1,2,3]}"#,
            r#"{"formats":[{"height":"720"}]}"#,
            r#"{"formats":[{"height":0},{"height":-1}]}"#,
        ] {
            assert!(parse_media_info(json).heights.is_empty(), "вход: {json}");
        }
    }
}
