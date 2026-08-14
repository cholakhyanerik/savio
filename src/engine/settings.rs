//! Выбор пользователя, переживающий закрытие окна.
//!
//! Формат, ступень качества, папка сохранения и то, что вшивается в файл,
//! выставляются один раз и дальше обычно не меняются: тот, кто качает только
//! MP3 к себе в «Музыку» и всегда вшивает обложку с названием, не должен
//! переключать это при каждом запуске.
//!
//! Живёт в слое движка, а не UI, по той же причине, что и всё остальное здесь:
//! это работа с файловой системой и разбор JSON, а `ui()` не должен ни читать,
//! ни писать. Про egui модуль не знает ничего — наружу торчат только доменные
//! типы из [`crate::model`].
//!
//! **Ни одна неудача этого модуля не является ошибкой приложения.** Нет файла,
//! битый JSON, каталог только для чтения, диск полон — всё это молча приводит
//! к тем же значениям по умолчанию, что были зашиты в UI до появления памяти
//! настроек. Настройки — удобство, и падать или ругаться из-за них нельзя.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::binaries;
use crate::model::{DownloadOptions, Format, Quality};

/// Имя файла в каталоге Savio.
const FILE_NAME: &str = "settings.json";

/// Версия схемы файла.
///
/// Читатель её намеренно не проверяет: каждое поле разбирается независимо и
/// при любой непонятности откатывается к умолчанию, поэтому файл от чужой
/// версии не опасен. Число нужно другому — будущей версии Savio, если та
/// поменяет **смысл** какого-то значения: отличить старую запись от новой
/// иначе будет нечем, а гадать по набору полей — гадание и есть.
const SCHEMA: u64 = 1;

/// Сколько ждать после первого изменения, прежде чем писать на диск.
///
/// Пользователь щёлкает переключателями подряд (формат → качество → папка),
/// и без задержки каждый щелчок стоил бы отдельной записи. Отсчёт идёт от
/// **первого** изменения, а не от последнего: так запись гарантированно
/// случится через полсекунды, а не откладывается бесконечно, пока щелчки идут.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Что помним между запусками.
///
/// Значение по умолчанию обязано совпадать с тем, что UI показывал до
/// появления этого модуля: первый запуск (файла ещё нет) и запуск с битым
/// файлом должны выглядеть одинаково и привычно.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Settings {
    pub format: Format,
    pub quality: Quality,
    /// `None` — папки в файле не было либо её больше нет на диске.
    /// Тогда UI берёт свой обычный каталог загрузок.
    pub out_dir: Option<PathBuf>,
    /// Что вшивать в готовый файл — все четыре флажка разом.
    ///
    /// Доменный тип целиком, а не четыре `bool` рядом: флажки живут вместе,
    /// и разложенные по отдельным полям они разъехались бы с
    /// [`DownloadOptions`] при первом же добавлении пятого.
    pub options: DownloadOptions,
}

/// Читает настройки прошлого запуска.
///
/// Зовётся один раз при старте, до первого кадра: это одно чтение маленького
/// файла, столько же, сколько стоит уже делаемая рядом проверка наличия
/// инструментов.
pub fn load() -> Settings {
    match file_path() {
        Some(path) => load_from(&path),
        None => Settings::default(),
    }
}

/// То же, но из указанного файла: так чтение и запись проверяются тестом
/// на настоящем диске, не трогая настоящие настройки пользователя.
fn load_from(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };

    let mut settings = parse(&text);

    // Папку могли переименовать, удалить или отключить вместе с флешкой.
    // Молча забываем её: сохранять в исчезнувший путь — отказ загрузки на
    // ровном месте, причём такой, который пользователь никак не свяжет с
    // выбором, сделанным неделю назад.
    if settings.out_dir.as_deref().is_some_and(|dir| !dir.is_dir()) {
        settings.out_dir = None;
    }

    settings
}

/// Путь к файлу настроек. `None` — каталога данных на этой машине нет
/// (не заданы ни `LOCALAPPDATA`, ни `HOME`), запоминать некуда.
fn file_path() -> Option<PathBuf> {
    Some(binaries::app_dir()?.join(FILE_NAME))
}

// ---------------------------------------------------------------------------
// Разбор и сборка файла — чистые функции, поэтому и покрыты тестами
// ---------------------------------------------------------------------------

/// Формат строкой. Отдельные короткие метки, а не `Debug`: имена вариантов
/// принадлежат коду и переименовываются рефакторингом, а файл на диске от
/// такого переименования обязан не пострадать.
fn format_token(format: Format) -> &'static str {
    match format {
        Format::Mp4 => "mp4",
        Format::Mp3 => "mp3",
    }
}

fn format_from_token(token: &str) -> Option<Format> {
    match token {
        "mp4" => Some(Format::Mp4),
        "mp3" => Some(Format::Mp3),
        _ => None,
    }
}

/// Ступень качества строкой.
///
/// Число, а не имя варианта: шкала одна на оба формата, и «1080» одинаково
/// честно читается и как высота кадра, и как ступень битрейта. Привязать
/// метку к одному из смыслов значило бы соврать во втором.
fn quality_token(quality: Quality) -> &'static str {
    match quality {
        Quality::Best => "best",
        Quality::P2160 => "2160",
        Quality::P1440 => "1440",
        Quality::P1080 => "1080",
        Quality::P720 => "720",
        Quality::P480 => "480",
    }
}

fn quality_from_token(token: &str) -> Option<Quality> {
    match token {
        "best" => Some(Quality::Best),
        "2160" => Some(Quality::P2160),
        "1440" => Some(Quality::P1440),
        "1080" => Some(Quality::P1080),
        "720" => Some(Quality::P720),
        "480" => Some(Quality::P480),
        _ => None,
    }
}

/// Содержимое файла для текущих настроек.
fn to_json(settings: &Settings) -> String {
    // Флажки пишем всегда, включая выключенные: снятая галочка — такой же
    // выбор человека, как поставленная, а по отсутствию ключа отличить
    // «выключил» от «версия ещё не умела это помнить» было бы нечем.
    let mut value = serde_json::json!({
        "version": SCHEMA,
        "format": format_token(settings.format),
        "quality": quality_token(settings.quality),
        "embed_metadata": settings.options.embed_metadata,
        "embed_thumbnail": settings.options.embed_thumbnail,
        "embed_subs": settings.options.embed_subs,
        "auto_subs": settings.options.auto_subs,
    });

    // Путь кладём только тогда, когда он выражается в UTF-8. Терять запомненную
    // папку из-за экзотического имени обидно, но `display()` тут не годится:
    // он подменяет непреобразуемые байты знаком вопроса, и при следующем
    // запуске мы бы честно сохранили файл в **другой** каталог.
    if let Some(dir) = settings.out_dir.as_deref().and_then(Path::to_str) {
        value["out_dir"] = serde_json::Value::String(dir.to_owned());
    }

    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// Разбирает содержимое файла.
///
/// Каждое поле — само по себе: непонятное значение откатывается к умолчанию,
/// а остальные при этом сохраняются. Файл, испорченный наполовину, не должен
/// стирать целиком то, что в нём ещё читается.
fn parse(text: &str) -> Settings {
    let mut settings = Settings::default();

    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return settings;
    };

    if let Some(format) = value
        .get("format")
        .and_then(serde_json::Value::as_str)
        .and_then(format_from_token)
    {
        settings.format = format;
    }

    if let Some(quality) = value
        .get("quality")
        .and_then(serde_json::Value::as_str)
        .and_then(quality_from_token)
    {
        settings.quality = quality;
    }

    // Пустую строку отбрасываем здесь, а не в `load`: `PathBuf::from("")` —
    // это путь, который существует как значение, но никуда не ведёт, и дальше
    // он превратился бы в сохранение в текущий каталог процесса.
    if let Some(dir) = value
        .get("out_dir")
        .and_then(serde_json::Value::as_str)
        .filter(|dir| !dir.is_empty())
    {
        settings.out_dir = Some(PathBuf::from(dir));
    }

    // Флажки вшивания. Каждый по отдельности и с тем же правилом: нет ключа
    // (файл от версии до 0.20) или в нём не `true`/`false` — остаётся
    // выключенным, то есть ровно то, что Savio показывал раньше.
    let options = &mut settings.options;
    if let Some(on) = flag(&value, "embed_metadata") {
        options.embed_metadata = on;
    }
    if let Some(on) = flag(&value, "embed_thumbnail") {
        options.embed_thumbnail = on;
    }
    if let Some(on) = flag(&value, "embed_subs") {
        options.embed_subs = on;
    }
    if let Some(on) = flag(&value, "auto_subs") {
        options.auto_subs = on;
    }

    settings
}

/// Флажок из файла. `None` — ключа нет или в нём лежит не логическое
/// значение; и то и другое означает «оставить умолчание».
fn flag(value: &serde_json::Value, key: &str) -> Option<bool> {
    value.get(key).and_then(serde_json::Value::as_bool)
}

// ---------------------------------------------------------------------------
// Запись
// ---------------------------------------------------------------------------

enum Job {
    Save(Settings),
    /// Дописать отложенное и завершиться.
    Flush,
}

/// Фоновый писатель настроек.
///
/// Отдельный поток нужен по Правилу 1: запись файла — ввод-вывод, а зовут её
/// из обработчика щелчка, то есть из кадра отрисовки. Поток же ещё и снимает
/// гонку: писатель один, и порядок записей совпадает с порядком щелчков.
/// Пачка отдельных потоков «по потоку на сохранение» этого не гарантирует —
/// два переключения подряд могли бы лечь на диск задом наперёд.
pub struct Saver {
    /// `None` — сохранять некуда (нет каталога данных) либо уже сброшено.
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    /// Запускает писателя. Пока пользователь ничего не менял, поток спит
    /// на `recv` и не стоит ничего.
    ///
    /// Имя не `new` намеренно: конструктор с побочным эффектом в виде потока
    /// лучше называть тем, что он делает.
    pub fn spawn() -> Self {
        match file_path() {
            Some(path) => Self::spawn_to(path),
            // Сохранять некуда — писателя не заводим вовсе, `save` станет
            // пустой операцией.
            None => Self {
                tx: None,
                thread: None,
            },
        }
    }

    fn spawn_to(path: PathBuf) -> Self {
        let (tx, rx) = channel::<Job>();
        let thread = std::thread::spawn(move || {
            loop {
                // Первого изменения ждём сколько угодно долго.
                let mut pending = match rx.recv() {
                    Ok(Job::Save(settings)) => settings,
                    // Отправитель ушёл или просят завершиться: копить было
                    // нечего, писать нечего.
                    Ok(Job::Flush) | Err(_) => return,
                };

                // Дальше — окно дебаунса: держим последнее состояние.
                let deadline = Instant::now() + DEBOUNCE;
                loop {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(wait) {
                        Ok(Job::Save(settings)) => pending = settings,
                        // Закрывают окно — дописываем немедленно, ждать
                        // остаток дебаунса уже незачем.
                        Ok(Job::Flush) | Err(RecvTimeoutError::Disconnected) => {
                            write(&path, &pending);
                            return;
                        }
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }

                write(&path, &pending);
            }
        });

        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    /// Запоминает состояние. Возврат мгновенный: диска здесь не касаемся.
    pub fn save(&self, settings: Settings) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Save(settings));
        }
    }

    /// Дописывает отложенное и останавливает поток.
    ///
    /// Звать обязательно при выходе: eframe после `App::on_exit` вызывает
    /// `std::process::exit(0)`, поэтому `Drop` у приложения — и у писателя
    /// вместе с ним — не выполняется никогда. Флаг «сохранять при закрытии»
    /// на `Drop` собрался бы, прошёл бы тесты и молча не работал.
    ///
    /// Повторный вызов ничего не делает.
    pub fn flush(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Job::Flush);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Записывает файл целиком.
///
/// Через временный файл и переименование: обрыв записи (сняли задачу,
/// выключили питание) оставил бы обрезанный JSON, и следующий запуск молча
/// откатился бы к умолчаниям. `std::fs::rename` заменяет существующий файл
/// одним действием на всех трёх системах.
///
/// Все неудачи глушатся — см. заметку о молчании в шапке модуля.
fn write(path: &Path, settings: &Settings) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }

    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, to_json(settings)).is_err() || std::fs::rename(&tmp, path).is_err() {
        // Недописанный временный файл рядом с настоящим не нужен: он ничего
        // не значит, а места занимает и вопросы вызывает.
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_broken_files_fall_back_to_defaults() {
        // Ровно то, что увидит первый запуск и запуск после порчи файла.
        for text in ["", "   ", "не json вовсе", "{", "[1, 2, 3]", "null"] {
            assert_eq!(parse(text), Settings::default(), "на входе {text:?}");
        }
    }

    #[test]
    fn round_trip_keeps_every_field() {
        let settings = Settings {
            format: Format::Mp3,
            quality: Quality::P720,
            // Обратные слэши в JSON экранируются — путь Windows проверяем
            // именно ради этого.
            out_dir: Some(PathBuf::from(r"C:\Users\Вася\Музыка")),
            options: DownloadOptions {
                embed_metadata: true,
                embed_thumbnail: true,
                embed_subs: false,
                auto_subs: true,
            },
        };
        assert_eq!(parse(&to_json(&settings)), settings);
    }

    #[test]
    fn switched_off_flags_are_written_too() {
        // Снятая галочка обязана пережить перезапуск так же, как
        // поставленная: отсутствие ключа означало бы «этой версии Savio
        // такая настройка ещё неизвестна», и файл врал бы о выборе.
        let json = to_json(&Settings::default());
        for key in ["embed_metadata", "embed_thumbnail", "embed_subs", "auto_subs"] {
            assert!(json.contains(key), "в файле нет {key}");
        }
        assert_eq!(parse(&json), Settings::default());
    }

    #[test]
    fn a_file_without_flags_leaves_them_off() {
        // Файл от версии до 0.20: галочек в нём нет вовсе, и появиться
        // взведёнными они не должны.
        let text = r#"{"version": 1, "format": "mp3", "quality": "720"}"#;
        let settings = parse(text);
        assert_eq!(settings.format, Format::Mp3);
        assert_eq!(settings.options, DownloadOptions::default());
    }

    #[test]
    fn round_trip_without_folder() {
        let settings = Settings {
            format: Format::Mp4,
            quality: Quality::Best,
            out_dir: None,
            options: DownloadOptions::default(),
        };
        let json = to_json(&settings);
        assert!(!json.contains("out_dir"), "пустого пути в файле быть не должно");
        assert_eq!(parse(&json), settings);
    }

    #[test]
    fn unknown_values_do_not_poison_the_rest() {
        // Формат от будущей версии Savio, качество — читаемое: качество
        // обязано уцелеть.
        let text = r#"{"version": 99, "format": "webm", "quality": "480"}"#;
        let settings = parse(text);
        assert_eq!(settings.format, Format::default());
        assert_eq!(settings.quality, Quality::P480);
    }

    #[test]
    fn wrong_types_fall_back_field_by_field() {
        let text = r#"{"format": 4, "quality": ["1080"], "out_dir": true,
                       "embed_metadata": "да", "auto_subs": 1}"#;
        assert_eq!(parse(text), Settings::default());
    }

    #[test]
    fn empty_folder_is_not_a_folder() {
        // `PathBuf::from("")` дальше превратился бы в сохранение в текущий
        // каталог процесса — то есть неизвестно куда.
        let text = r#"{"out_dir": ""}"#;
        assert_eq!(parse(text).out_dir, None);
    }

    /// Свой каталог на диске под каждый тест: настоящие настройки
    /// пользователя трогать нельзя, а `%LOCALAPPDATA%\Savio` — именно они.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("savio-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn saver_writes_what_load_reads_back() {
        let dir = scratch("round-trip");
        // Каталога ещё нет: писатель обязан создать его сам, как при самом
        // первом запуске Savio.
        let path = dir.join(FILE_NAME);

        let settings = Settings {
            format: Format::Mp3,
            quality: Quality::P1080,
            out_dir: Some(dir.clone()),
            options: DownloadOptions {
                embed_thumbnail: true,
                ..DownloadOptions::default()
            },
        };

        let mut saver = Saver::spawn_to(path.clone());
        saver.save(settings.clone());
        // Закрытие окна до истечения дебаунса — обычный случай, и именно он
        // ломался бы, полагайся мы на `Drop`.
        saver.flush();

        assert_eq!(load_from(&path), settings);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "временный файл после записи оставаться не должен"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_change_wins_and_folder_that_vanished_is_forgotten() {
        let dir = scratch("last-wins");
        std::fs::create_dir_all(&dir).expect("временный каталог");
        let path = dir.join(FILE_NAME);

        let gone = dir.join("флешку вынули");

        let mut saver = Saver::spawn_to(path.clone());
        // Три щелчка подряд — на диск обязан лечь последний.
        saver.save(Settings {
            format: Format::Mp4,
            quality: Quality::P480,
            out_dir: None,
            options: DownloadOptions::default(),
        });
        saver.save(Settings {
            format: Format::Mp3,
            quality: Quality::P720,
            out_dir: Some(dir.clone()),
            options: DownloadOptions::default(),
        });
        saver.save(Settings {
            format: Format::Mp3,
            quality: Quality::P2160,
            out_dir: Some(gone),
            options: DownloadOptions {
                embed_metadata: true,
                ..DownloadOptions::default()
            },
        });
        saver.flush();

        let loaded = load_from(&path);
        assert_eq!(loaded.format, Format::Mp3);
        assert_eq!(loaded.quality, Quality::P2160);
        assert!(loaded.options.embed_metadata);
        // Папки на диске нет — UI должен получить `None` и взять свой обычный
        // каталог загрузок, а не пытаться сохранить в никуда.
        assert_eq!(loaded.out_dir, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_saved_means_no_file_at_all() {
        let dir = scratch("untouched");
        let path = dir.join(FILE_NAME);

        // Пользователь ничего не менял: запускаем и закрываем.
        let mut saver = Saver::spawn_to(path.clone());
        saver.flush();

        assert!(!path.exists(), "без единого изменения писать нечего");
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn every_token_survives_a_round_trip() {
        // Стережёт опечатку в таблицах: разъедься `*_token` и `*_from_token`,
        // настройка молча перестала бы запоминаться.
        for format in [Format::Mp4, Format::Mp3] {
            assert_eq!(format_from_token(format_token(format)), Some(format));
        }
        for quality in Quality::ALL {
            assert_eq!(quality_from_token(quality_token(quality)), Some(quality));
        }
    }
}
