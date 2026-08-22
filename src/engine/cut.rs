//! Своя обрезка уже скачанного файла.
//!
//! Второй путь к тому же фрагменту. Первый — ключ `--download-sections`, и там
//! качает не yt-dlp, а запущенный им ffmpeg: одним последовательным потоком,
//! без разбиения на диапазоны, которым обычная загрузка и быстра. На коротком
//! куске это терпимо, а на длинном выигрыш пропадает вовсе — замеры записаны
//! у [`crate::model::Section::plan`], там же и правило выбора.
//!
//! Здесь — вторая половина: ролик уже скачан целиком обычным быстрым путём,
//! осталось вырезать кусок на диске и подменить им исходный файл.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use super::{Control, wait_and_release};
use crate::model::{Event, Section};

/// Чем кончилась обрезка.
///
/// Три исхода, а не `Result`: у отмены и у неудачи разное продолжение.
/// Отменённая обрезка не оставляет после себя ничего и не шлёт ни `Done`,
/// ни `Failed` (Правило 2 — UI показывает «Отменено»), а неудавшаяся отдаёт
/// человеку скачанный ролик целиком, потому что это лучшее, что у нас есть.
pub(super) enum Outcome {
    /// Кусок вырезан, файл по прежнему пути — уже он.
    Done,
    /// Отменили: ни фрагмента, ни ролика на диске не осталось.
    Cancelled,
    /// Вырезать не вышло, и вот почему. Ролик остался целиком.
    Failed(String),
}

/// Куда складываем кусок, пока он не готов.
///
/// Рядом с исходным файлом, а не во временном каталоге системы: тот бывает на
/// другом диске, и тогда `rename` в конце превратился бы в копирование сотен
/// мегабайт. Приметное `savio-cut` в имени — чтобы недорезанный огрызок,
/// переживший выключение питания, можно было опознать глазами.
///
/// Расширение сохраняем: по нему ffmpeg выбирает мультиплексор, и `.tmp`
/// вместо `.mp4` кончился бы отказом «Unable to find a suitable output format».
fn temp_path(file: &Path) -> PathBuf {
    let mut name = file.file_stem().unwrap_or_default().to_os_string();
    name.push(".savio-cut");
    if let Some(ext) = file.extension() {
        name.push(".");
        name.push(ext);
    }
    file.with_file_name(name)
}

/// Аргументы ffmpeg: вырезать кусок без пересжатия.
///
/// Обе границы стоят **до** `-i`, и это не стиль. Так они становятся
/// свойствами входа: ffmpeg перематывает файл к нужному месту и дочитывает
/// до нужного, вместо того чтобы прогонять через себя весь ролик и
/// выбрасывать лишнее. На часовой записи разница — секунды против минуты.
///
/// `-to` в этой позиции отсчитывается по шкале **входа**, а не от начала
/// куска, — то есть значит ровно «по такое-то место ролика». Тем же способом
/// режет и сам yt-dlp, и обрезка от этого не меняет смысла: у MP4 начало
/// по-прежнему съезжает к ближайшему ключевому кадру назад (`-c copy`
/// пересжимать не даёт), а конец остаётся там, где просили.
///
/// `-map 0` обязателен: без него ffmpeg берёт по одной дорожке каждого рода
/// и молча выбрасывает всё, что вшила загрузка, — вторую дорожку субтитров,
/// обложку. Ошибки при этом не будет никакой: файл получится, просто беднее.
fn args(input: &Path, output: &Path, section: Section) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        // Ругань ffmpeg уезжает в журнал и в объяснение, поэтому оставляем
        // только её: полный вывод — это ещё и статистика по каждой дорожке.
        "-v".into(),
        "error".into(),
        // Файл под временным именем мог остаться от прошлого раза (питание
        // выключили посреди обрезки). Без `-y` ffmpeg спросил бы про замену
        // и завис бы навсегда: stdin у него закрыт.
        "-y".into(),
    ];

    if let Some(start) = section.start {
        args.push("-ss".into());
        args.push(start.to_string());
    }
    if let Some(end) = section.end {
        args.push("-to".into());
        args.push(end.to_string());
    }

    args.extend([
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        output.to_string_lossy().into_owned(),
    ]);
    args
}

/// Сколько строк ругани ffmpeg показываем человеку — и это **первые** строки,
/// а не последние, в отличие от хвоста stderr у yt-dlp.
///
/// Разница не в небрежности. У yt-dlp внизу стоит итоговое `ERROR:`, а всё
/// выше — ход работы; у ffmpeg с `-v error` ход работы не печатается вовсе,
/// зато первая же строка называет настоящую причину («No such file»,
/// «Invalid data found»), а следующие за ней — её последствия. Возьми мы
/// хвост, человек прочитал бы «Conversion failed!» и ничего больше.
const TAIL_LINES: usize = 4;

/// Вырезает кусок из готового файла и подменяет им исходный.
///
/// Процесс отдаётся в тот же слот `Control`, что и yt-dlp: «Отмена» умеет
/// убивать ровно то, что в нём лежит, и второй ручки заводить не нужно.
pub(super) fn cut(
    ffmpeg: &Path,
    file: &Path,
    section: Section,
    control: &Control,
    tx: &Sender<Event>,
) -> Outcome {
    let temp = temp_path(file);
    let args = args(file, &temp, section);
    let _ = tx.send(Event::Log(format!("ffmpeg {}", args.join(" "))));

    let mut cmd = Command::new(ffmpeg);
    cmd.args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    super::ytdlp::hide_console(&mut cmd);
    control.arm(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return Outcome::Failed(format!("не удалось запустить ffmpeg: {err}")),
    };
    let Some(stderr) = child.stderr.take() else {
        super::kill_and_reap(&mut child);
        return Outcome::Failed("у ffmpeg нет stderr".into());
    };

    // Отменить могли, пока процесс запускался: тогда `adopt` его уже убил.
    match control.adopt(child) {
        Ok(true) => {}
        Ok(false) => return cancelled(file, &temp, tx),
        Err(err) => return Outcome::Failed(err),
    }

    // Читаем на этом потоке, а не после `wait`: с `-v error` ругани обычно
    // нет вовсе, но труба у неё того же размера, что у всех, и переполнись
    // она — ffmpeg встал бы, а мы ждали бы его конца.
    let mut errors: Vec<String> = Vec::new();
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if errors.len() < TAIL_LINES {
            errors.push(line.clone());
        }
        let _ = tx.send(Event::Log(line));
    }

    let Some(status) = wait_and_release(control) else {
        return Outcome::Failed("ffmpeg потерян".into());
    };

    // Отмену спрашиваем прежде кода возврата: убитый процесс возвращает
    // ненулевой код и без этой строки выглядел бы неудачей обрезки.
    if control.cancelled() {
        return cancelled(file, &temp, tx);
    }

    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        let code = status.code().unwrap_or(-1);
        return Outcome::Failed(if errors.is_empty() {
            format!("ffmpeg завершился с ошибкой (код {code})")
        } else {
            format!("ffmpeg завершился с ошибкой (код {code}):\n{}", errors.join("\n"))
        });
    }

    // Код возврата — ещё не доказательство (Правило 6): ffmpeg отвечает
    // успехом и тогда, когда писать было некуда. Спрашиваем сам файл.
    match std::fs::metadata(&temp) {
        Ok(meta) if meta.len() > 0 => {}
        _ => {
            let _ = std::fs::remove_file(&temp);
            return Outcome::Failed(
                "ffmpeg отчитался об успехе, а файла с фрагментом не оставил".into(),
            );
        }
    }

    // `rename` поверх существующего файла на всех трёх системах заменяет его
    // одним движением — отдельного удаления не нужно, и промежутка, в котором
    // не существует ни одного из двух файлов, тоже не возникает.
    if let Err(err) = std::fs::rename(&temp, file) {
        let _ = std::fs::remove_file(&temp);
        return Outcome::Failed(format!("не удалось заменить файл вырезанным куском: {err}"));
    }

    Outcome::Done
}

/// Отмена: убираем и недорезанный кусок, и скачанный целиком ролик.
///
/// Ролик тоже, и это решение, а не небрежность. Просили фрагмент, а целый
/// ролик — наша внутренняя кухня: человек о нём не просил и в папке его не
/// ждёт. Оставить его значило бы повторить ровно ту беду, из-за которой
/// «Отмена» и считается сломанной, — «Отменено» в окне и файл на диске.
fn cancelled(file: &Path, temp: &Path, tx: &Sender<Event>) -> Outcome {
    let _ = std::fs::remove_file(temp);
    if let Err(err) = std::fs::remove_file(file) {
        // Не беда, о которой стоит поднимать баннер: загрузка отменена,
        // а файл человек увидит и уберёт сам. Но сказать надо — иначе
        // непонятно, откуда он взялся.
        let _ = tx.send(Event::Log(format!(
            "Скачанный целиком ролик не удалось убрать после отмены: {err}"
        )));
    }
    Outcome::Cancelled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn section(start: Option<u64>, end: Option<u64>) -> Section {
        Section { start, end }
    }

    /// Временное имя обязано лежать рядом с исходным файлом и сохранять
    /// расширение: по нему ffmpeg выбирает мультиплексор.
    #[test]
    fn the_temporary_name_keeps_the_folder_and_the_extension() {
        let file = Path::new("/дом/видео/Ролик.mp4");
        let temp = temp_path(file);
        assert_eq!(temp.parent(), file.parent());
        assert_eq!(temp.extension().and_then(|e| e.to_str()), Some("mp4"));
        assert_ne!(temp, file);

        // Файл без расширения — законный случай (часть сайтов отдаёт такое).
        let plain = Path::new("/дом/видео/Ролик");
        assert_eq!(temp_path(plain), Path::new("/дом/видео/Ролик.savio-cut"));

        // Точка в названии ролика расширением не является: обрезать надо
        // последнюю, а не первую.
        let dotted = Path::new("/дом/видео/Серия 1. Начало.mp4");
        assert_eq!(
            temp_path(dotted),
            Path::new("/дом/видео/Серия 1. Начало.savio-cut.mp4")
        );
    }

    /// Границы стоят до `-i`, а не после: иначе ffmpeg прогоняет через себя
    /// весь ролик и выбрасывает лишнее — на часовой записи это минута против
    /// секунд. Ни сборка, ни код возврата такого не заметят: файл получится
    /// тот же самый.
    #[test]
    fn the_bounds_go_before_the_input() {
        let args = args(
            Path::new("Ролик.mp4"),
            Path::new("Ролик.savio-cut.mp4"),
            section(Some(90), Some(300)),
        );

        let at = |flag: &str| args.iter().position(|a| a == flag);
        let input = at("-i").expect("нет ключа входного файла");
        assert!(at("-ss").expect("нет начала") < input, "{args:?}");
        assert!(at("-to").expect("нет конца") < input, "{args:?}");

        assert_eq!(args[at("-ss").unwrap() + 1], "90");
        assert_eq!(args[at("-to").unwrap() + 1], "300");
        // Без пересжатия: обрезка не имеет права трогать качество.
        assert_eq!(args[at("-c").expect("нет ключа кодека") + 1], "copy");
        // Все дорожки, а не по одной каждого рода: вшитые субтитры и обложка
        // иначе молча пропадают.
        assert_eq!(args[at("-map").expect("нет ключа дорожек") + 1], "0");
    }

    /// Настоящая обрезка настоящим ffmpeg: файл на месте подменён куском,
    /// временного огрызка не осталось, а сорвавшаяся обрезка исходник
    /// не трогает.
    ///
    /// Сети здесь не нужно — ролик рисует сам ffmpeg (`testsrc`), — но нужен
    /// он сам, поэтому в обычном прогоне тест отключён. Запуск вручную:
    /// `cargo test -- --ignored --nocapture`.
    ///
    /// Проверять это стоит именно живьём: и подмена файла, и то, что кусок
    /// вообще получился, — целиком за пределами того, что видят сборка,
    /// `clippy` и код возврата ffmpeg (Правило 6).
    #[test]
    #[ignore = "требует установленного ffmpeg"]
    fn real_cut_replaces_the_file_and_leaves_no_leftovers() {
        let ffmpeg = super::super::binaries::locate(super::super::binaries::FFMPEG_NAME)
            .expect("ffmpeg обязан быть найден");
        let ffprobe = super::super::binaries::locate(super::super::binaries::FFPROBE_NAME)
            .expect("ffprobe обязан быть найден");

        let dir = std::env::temp_dir().join("savio-cut-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("создать временный каталог");

        // Каждый кадр ключевой (`-g 1`): иначе начало съедет к ближайшему
        // ключевому кадру назад, и проверять длину пришлось бы с запасом,
        // который скрыл бы настоящую ошибку.
        let source = dir.join("Ролик.mp4");
        let made = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=10:size=320x240:rate=10",
                "-c:v",
                "mpeg4",
                "-g",
                "1",
            ])
            .arg(&source)
            .status()
            .expect("запустить ffmpeg");
        assert!(made.success(), "не вышло нарисовать исходный ролик");

        let duration = |file: &Path| -> f64 {
            let out = Command::new(&ffprobe)
                .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
                .arg(file)
                .output()
                .expect("запустить ffprobe");
            String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0.0)
        };
        let whole = duration(&source);
        assert!((9.0..=11.0).contains(&whole), "исходный ролик не тот: {whole}");

        let (tx, rx) = std::sync::mpsc::channel();
        let control = Control::default();
        let outcome = cut(&ffmpeg, &source, section(Some(2), Some(8)), &control, &tx);
        assert!(matches!(outcome, Outcome::Done), "обрезка не удалась");

        let cut_length = duration(&source);
        assert!(
            (5.5..=6.5).contains(&cut_length),
            "вместо шести секунд получилось {cut_length}"
        );

        // Временного файла остаться не должно: он бы лежал в папке
        // сохранения на виду у человека.
        assert!(
            !temp_path(&source).exists(),
            "недорезанный огрызок остался в папке"
        );

        // Отмена, нажатая на стадии обрезки: на диске не должно остаться
        // ни куска, ни скачанного целиком ролика — «Отменено» в окне и файл
        // в папке это ровно то, чем сломана «Отмена» (задача 25 реестра).
        let dropped = dir.join("Отменённый.mp4");
        std::fs::write(&dropped, "ролик").expect("создать файл под отмену");
        let stopped = Control::default();
        stopped.cancelled.store(true, Ordering::Relaxed);
        let outcome = cut(&ffmpeg, &dropped, section(Some(2), Some(8)), &stopped, &tx);
        assert!(
            matches!(outcome, Outcome::Cancelled),
            "отмена не распознана"
        );
        assert!(!dropped.exists(), "после отмены остался скачанный ролик");
        assert!(!temp_path(&dropped).exists(), "после отмены остался огрызок");

        // Сорвавшаяся обрезка обязана оставить исходник в покое: у него
        // человек и заберёт свой ролик целиком.
        const JUNK: &str = "это не видео";
        let junk = dir.join("Не ролик.mp4");
        std::fs::write(&junk, JUNK).expect("создать посторонний файл");
        let outcome = cut(&ffmpeg, &junk, section(Some(2), Some(8)), &control, &tx);
        assert!(
            matches!(outcome, Outcome::Failed(_)),
            "ffmpeg проглотил не видео"
        );
        assert_eq!(
            std::fs::read_to_string(&junk).expect("исходник обязан остаться"),
            JUNK
        );
        assert!(!temp_path(&junk).exists(), "остался огрызок после неудачи");

        drop(rx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Пустая граница — это «с начала» и «до конца», и ключа у неё быть
    /// не должно: `-ss 0` безобиден, а вот `-to 0` вырезал бы пустоту.
    #[test]
    fn an_open_bound_gets_no_flag() {
        let from = args(
            Path::new("Ролик.mp4"),
            Path::new("Ролик.savio-cut.mp4"),
            section(Some(90), None),
        );
        assert!(from.iter().any(|a| a == "-ss"));
        assert!(!from.iter().any(|a| a == "-to"), "{from:?}");

        let till = args(
            Path::new("Ролик.mp4"),
            Path::new("Ролик.savio-cut.mp4"),
            section(None, Some(300)),
        );
        assert!(!till.iter().any(|a| a == "-ss"), "{till:?}");
        assert!(till.iter().any(|a| a == "-to"));
    }
}
