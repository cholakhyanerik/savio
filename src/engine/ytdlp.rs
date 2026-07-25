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
    fn fake_tools() -> Tools {
        Tools {
            ytdlp: PathBuf::from("yt-dlp"),
            ffmpeg: None,
        }
    }

    /// Достаёт значение ключа из собранных аргументов.
    fn value_of<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == key)?;
        args.get(at + 1).map(String::as_str)
    }

    /// Аргументы одной загрузки: ссылка здесь любая, проверяется состав
    /// командной строки, а не сама ссылка.
    fn args_for(format: Format, quality: Quality) -> Vec<String> {
        let request = Request {
            url: "https://example.com/video".to_owned(),
            format,
            quality,
        };
        download_args(&request, &PathBuf::from("out"), &fake_tools())
    }

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
