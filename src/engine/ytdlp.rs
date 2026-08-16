//! Сборка аргументов yt-dlp и разбор его вывода.
//!
//! Прогресс читается не из человекочитаемого вывода, а из машинного:
//! `--progress-template` заставляет yt-dlp печатать готовый JSON, который
//! остаётся только распарсить. Скрейпинг обычного вывода ломается при
//! каждом изменении форматирования, поэтому так делать не стоит.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::binaries::Tools;
use crate::model::{
    CookieSource, Format, MediaInfo, Progress, Quality, Request, Section, SubtitlePlan,
    SubtitleTrack,
};

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

pub fn probe_args(url: &str, cookies: CookieSource, cookie_file: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-J".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
    ];
    push_cookies(&mut args, cookies, cookie_file);
    args.push(url.into());
    args
}

/// Ключ входа на сайт: `--cookies-from-browser <браузер>` либо
/// `--cookies <файл>`.
///
/// Общая для `probe_args` и `download_args` намеренно: спрашивать сайт двумя
/// разными личностями нельзя. У закрытого ролика анонимный `-J` проваливается,
/// и человек получил бы пустую карточку — без названия, длительности и обложки —
/// у загрузки, которая на самом деле идёт и заканчивается файлом на диске.
///
/// Двух ключей сразу не бывает: yt-dlp принял бы оба, и какой из двух входов
/// победит, зависело бы от него, а не от того, что выбрал человек.
///
/// **Несуществующий файл ключа не получает.** Не из аккуратности: yt-dlp на
/// пропавший файл не ругается вовсе — проверено вживую (2026.07.04), код
/// возврата 0, ролик скачивается, cookies просто не применяются, и даже без
/// `--no-warnings` в выводе об этом ни слова. То есть человек, у которого
/// файл переименовали, получил бы «видео с возрастным ограничением» вместо
/// «файл cookies не найден». Сказать правду может только Savio, и говорит он
/// её `Event::Warning` из `engine::start`; здесь остаётся не делать вид, что
/// вход передан.
fn push_cookies(args: &mut Vec<String>, cookies: CookieSource, cookie_file: Option<&Path>) {
    if let Some(browser) = cookies.browser() {
        args.push("--cookies-from-browser".into());
        args.push(browser.to_owned());
    } else if cookies == CookieSource::File
        && let Some(file) = cookie_file
    {
        args.push("--cookies".into());
        args.push(file.to_string_lossy().into_owned());
    }
}

/// Ключ, которым передаётся файл cookies. Рядом с разбором в `log_args`:
/// разъедься эти два места — путь пользователя уехал бы в журнал.
const COOKIE_FILE_FLAG: &str = "--cookies";

/// Командная строка для журнала — та же, что уходит в yt-dlp, но без пути
/// к файлу cookies: от него остаётся только имя.
///
/// Журнал человек копирует кнопкой и вкладывает в сообщение о проблеме, а
/// путь к файлу — это его каталоги и его имя пользователя. Имя файла при этом
/// оставляем: диагностика без него теряет смысл (перепутанный файл — самая
/// частая беда этой возможности), а личного в «cookies.txt» нет.
///
/// Сама строка из журнала не убирается и убрана быть не может: на успехе её
/// отправки держится проверка живого приёмника в `engine::run`.
pub fn log_args(args: &[String]) -> String {
    let mut out = String::new();
    let mut hide_next = false;
    for arg in args {
        if !out.is_empty() {
            out.push(' ');
        }
        if hide_next {
            // `file_name()` отдаёт `None` только у пути, кончающегося на
            // `..`, — такого файла не бывает, но подставлять на месте
            // неразобранного пустоту тоже нельзя: строка журнала перестала
            // бы читаться.
            let name = Path::new(arg)
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("файл cookies"));
            out.push('…');
            out.push(std::path::MAIN_SEPARATOR);
            out.push_str(&name.to_string_lossy());
        } else {
            out.push_str(arg);
        }
        hide_next = arg == COOKIE_FILE_FLAG;
    }
    out
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

/// Значение ключа `--download-sections` для запрошенного фрагмента.
/// `None` — фрагмент не просят, и ключа в командной строке быть не должно.
///
/// Две мелочи в этой строке — не вкусовщина, а защита от тихих бед.
///
/// **Звёздочка обязательна.** Без неё yt-dlp считает строку регулярным
/// выражением по названиям глав, ни одной не находит и **не скачивает
/// ничего**: проверено вживую (2026.07.04) — `--download-sections "5-15"`
/// печатает `There are no chapters matching the regex` и выходит с кодом 0.
/// Ни ошибки, ни файла.
///
/// **Границы печатаем числом секунд**, а не в том виде, в каком их набрал
/// человек. Голое число — единственная запись, в которой нельзя ошибиться
/// разделителем, а yt-dlp на непонятом диапазоне отвечает не отказом
/// разобрать, а `usage`-ошибкой на весь экран.
///
/// Открытый конец — `inf`: так его называет сам yt-dlp.
fn section_arg(section: Section) -> Option<String> {
    if !section.any() {
        return None;
    }

    let start = section.start.unwrap_or(0);
    Some(match section.end {
        Some(end) => format!("*{start}-{end}"),
        None => format!("*{start}-inf"),
    })
}

/// Собирает командную строку загрузки.
///
/// `subs` — что решено про субтитры по ответу `probe`
/// (`MediaInfo::subtitle_plan`). Отдельным параметром, а не полем `Request`,
/// потому что взяться ему неоткуда до запроса метаданных: в запросе лежит
/// просьба человека («язык ролика», «можно робота»), а здесь нужен уже
/// разобранный ответ сайта.
pub fn download_args(
    request: &Request,
    out_dir: &Path,
    tools: &Tools,
    subs: &SubtitlePlan,
) -> Vec<String> {
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

            // Автоматические субтитры — распознавание речи и машинный перевод
            // с него.
            //
            // Спрашиваем не галочку, а план: при `--embed-subs` вместе с
            // `--write-auto-subs` yt-dlp вшивает **робота**, даже когда автор
            // выложил субтитры на том же языке. Проверено вживую (2026.07.04):
            // у ролика с обеими дорожками в файл уехало «[Music]» вместо
            // авторского «[♪♪♪]», при коде возврата 0 и без предупреждений.
            // Поэтому ключ добавляется только там, где авторской дорожки нет
            // (`MediaInfo::subtitle_plan`), — иначе новая галочка молча
            // ухудшила бы то, что и так работало.
            if subs.auto {
                args.push("--write-auto-subs".into());
            }

            // **Явный язык обязателен.** Без `--sub-langs` yt-dlp берёт `en`,
            // и это не догадка: проверено вживую (yt-dlp 2026.07.04) на
            // русском ролике — `[info] Downloading subtitles: en`. А `en`
            // у YouTube есть почти всегда, потому что в автоматические
            // субтитры кладётся машинный перевод на полторы сотни языков:
            // в русский ролик уехал бы английский перевод русского же
            // распознавания. Ошибки при этом нет ни в сборке, ни в коде
            // возврата — субтитры появятся, просто не те.
            //
            // `None` бывает, когда язык определить нечем (см.
            // `MediaInfo::subtitle_code`). Тогда ключа нет, и поведение
            // остаётся ровно тем, что было до появления выбора языка.
            if let Some(lang) = &subs.lang {
                args.push("--sub-langs".into());
                args.push(lang.clone());
            }
        }

        // Обрезка лежит на ffmpeg целиком, и без него она не «просто не
        // сработает». Проверено вживую (yt-dlp 2026.07.04): с этим ключом и
        // без ffmpeg загрузка обрывается ещё до начала — `ERROR: You have
        // requested downloading the video partially, but ffmpeg is not
        // installed. Aborting`, код 1, файла нет вовсе. Поэтому ключ живёт
        // здесь, внутри ветки с живым ffmpeg, как и ключи вшивания, а про
        // пропуск говорит `Event::Warning` в `engine::start`.
        if let Some(section) = section_arg(request.section) {
            args.push("--download-sections".into());
            args.push(section);

            // `--force-keyframes-at-cuts` — только для MP3, и это не выбор
            // «точнее или быстрее». Без него у `-x` **молча теряется начало
            // фрагмента**: проверено, диапазоны 5–15 и 10–15 у одного ролика
            // дали одинаковые 15 секунд, считая от нуля, — то есть не тот
            // кусок при коде возврата 0. Вырезанная дорожка приезжает со
            // смещённой шкалой времени, и перекодирование в MP3 отсчитывает
            // её от нуля; ключ заставляет резать заново и точно.
            //
            // У MP4 такой беды нет (5–15 даёт ровно 10 секунд), а ключ там
            // стоил бы полного перекодирования видео — поэтому он не общий.
            // Звук и так перекодируется в MP3, так что для него это даром.
            if request.format == Format::Mp3 {
                args.push("--force-keyframes-at-cuts".into());
            }
        }
    }

    push_cookies(&mut args, request.cookies, request.cookie_file.as_deref());

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
            // Чья это загрузка, знает только тот, кто её запустил: номер
            // проставляет `engine::run` поверх разобранного. Здесь оставляем
            // `NO_DOWNLOAD` из `Default` — разборщик строки про очередь
            // не знает.
            download_id: crate::model::NO_DOWNLOAD,
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
    // Три приметы ниже — про сами cookies, и стоят они выше остальных
    // намеренно: пока браузер не отдал cookies, ни возраста, ни приватности
    // yt-dlp даже не проверял. Все формулировки сняты с живого вывода
    // (yt-dlp 2026.07.04, Windows 11), а не выписаны из документации.
    (
        // «Could not copy Chrome cookie database» — и слово «Chrome» здесь
        // **захардкожено**: ровно этот текст пришёл на выбранный Edge.
        // Поэтому в объяснении браузер не называем.
        //
        // Примета — только «cookie database», без «could not copy»: последнее
        // слишком общее и однажды поймает чужую беду, а совет «закройте
        // браузер» к ней не подойдёт. С «cookies database» из соседней приметы
        // (браузера нет вовсе) эта не пересекается: там «cookies» с «s».
        &["cookie database"],
        "Браузер не отдал cookies: файл занят.\n\n\
         Пока браузер работает, он держит свою базу cookies открытой, и \
         прочитать её нельзя. Закройте браузер полностью — вместе со значком \
         в области уведомлений — и попробуйте снова.",
    ),
    (
        &["decrypt with dpapi", "failed to decrypt cookie"],
        "Этот браузер не отдаёт cookies.\n\n\
         Chrome и браузеры на его основе (Edge, Brave, Opera, Vivaldi) в \
         свежих версиях шифруют cookies так, что снаружи их не прочитать. \
         Это защита самого браузера, обойти её Savio не может. Выходов два: \
         выбрать в списке Mozilla Firefox — его cookies читаются — или \
         выгрузить cookies из этого же браузера расширением вроде «Get \
         cookies.txt» и указать в списке «Из файла…» то, что оно сохранит.",
    ),
    (
        &["cookies database in", "could not find cookies"],
        "В этом браузере cookies не нашлись.\n\n\
         Savio не нашёл его базу cookies: скорее всего браузер не установлен \
         или вы ни разу его не открывали. Выберите тот браузер, в котором \
         открыт нужный сайт.",
    ),
    (
        // Ответ на посторонний файл и, что менее очевидно, на пустой:
        // проверено вживую (yt-dlp 2026.07.04) — оба дают эту же строку
        // и код 1.
        &["does not look like a netscape format cookies file"],
        "Выбранный файл — не файл cookies.\n\n\
         Нужен текстовый файл формата Netscape: такой выгружает расширение \
         браузера, например «Get cookies.txt», кнопкой «Экспорт». Тем же \
         ответом кончается и пустой файл. Выберите в строке под списком \
         другой файл — или верните пункт «Не использовать».",
    ),
    (
        // Примета — имя модуля из питоновского трейсбека, а не «permission
        // denied»: последнее приходит и от папки сохранения, а совет там
        // нужен совсем другой. В хвост из четырёх строк имя попадает
        // (проверено вживую): трейсбек кончается строкой `cookies.py`,
        // самим исключением и сообщением упаковщика.
        &["cookies.py"],
        "В файл cookies не удалось записать.\n\n\
         После работы yt-dlp дописывает в этот файл свежие cookies — и не \
         смог: скорее всего у файла стоит «Только чтение» либо он лежит там, \
         куда писать нельзя. Ролик при этом, скорее всего, уже скачан — \
         загляните в папку сохранения. Снимите с файла защиту от записи или \
         скопируйте его в обычную папку.",
    ),
    (
        &["not a bot"],
        "Сайт требует подтвердить, что вы не робот.\n\n\
         Так отвечают, когда с вашего адреса приходит слишком много запросов. \
         Нажмите «Обновить движок» и попробуйте снова через несколько минут. \
         Если включён VPN — выключите его: одним адресом пользуются многие, \
         и проверка на нём срабатывает чаще. А если вы вошли на этот сайт \
         в браузере — выберите его в списке «Вход на сайт».",
    ),
    (
        &[
            "confirm your age",
            "age-restricted",
            "age restricted",
            "inappropriate for some users",
        ],
        "Видео с возрастным ограничением.\n\n\
         Сайт отдаёт его только тем, кто вошёл в аккаунт. Выберите в списке \
         «Вход на сайт» тот браузер, где вы вошли, — Savio возьмёт вход \
         оттуда. Иногда помогает и «Обновить движок»: свежий yt-dlp обходит \
         часть таких проверок.",
    ),
    (
        &["private video", "is private"],
        "Доступ к видео закрыт.\n\n\
         Владелец сделал его приватным — оно отдаётся только тем, кому он \
         открыл доступ. Если доступ открыт вам, выберите в списке «Вход \
         на сайт» тот браузер, где вы вошли в аккаунт. Иначе остаётся \
         поискать открытую копию по другой ссылке.",
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
///
/// `cookies` нужен ровно одной подсказке и потому в таблицу не убран: пустой
/// список дорожек значит совершенно разное с cookies и без них, и различить
/// эти два случая по хвосту stderr нечем.
pub fn explain_failure(code: i32, tail: &str, cookies: CookieSource) -> String {
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

    // Самая обидная неудача этой возможности, и обнаружить её можно только
    // вживую. Проверено (yt-dlp 2026.07.04, 2026-07-27): YouTube на запрос
    // с cookies из браузера переключается на урезанный ответ плеера, в
    // котором дорожек нет **вовсе**, — ролик, прекрасно скачивавшийся
    // минуту назад, перестаёт скачиваться совсем. Формулировка отказа при
    // этом зависит от того, был ли `-f`: у видео это «Requested format is
    // not available», у MP3 (там `-x` без `-f`) — «No video formats found».
    //
    // Без этой ветки человек получил бы совет из соседней таблицы про
    // качество или сырой английский хвост — и никогда бы не догадался, что
    // виноват список, который он сам только что переключил.
    //
    // Спрашиваем `any()`, а не `browser()`: cookies из файла доезжают до
    // сайта теми же самыми, и урезанный ответ плеера приходит на них так же.
    if cookies.any()
        && (lower.contains("requested format is not available")
            || lower.contains("no video formats found"))
    {
        return "Сайт не отдал ни одной дорожки — похоже, из-за cookies.\n\n\
                Верните в списке «Вход на сайт» пункт «Не использовать» \
                и попробуйте снова. YouTube почти всегда отвечает так на запрос \
                с cookies: он переключается на урезанный ответ, в котором \
                дорожек нет вовсе. Включать cookies стоит только для тех \
                роликов, которые без них не скачиваются."
            .to_owned();
    }

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
        language: text("language").filter(|language| !language.is_empty()),
        subtitles: parse_subtitles(&v),
    }
}

/// Запись чата прошедшей трансляции. YouTube кладёт её в `subtitles` рядом
/// с настоящими дорожками, и по форме ответа она от них неотличима. Без
/// этой проверки у любой записи стрима «субтитры есть» — то есть подсказка
/// соврала бы ровно там, где нужна.
const LIVE_CHAT: &str = "live_chat";

/// Хвост, которым YouTube помечает распознанную дорожку.
///
/// Она лежит в ответе **дважды**: под кодом языка (`ru`) и под `ru-orig`
/// с именем «Russian (Original)» — адрес у обеих один и тот же, `tlang`
/// в нём нет. Проверено на живом ответе `-J`. В списке языков это выглядело
/// бы как два разных пункта, ведущих к одному файлу, поэтому дубль убираем.
const ORIG_SUFFIX: &str = "-orig";

/// Собирает список дорожек субтитров из ответа `-J`.
///
/// Авторские идут первыми, и это не косметика: при совпадении кода языка
/// в списке остаётся первая, то есть авторская. Так же поступает и сам
/// yt-dlp, когда `--embed-subs` и `--write-auto-subs` включены вместе.
fn parse_subtitles(v: &serde_json::Value) -> Vec<SubtitleTrack> {
    let mut tracks = Vec::new();
    collect_subtitles(v.get("subtitles"), false, &mut tracks);
    collect_subtitles(v.get("automatic_captions"), true, &mut tracks);

    // Убираем `<код>-orig` там, где рядом лежит сам `<код>`: это одна и та
    // же дорожка под двумя именами. Коды берём отдельным списком, потому что
    // `retain` не даёт заглянуть в остальной вектор изнутри.
    let codes: Vec<String> = tracks.iter().map(|track| track.code.clone()).collect();
    tracks.retain(|track| {
        track
            .code
            .strip_suffix(ORIG_SUFFIX)
            .is_none_or(|base| !codes.iter().any(|code| code == base))
    });

    tracks
}

/// Перекладывает один раздел ответа (`subtitles` или `automatic_captions`)
/// в общий список.
///
/// Пустой список файлов у языка — не дорожка: экстракторы иногда оставляют
/// такие заготовки, и обещать по ним субтитры нельзя.
fn collect_subtitles(node: Option<&serde_json::Value>, auto: bool, out: &mut Vec<SubtitleTrack>) {
    let Some(languages) = node.and_then(|x| x.as_object()) else {
        return;
    };

    for (code, files) in languages {
        if code == LIVE_CHAT {
            continue;
        }
        let Some(files) = files.as_array().filter(|files| !files.is_empty()) else {
            continue;
        };
        // Первым победил — то есть авторская дорожка выигрывает у
        // автоматической с тем же кодом. Разделы обходятся именно в этом
        // порядке (см. `parse_subtitles`).
        if out.iter().any(|track| track.code == *code) {
            continue;
        }

        // Имя языка даёт сам источник («Russian», «Portuguese (Brazil)»).
        // Переводить его нам нечем: языков полторы сотни, и своя таблица
        // из двух десятков дала бы список наполовину по-русски. Код рядом
        // обязателен: у одного ролика бывает три английские дорожки, и
        // различаются они только кодом (`en`, `en-nP7-2PuUl7o`).
        let name = files
            .iter()
            .find_map(|file| file.get("name").and_then(|x| x.as_str()))
            .filter(|name| !name.is_empty());
        let label = match name {
            Some(name) => format!("{name} · {code}"),
            None => code.clone(),
        };

        out.push(SubtitleTrack {
            code: code.clone(),
            label,
            auto,
        });
    }
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
    use crate::model::{CookieSource, DownloadOptions, SubLang};

    const COOKIE_FLAG: &str = "--cookies-from-browser";

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
        let message = explain_failure(1, tail, CookieSource::None);

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
            let message = explain_failure(1, tail, CookieSource::None);
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
        let message = explain_failure(1, tail, CookieSource::None);
        assert!(message.contains(tail), "подсказка сработала зря: {message}");
    }

    /// Всё, что не опознано, обязано остаться как было: чужую диагностику
    /// лучше показать целиком, чем подменить догадкой.
    #[test]
    fn other_failures_keep_their_output() {
        let tail = "ERROR: unable to rename file: Permission denied";
        let message = explain_failure(1, tail, CookieSource::None);
        assert!(message.contains("код 1"), "{message}");
        assert!(message.contains(tail), "хвост stderr обязан остаться: {message}");

        // Коды с готовой подсказкой и пустой хвост — поведение прежнее.
        assert_eq!(
            explain_failure(101, "", CookieSource::None),
            "Ошибка (код 101): загрузка остановлена (лимит или файл уже есть)"
        );
        assert_eq!(
            explain_failure(2, "", CookieSource::None),
            "Ошибка (код 2): yt-dlp не принял аргументы — это баг Savio"
        );
        assert_eq!(
            explain_failure(-1, "", CookieSource::None),
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
            // Пара всегда докачивается вместе, поэтому в заготовке они тоже
            // появляются и исчезают вдвоём.
            ffprobe: ffmpeg.then(|| PathBuf::from("ffprobe")),
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
        args_full(format, quality, options, ffmpeg, CookieSource::None, None)
    }

    fn args_full(
        format: Format,
        quality: Quality,
        options: DownloadOptions,
        ffmpeg: bool,
        cookies: CookieSource,
        cookie_file: Option<&str>,
    ) -> Vec<String> {
        let request = Request {
            url: "https://example.com/video".to_owned(),
            format,
            quality,
            options,
            cookies,
            cookie_file: cookie_file.map(PathBuf::from),
            section: Section::default(),
            sub_lang: SubLang::default(),
        };
        download_args(
            &request,
            &PathBuf::from("out"),
            &fake_tools(ffmpeg),
            &SubtitlePlan::default(),
        )
    }

    /// Аргументы загрузки с субтитрами: план уже собран по ответу `probe`,
    /// как это делает `engine::run`.
    fn args_subs(format: Format, options: DownloadOptions, plan: &SubtitlePlan) -> Vec<String> {
        let request = Request {
            url: "https://example.com/video".to_owned(),
            format,
            quality: Quality::Best,
            options,
            cookies: CookieSource::None,
            cookie_file: None,
            section: Section::default(),
            sub_lang: SubLang::default(),
        };
        download_args(&request, &PathBuf::from("out"), &fake_tools(true), plan)
    }

    /// План «язык такой-то, робот такой-то» — то, что отдаёт
    /// `MediaInfo::subtitle_plan`.
    fn plan(lang: Option<&str>, auto: bool) -> SubtitlePlan {
        SubtitlePlan {
            lang: lang.map(str::to_owned),
            auto,
        }
    }

    /// Аргументы загрузки с запрошенным фрагментом.
    fn args_section(format: Format, section: Section, ffmpeg: bool) -> Vec<String> {
        let request = Request {
            url: "https://example.com/video".to_owned(),
            format,
            quality: Quality::Best,
            options: DownloadOptions::default(),
            cookies: CookieSource::None,
            cookie_file: None,
            section,
            sub_lang: SubLang::default(),
        };
        download_args(
            &request,
            &PathBuf::from("out"),
            &fake_tools(ffmpeg),
            &SubtitlePlan::default(),
        )
    }

    const SECTION_FLAG: &str = "--download-sections";
    const KEYFRAMES_FLAG: &str = "--force-keyframes-at-cuts";

    fn args_for(format: Format, quality: Quality) -> Vec<String> {
        args_with(format, quality, DownloadOptions::default(), true)
    }

    /// Все три галочки вшивания сразу — самый ходовой случай «поставил
    /// и забыл». Без «можно автоматические»: тесты ниже про вшивание,
    /// а робот — отдельная просьба со своими проверками.
    fn all_options() -> DownloadOptions {
        DownloadOptions {
            embed_metadata: true,
            embed_thumbnail: true,
            embed_subs: true,
            auto_subs: false,
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

    /// Пока фрагмент не просят, командной строке меняться не с чего:
    /// обрезка ведёт загрузку совсем другим путём (медленным, через ffmpeg),
    /// и попасть на него нечаянно — заметная беда.
    #[test]
    fn nothing_is_trimmed_until_asked() {
        for format in [Format::Mp4, Format::Mp3] {
            let args = args_section(format, Section::default(), true);
            assert!(!has(&args, SECTION_FLAG), "{format:?}: непрошеная обрезка");
            assert!(!has(&args, KEYFRAMES_FLAG), "{format:?}: лишний ключ");
        }
    }

    /// Диапазон уходит числом секунд и всегда со звёздочкой. Без неё yt-dlp
    /// ищет главу по регулярному выражению, не находит и молча не скачивает
    /// ничего — при коде возврата 0.
    #[test]
    fn section_is_written_the_way_ytdlp_reads_it() {
        for (section, expected) in [
            (
                Section {
                    start: Some(90),
                    end: Some(240),
                },
                "*90-240",
            ),
            // Открытый конец — «до конца ролика».
            (
                Section {
                    start: Some(90),
                    end: None,
                },
                "*90-inf",
            ),
            // Открытое начало — «с начала»: нижняя граница всё равно нужна.
            (
                Section {
                    start: None,
                    end: Some(240),
                },
                "*0-240",
            ),
        ] {
            let args = args_section(Format::Mp4, section, true);
            assert_eq!(value_of(&args, SECTION_FLAG), Some(expected), "{section:?}");
        }
    }

    /// Без ffmpeg ключ обрезки не просто бесполезен: yt-dlp обрывает загрузку
    /// до её начала («requested downloading the video partially, but ffmpeg is
    /// not installed») и не оставляет файла вовсе. Ролик целиком — плохой
    /// исход, но несравнимо лучше пустой папки и красной «Ошибки».
    #[test]
    fn section_is_skipped_without_ffmpeg() {
        let section = Section {
            start: Some(90),
            end: Some(240),
        };
        for format in [Format::Mp4, Format::Mp3] {
            let args = args_section(format, section, false);
            assert!(
                !has(&args, SECTION_FLAG),
                "{format:?}: обрезка без ffmpeg — загрузка оборвётся, файла не будет"
            );
            assert!(!has(&args, KEYFRAMES_FLAG), "{format:?}: лишний ключ");
            assert!(has(&args, "https://example.com/video"), "потеряна ссылка");
        }
    }

    /// Самая тихая беда этой возможности: у MP3 без `--force-keyframes-at-cuts`
    /// начало фрагмента теряется. Проверено вживую — 5–15 и 10–15 дают
    /// одинаковые 15 секунд от нуля, код возврата 0, файл на месте. У MP4
    /// ключа быть не должно: там он не нужен и стоит перекодирования видео.
    #[test]
    fn audio_cuts_are_forced_to_be_exact() {
        let section = Section {
            start: Some(90),
            end: Some(240),
        };
        let audio = args_section(Format::Mp3, section, true);
        assert!(
            has(&audio, KEYFRAMES_FLAG),
            "у MP3 без этого ключа фрагмент молча начнётся с нуля"
        );

        let video = args_section(Format::Mp4, section, true);
        assert!(
            !has(&video, KEYFRAMES_FLAG),
            "у MP4 ключ лишний: он заставляет перекодировать видео целиком"
        );
        assert!(has(&video, SECTION_FLAG), "потерялась сама обрезка");
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

    /// Свои субтитры в ответе `-J` — те, что выложил автор. `live_chat` там
    /// же: это запись чата трансляции, и по форме она от дорожки субтитров
    /// не отличается.
    #[test]
    fn own_subtitles_are_detected_from_the_probe() {
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
            // Автоматические своими не считаются: их пишет робот, и без
            // отдельной галочки yt-dlp их не берёт.
            (r#"{"automatic_captions":{"en":[{"ext":"vtt"}]}}"#, false),
            (r#"{"subtitles":{}}"#, false),
            (r#"{"subtitles":{"en":[]}}"#, false),
            (r#"{"subtitles":"нет"}"#, false),
            ("{}", false),
            ("не json", false),
        ];

        for (json, expected) in cases {
            assert_eq!(
                parse_media_info(json).subtitle_tracks(false).next().is_some(),
                expected,
                "вход: {json}"
            );
        }
    }

    /// Ответ `-J` в том виде, в каком его отдаёт YouTube на русском ролике:
    /// авторских дорожек нет, распознанная лежит **дважды** (`ru` и
    /// `ru-orig` с одним и тем же адресом), а рядом — машинный перевод на
    /// другие языки. Выдумать такой образец нельзя, он снят с живого ответа
    /// (yt-dlp 2026.07.04); сокращены только 157 языков до трёх.
    const REAL_AUTO_CAPTIONS: &str = r#"{
        "title":"Ролик",
        "language":"ru",
        "subtitles":{},
        "automatic_captions":{
            "af":[{"ext":"vtt","name":"Afrikaans","url":"https://x/af?tlang=af"}],
            "en":[{"ext":"vtt","name":"English","url":"https://x/en?tlang=en"}],
            "ru":[{"ext":"vtt","name":"Russian","url":"https://x/ru"}],
            "ru-orig":[{"ext":"vtt","name":"Russian (Original)","url":"https://x/ru"}],
            "live_chat":[{"ext":"json","name":"Live chat","url":"https://x/chat"}]
        }
    }"#;

    /// Главная ловушка задачи в разборе: «язык ролика» обязан находиться.
    /// Промахнись он — и в русский ролик уехал бы `en`, то есть английский
    /// машинный перевод русского же распознавания, при коде возврата 0.
    #[test]
    fn video_language_and_auto_tracks_come_from_the_probe() {
        let info = parse_media_info(REAL_AUTO_CAPTIONS);

        assert_eq!(info.language.as_deref(), Some("ru"));
        assert_eq!(
            info.subtitle_tracks(false).count(),
            0,
            "у ролика нет авторских дорожек"
        );
        assert_eq!(info.subtitle_code(&SubLang::Original), Some("ru"));

        // `ru-orig` — та же дорожка под вторым именем (адрес совпадает),
        // и в списке ей делать нечего.
        let codes: Vec<&str> = info
            .subtitles
            .iter()
            .map(|track| track.code.as_str())
            .collect();
        assert_eq!(codes, vec!["af", "en", "ru"], "список поехал: {codes:?}");

        // Запись чата — не субтитры.
        assert!(!codes.contains(&"live_chat"));

        // Подпись собрана заранее и несёт код: у одного ролика бывает
        // несколько дорожек одного языка, и различаются они только кодом.
        assert_eq!(info.subtitle_label("ru"), Some("Russian · ru"));
        assert!(info.subtitles.iter().all(|track| track.auto));
    }

    /// Авторская дорожка обязана вытеснить автоматическую с тем же кодом:
    /// человек выкладывал субтитры сам, и они точнее робота. Так же ведёт
    /// себя и yt-dlp, когда оба ключа включены вместе.
    #[test]
    fn authored_tracks_win_over_automatic_ones() {
        let json = r#"{
            "language":"en",
            "subtitles":{"en":[{"ext":"vtt","name":"English"}]},
            "automatic_captions":{
                "en":[{"ext":"vtt","name":"English"}],
                "ru":[{"ext":"vtt","name":"Russian"}]
            }
        }"#;
        let info = parse_media_info(json);

        let en = info
            .subtitles
            .iter()
            .find(|track| track.code == "en")
            .expect("нет английской дорожки");
        assert!(!en.auto, "авторскую дорожку затёрла автоматическая");

        // Авторские идут первыми: на этом держится и приоритет, и заголовки
        // разделов в списке.
        assert_eq!(info.subtitles.first().map(|t| t.code.as_str()), Some("en"));
        assert_eq!(info.subtitle_tracks(false).count(), 1);
        assert_eq!(info.subtitle_tracks(true).count(), 2);
    }

    #[test]
    fn broken_subtitles_do_not_break_the_probe() {
        for json in [
            "{}",
            "не json",
            r#"{"subtitles":"нет","automatic_captions":42}"#,
            r#"{"subtitles":{"en":[]}}"#,
            r#"{"automatic_captions":{"ru":"нет"}}"#,
            // Язык пустой строкой — то же, что отсутствие поля.
            r#"{"language":""}"#,
        ] {
            let info = parse_media_info(json);
            assert!(info.subtitles.is_empty(), "вход: {json}");
            assert_eq!(info.language, None, "вход: {json}");
        }

        // Имени у дорожки может не быть — тогда подписью служит сам код.
        let info = parse_media_info(r#"{"subtitles":{"cs":[{"ext":"vtt"}]}}"#);
        assert_eq!(info.subtitle_label("cs"), Some("cs"));
    }

    // -----------------------------------------------------------------------
    // Автоматические субтитры и их язык
    // -----------------------------------------------------------------------

    const AUTO_FLAG: &str = "--write-auto-subs";
    const LANGS_FLAG: &str = "--sub-langs";

    /// Галочки субтитров: «вшить» и, по желанию, «можно автоматические».
    fn subs_options(auto: bool) -> DownloadOptions {
        DownloadOptions {
            embed_subs: true,
            auto_subs: auto,
            ..DownloadOptions::default()
        }
    }

    /// Главная ловушка задачи: `--write-auto-subs` без явного языка молча
    /// даёт **не те** субтитры. Проверено вживую (yt-dlp 2026.07.04) на
    /// русском ролике: без `--sub-langs` в журнале «Downloading subtitles:
    /// en», и в файл уехал бы английский машинный перевод. Ни сборка, ни код
    /// возврата этого не видят.
    #[test]
    fn automatic_subtitles_never_go_without_a_language() {
        let args = args_subs(Format::Mp4, subs_options(true), &plan(Some("ru"), true));
        assert!(has(&args, AUTO_FLAG), "нет {AUTO_FLAG}");
        assert_eq!(value_of(&args, LANGS_FLAG), Some("ru"), "язык не доехал");

        // Язык определить не вышло — ключа нет вовсе, и yt-dlp ведёт себя
        // ровно как до появления выбора языка. Врать про язык нельзя.
        let args = args_subs(Format::Mp4, subs_options(true), &plan(None, true));
        assert!(has(&args, AUTO_FLAG));
        assert!(!has(&args, LANGS_FLAG), "язык взялся из ниоткуда");
    }

    /// Вторая ловушка, и она обратна ожиданию: `--write-auto-subs` рядом
    /// с `--embed-subs` **вытесняет** авторские субтитры, а не дополняет их.
    /// Поэтому решает не галочка, а план — и когда план говорит «автор есть»,
    /// ключа робота в строке быть не должно.
    #[test]
    fn the_robot_is_not_called_where_the_author_wrote() {
        let args = args_subs(Format::Mp4, subs_options(true), &plan(Some("en"), false));
        assert!(has(&args, "--embed-subs"), "потерялись субтитры целиком");
        assert!(
            !has(&args, AUTO_FLAG),
            "робот вытеснит авторские субтитры — проверено вживую"
        );
        assert_eq!(value_of(&args, LANGS_FLAG), Some("en"));
        // Рядом с роликом по-прежнему ничего не остаётся.
        assert!(!has(&args, "--write-subs"), "рядом останется .vtt");
    }

    /// Пока «можно автоматические» не поставили, командной строке меняться
    /// не с чего — кроме языка, который теперь называется всегда.
    #[test]
    fn automatic_subtitles_are_not_taken_until_asked() {
        let args = args_subs(Format::Mp4, subs_options(false), &plan(Some("ru"), false));
        assert!(has(&args, "--embed-subs"));
        assert!(!has(&args, AUTO_FLAG), "непрошеный робот");
        assert_eq!(value_of(&args, LANGS_FLAG), Some("ru"));

        // Субтитров не просят вовсе — ни одного ключа из этой тройки.
        let args = args_subs(
            Format::Mp4,
            DownloadOptions::default(),
            &plan(Some("ru"), true),
        );
        for flag in ["--embed-subs", AUTO_FLAG, LANGS_FLAG] {
            assert!(!has(&args, flag), "непрошеный {flag}");
        }
    }

    /// В MP3 субтитры класть некуда, и вся тройка ключей туда не едет —
    /// включая язык: без `--embed-subs` он лишний.
    #[test]
    fn audio_takes_no_subtitle_flags_at_all() {
        let args = args_subs(Format::Mp3, subs_options(true), &plan(Some("ru"), true));
        for flag in ["--embed-subs", AUTO_FLAG, LANGS_FLAG] {
            assert!(!has(&args, flag), "у MP3 оказался {flag}");
        }
    }

    /// Без ffmpeg вшивать нечем, и ключи субтитров пропускаются целиком:
    /// иначе удавшаяся загрузка обернулась бы ошибкой на постобработке.
    #[test]
    fn subtitle_flags_are_skipped_without_ffmpeg() {
        let request = Request {
            url: "https://example.com/video".to_owned(),
            format: Format::Mp4,
            quality: Quality::Best,
            options: subs_options(true),
            cookies: CookieSource::None,
            cookie_file: None,
            section: Section::default(),
            sub_lang: SubLang::default(),
        };
        let args = download_args(
            &request,
            &PathBuf::from("out"),
            &fake_tools(false),
            &plan(Some("ru"), true),
        );
        for flag in ["--embed-subs", AUTO_FLAG, LANGS_FLAG] {
            assert!(!has(&args, flag), "{flag} без ffmpeg сорвёт загрузку");
        }
        assert!(has(&args, "https://example.com/video"), "потеряна ссылка");
    }

    /// Ссылка обязана остаться последней: `--sub-langs` берёт следующее
    /// слово как своё значение, и вклинься он между ключом и ссылкой —
    /// качать стали бы язык.
    #[test]
    fn the_url_stays_last_with_subtitle_flags() {
        let args = args_subs(Format::Mp4, subs_options(true), &plan(Some("ru"), true));
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://example.com/video")
        );
    }

    // -----------------------------------------------------------------------
    // Cookies
    // -----------------------------------------------------------------------

    /// Пока браузер не выбран, командная строка обязана остаться прежней —
    /// и у загрузки, и у запроса метаданных. Лишний `--cookies-from-browser`
    /// здесь означал бы чтение чужого профиля браузера без просьбы.
    #[test]
    fn cookies_are_not_requested_until_asked() {
        for format in [Format::Mp4, Format::Mp3] {
            let args = args_for(format, Quality::Best);
            assert!(!has(&args, COOKIE_FLAG), "{format:?}: непрошеные cookies");
        }
        let probe = probe_args("https://example.com/video", CookieSource::None, None);
        assert!(!has(&probe, COOKIE_FLAG), "непрошеные cookies у -J");
        assert!(!has(&probe, COOKIE_FILE_FLAG), "непрошеный файл cookies у -J");
        // Прежний состав `-J` при этом не поехал: ссылка идёт последней.
        assert_eq!(
            probe,
            vec![
                "-J",
                "--no-playlist",
                "--no-warnings",
                "https://example.com/video"
            ]
        );
    }

    /// Выбранный браузер обязан доехать до командной строки — и именно тем
    /// именем, которое понимает yt-dlp.
    #[test]
    fn chosen_browser_reaches_the_command_line() {
        for source in CookieSource::ALL {
            let Some(browser) = source.browser() else {
                continue;
            };
            let args = args_full(
                Format::Mp4,
                Quality::Best,
                DownloadOptions::default(),
                true,
                source,
                None,
            );
            assert_eq!(
                value_of(&args, COOKIE_FLAG),
                Some(browser),
                "{source:?}: браузер не доехал до аргументов"
            );
            // Ссылка обязана остаться последней: `--cookies-from-browser`
            // берёт следующее слово как своё значение, и вклинься он между
            // ключом и ссылкой — качать стали бы браузер.
            assert_eq!(
                args.last().map(String::as_str),
                Some("https://example.com/video"),
                "{source:?}: ссылка перестала быть последней"
            );
        }
    }

    /// Метаданные и сам файл обязаны спрашиваться одной и той же личностью.
    /// Разойдись они — у закрытого ролика `-J` провалится, и человек увидит
    /// пустую карточку у загрузки, которая на самом деле идёт.
    #[test]
    fn probe_asks_with_the_same_cookies_as_the_download() {
        const FILE: &str = "/home/me/cookies.txt";
        for source in CookieSource::ALL {
            let download = args_full(
                Format::Mp4,
                Quality::Best,
                DownloadOptions::default(),
                true,
                source,
                Some(FILE),
            );
            let probe = probe_args(
                "https://example.com/video",
                source,
                Some(Path::new(FILE)),
            );
            for flag in [COOKIE_FLAG, COOKIE_FILE_FLAG] {
                assert_eq!(
                    value_of(&probe, flag),
                    value_of(&download, flag),
                    "{source:?}: `-J` и загрузка спрашивают по-разному ({flag})"
                );
            }
        }
    }

    /// Cookies не должны трогать ничего остального: отбор дорожек, вшивание
    /// и звук остаются теми же, что и без них.
    #[test]
    fn cookies_do_not_disturb_the_rest_of_the_command() {
        let plain = args_with(Format::Mp4, Quality::P720, all_options(), true);
        let with_cookies = args_full(
            Format::Mp4,
            Quality::P720,
            all_options(),
            true,
            CookieSource::Firefox,
            None,
        );
        assert_eq!(value_of(&plain, "-f"), value_of(&with_cookies, "-f"));
        for flag in EMBED_FLAGS {
            assert_eq!(has(&plain, flag), has(&with_cookies, flag), "разъехался {flag}");
        }
        assert_eq!(with_cookies.len(), plain.len() + 2, "лишние аргументы");
    }

    /// Выбранный файл обязан доехать до `--cookies` целым путём — и у
    /// загрузки, и у `-J`. Второй источник входа затевался ради тех, у кого
    /// браузер cookies не отдаёт (DPAPI), так что молчаливая потеря пути
    /// оставила бы их вовсе без входа.
    #[test]
    fn the_chosen_cookie_file_reaches_the_command_line() {
        const FILE: &str = "/home/me/Загрузки/cookies.txt";
        let args = args_full(
            Format::Mp4,
            Quality::Best,
            DownloadOptions::default(),
            true,
            CookieSource::File,
            Some(FILE),
        );
        assert_eq!(value_of(&args, COOKIE_FILE_FLAG), Some(FILE));
        // Браузерного ключа при этом нет: два входа сразу — это выбор
        // за yt-dlp, а не за человеком.
        assert!(!has(&args, COOKIE_FLAG), "два источника входа разом");
        // Ссылка обязана остаться последней: `--cookies` берёт следующее
        // слово как своё значение.
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://example.com/video")
        );
    }

    /// Файл, выбранный при браузере, в командную строку не попадает, и
    /// наоборот. Поля два, а вход один: передай мы оба ключа, какой из них
    /// победит, решал бы yt-dlp.
    #[test]
    fn only_the_chosen_source_reaches_the_command_line() {
        let with_browser = args_full(
            Format::Mp4,
            Quality::Best,
            DownloadOptions::default(),
            true,
            CookieSource::Firefox,
            Some("/home/me/cookies.txt"),
        );
        assert_eq!(value_of(&with_browser, COOKIE_FLAG), Some("firefox"));
        assert!(
            !has(&with_browser, COOKIE_FILE_FLAG),
            "файл уехал вместе с браузером"
        );

        // И зеркально: пункт «не использовать» не оживляет забытый путь.
        let none = args_full(
            Format::Mp4,
            Quality::Best,
            DownloadOptions::default(),
            true,
            CookieSource::None,
            Some("/home/me/cookies.txt"),
        );
        assert!(!has(&none, COOKIE_FILE_FLAG), "непрошеный вход из файла");
    }

    /// «Из файла…» без файла — это отсутствие ключа, а не пустое значение.
    /// Пустая строка после `--cookies` съела бы ссылку: yt-dlp берёт
    /// следующее слово как значение ключа.
    #[test]
    fn a_missing_cookie_file_leaves_no_flag_behind() {
        let args = args_full(
            Format::Mp4,
            Quality::Best,
            DownloadOptions::default(),
            true,
            CookieSource::File,
            None,
        );
        assert!(!has(&args, COOKIE_FILE_FLAG), "ключ без значения");
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://example.com/video"),
            "ссылка перестала быть последней"
        );
    }

    /// Путь к файлу cookies в журнал не уходит: журнал копируют кнопкой и
    /// вкладывают в сообщение о проблеме, а в пути — каталоги и имя
    /// пользователя. Имя файла остаётся: без него по журналу не понять,
    /// тот ли файл выбрали, а личного в «cookies.txt» нет.
    #[test]
    fn the_log_keeps_the_file_name_but_not_the_path() {
        let args = args_full(
            Format::Mp4,
            Quality::Best,
            DownloadOptions::default(),
            true,
            CookieSource::File,
            Some("/home/ivan/секретное/cookies.txt"),
        );
        let line = log_args(&args);

        assert!(!line.contains("ivan"), "имя пользователя утекло: {line}");
        assert!(!line.contains("секретное"), "каталог утёк: {line}");
        assert!(line.contains("cookies.txt"), "потеряно имя файла: {line}");
        assert!(line.contains(COOKIE_FILE_FLAG), "потерян сам ключ: {line}");
        // Остальная командная строка не тронута: журнал остаётся
        // диагностикой, а не пересказом.
        assert!(line.contains("https://example.com/video"), "{line}");
        assert!(line.contains("--merge-output-format mp4"), "{line}");
    }

    /// Без файла cookies строка журнала обязана остаться дословной: правка
    /// ради чужого пути не должна калечить всё остальное.
    #[test]
    fn the_log_is_verbatim_when_there_is_no_cookie_file() {
        let args = args_for(Format::Mp4, Quality::Best);
        assert_eq!(log_args(&args), args.join(" "));
    }

    /// Хвосты сняты с живого вывода yt-dlp 2026.07.04 на Windows 11 —
    /// выдумать их нельзя, а промах подстроки не ловится ничем.
    #[test]
    fn cookie_failures_are_explained_in_russian() {
        let cases = [
            // Браузер работает и держит базу открытой. Слово «Chrome» в этом
            // сообщении захардкожено: выбран был Edge.
            (
                "ERROR: Could not copy Chrome cookie database. See  \
                 https://github.com/yt-dlp/yt-dlp/issues/7271  for more info",
                "Закройте браузер",
            ),
            // Chrome со свежим шифрованием cookies.
            (
                "ERROR: Failed to decrypt with DPAPI. See  \
                 https://github.com/yt-dlp/yt-dlp/issues/10927  for more info",
                "Firefox",
            ),
            // Браузер не установлен.
            (
                r#"ERROR: could not find vivaldi cookies database in "C:\Users\me\AppData\Local\Vivaldi\User Data""#,
                "не нашлись",
            ),
        ];

        for (tail, expected) in cases {
            let message = explain_failure(1, tail, CookieSource::Edge);
            assert!(
                message.contains(expected),
                "нет объяснения «{expected}»: {message}"
            );
            assert!(!message.contains("ERROR"), "утёк сырой вывод: {message}");
        }
    }

    /// Приметы «файл занят» и «браузера нет» не должны срабатывать друг
    /// за друга: разница между ними — одна буква «s» в `cookie(s) database`,
    /// а советы противоположные («закройте браузер» против «выберите другой»).
    #[test]
    fn locked_and_missing_cookie_databases_do_not_swap_hints() {
        let locked = explain_failure(
            1,
            "ERROR: Could not copy Chrome cookie database.",
            CookieSource::Edge,
        );
        let missing = explain_failure(
            1,
            "ERROR: could not find brave cookies database in \"C:\\x\"",
            CookieSource::Brave,
        );
        assert!(locked.contains("Закройте браузер"), "{locked}");
        assert!(!locked.contains("не нашлись"), "{locked}");
        assert!(missing.contains("не нашлись"), "{missing}");
        assert!(!missing.contains("Закройте браузер"), "{missing}");
    }

    /// Беды файла cookies объясняются по-русски и не путаются с бедами
    /// браузера: советы у них разные, а приметы сняты с живого вывода
    /// (yt-dlp 2026.07.04, Windows 11).
    #[test]
    fn cookie_file_failures_are_explained_in_russian() {
        // Посторонний файл — и точно так же пустой: проверено, ответ тот же.
        let garbage = explain_failure(
            1,
            "ERROR: 'C:/Users/me/скачано/заметки.txt' does not look like a \
             Netscape format cookies file",
            CookieSource::File,
        );
        assert!(garbage.contains("не файл cookies"), "{garbage}");
        assert!(garbage.contains("Netscape"), "{garbage}");
        assert!(!garbage.contains("ERROR"), "утёк сырой вывод: {garbage}");

        // Файл только для чтения. yt-dlp дописывает в него cookies после
        // работы и падает питоновским трейсбеком — уже поверх скачанного
        // ролика, поэтому в объяснении про папку сохранения сказано прямо.
        let readonly = explain_failure(
            1,
            "  File \"yt_dlp\\cookies.py\", line 1305, in open\n\
             PermissionError: [Errno 13] Permission denied: 'C:/Users/me/cookies.txt'\n\
             [PYI-2684:ERROR] Failed to execute script '__main__' due to unhandled exception!",
            CookieSource::File,
        );
        assert!(readonly.contains("не удалось записать"), "{readonly}");
        assert!(readonly.contains("папку сохранения"), "{readonly}");
        assert!(
            !readonly.contains("PermissionError"),
            "утёк трейсбек: {readonly}"
        );

        // И советы не перепутаны: у одной беды выбирают другой файл,
        // у другой — снимают защиту от записи.
        assert!(!garbage.contains("Только чтение"), "{garbage}");
        assert!(!readonly.contains("Netscape"), "{readonly}");
    }

    /// Подсказка про пустой список дорожек у YouTube обязана появляться и
    /// для входа из файла: cookies доезжают до сайта теми же самыми. До
    /// появления файла этот вопрос задавался через `browser()`, и на файле
    /// он молча отвечал бы «вход не просили».
    #[test]
    fn the_empty_format_list_hint_knows_about_the_file_login() {
        let tail = "ERROR: [youtube] dQw4w9WgXcQ: Requested format is not available.";
        let from_file = explain_failure(1, tail, CookieSource::File);
        assert!(
            from_file.contains("Не использовать"),
            "нет совета вернуть список: {from_file}"
        );
    }

    /// Главная ловушка задачи, и увидеть её можно только вживую: YouTube на
    /// запрос с cookies отвечает пустым списком дорожек, то есть ролик,
    /// скачивавшийся минуту назад, перестаёт скачиваться совсем. Один и тот
    /// же хвост без cookies значит другое, поэтому объяснение появляется
    /// только тогда, когда браузер выбран.
    #[test]
    fn empty_format_list_blames_cookies_only_when_they_were_used() {
        let tails = [
            "ERROR: [youtube] dQw4w9WgXcQ: Requested format is not available. \
             Use --list-formats for a list of available formats",
            "ERROR: [youtube] jNQXAC9IVRw: No video formats found!; please report \
             this issue on  https://github.com/yt-dlp/yt-dlp/issues?q=",
        ];

        for tail in tails {
            let with_cookies = explain_failure(1, tail, CookieSource::Firefox);
            assert!(
                with_cookies.contains("Не использовать"),
                "нет совета вернуть список: {with_cookies}"
            );
            assert!(!with_cookies.contains("ERROR"), "утёк сырой вывод");

            // Без cookies винить их нельзя: причина совсем другая, и уверенное
            // объяснение не про свою беду хуже английского хвоста.
            let without = explain_failure(1, tail, CookieSource::None);
            assert!(
                without.contains(tail),
                "хвост stderr обязан остаться: {without}"
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
