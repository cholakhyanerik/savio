//! Выбор пользователя, переживающий закрытие окна.
//!
//! Формат, ступень качества, папка сохранения, то, что вшивается в файл, и
//! вход на сайт выставляются один раз и дальше обычно не меняются: тот, кто
//! качает только MP3 к себе в «Музыку» и всегда вшивает обложку с названием,
//! не должен переключать это при каждом запуске.
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

use super::{binaries, weather};
use crate::i18n::Lang;
use crate::model::{
    Appearance, CookieSource, DownloadOptions, FAVORITES_LIMIT, Format, Place, PressureUnit,
    Quality, TempUnit, WeatherUnits, WindUnit,
};

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
///
/// `Eq` у структуры нет с тех пор, как в ней появилось место погоды:
/// координаты — `f64`, а у тех равенство только частичное.
#[derive(Clone, PartialEq, Debug)]
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
    /// Откуда брать вход на сайт.
    ///
    /// До 0.22 не запоминался намеренно: доступ к профилю браузера — не то же
    /// самое, что формат файла. Решение поменяно сознательно (задача 24
    /// реестра), потому что обратная сторона оказалась дороже: тот, кто качает
    /// с закрытого сайта постоянно, выбирал вход при каждом запуске. Само же
    /// опасение никуда не делось, и держат его две вещи в окне, которые
    /// нельзя выбрасывать.
    ///
    /// Первая — жёлтая оговорка под списком, но она внутри «Тонких настроек»,
    /// а те при запуске свёрнуты, так что запомненный вход её не покажет,
    /// пока группу не раскроют. Значит, вся работа при запуске достаётся
    /// второй — словам «вход на сайт» в заголовке свёрнутой группы. Отсюда
    /// требование: заголовок обязан различать включённый вход и выключенный
    /// (задача 51 реестра — сейчас различает хуже, чем хотелось бы). Без
    /// этого запомненный вход становится невидимым, а он хрупкий: у YouTube
    /// cookies ломают загрузку совсем, при открытом браузере база занята.
    pub cookies: CookieSource,
    /// Файл cookies, выбранный для [`CookieSource::File`].
    ///
    /// Пишется независимо от `cookies`: в окне выбранный файл переживает
    /// переключение списка на «Не использовать» и обратно, и обрывать эту
    /// память на закрытии окна было бы странностью без причины.
    ///
    /// `None` — файла не выбирали либо путь не выражается в UTF-8. В отличие
    /// от `out_dir`, пропавший файл здесь **не** забывается, см. `load_from`.
    pub cookie_file: Option<PathBuf>,
    /// Плавные переходы в окне.
    ///
    /// Системного «уменьшить движение» ни eframe, ни winit не сообщают, так
    /// что это единственный способ его выключить, и запоминать его надо тем
    /// же порядком, что формат с качеством: выключают движение один раз и
    /// навсегда, а не при каждом запуске.
    ///
    /// Умолчание — **включено**, и ради него у структуры руками написан
    /// `Default`: производный дал бы `false`, то есть окно без движения
    /// у всех, кто ни о чём не просил, причём молча.
    pub smooth: bool,
    /// Место, для которого показывается погода. `None` — ещё не выбирали:
    /// тогда вкладка при первом открытии определит его по IP.
    ///
    /// Запоминается, чтобы IP-адрес уходил геолокатору один раз, а не при
    /// каждом запуске, — и чтобы город, найденный руками вместо чужого
    /// города VPN, не терялся на закрытии окна.
    pub weather_place: Option<Place>,
    /// Градусы, ветер и давление.
    pub weather_units: WeatherUnits,
    /// Избранные места, не больше [`FAVORITES_LIMIT`].
    pub weather_favorites: Vec<Place>,
    /// Язык интерфейса.
    ///
    /// Запоминается по той же причине, что формат и качество: язык — это
    /// предпочтение человека, а не свойство ссылки, и выставлять его при
    /// каждом запуске он не должен. Умолчание — русский: он был у Savio
    /// единственным, и обновление не имеет права молча сменить язык окна.
    ///
    /// Переключатель при этом стоит **в нижнем блоке рельса**, то есть виден
    /// без единого щелчка на любом разделе; в свёрнутом рельсе он там же,
    /// под кнопкой «⋯». Это требование, а не вкус: запомненная
    /// настройка, спрятанная за щелчком, встречает человека своими
    /// последствиями при запуске, а объяснением — только если он сам полезет
    /// её искать. У языка цена такой ошибки наибольшая: человек, которому окно
    /// открылось на незнакомом алфавите, не прочтёт и подписи «Тонкие
    /// настройки».
    pub lang: Lang,
    /// Тема окна, тёмная или светлая.
    ///
    /// Запоминается по той же причине, что язык: это предпочтение человека,
    /// а не свойство ссылки. Умолчание — тёмная: она была у Savio
    /// единственной, и обновление не имеет права молча перекрасить окно.
    ///
    /// Переключатель стоит там же, где язык, и это требование, а не вкус:
    /// запомненная настройка, спрятанная за щелчком, встречает человека
    /// своими последствиями при запуске, а объяснением — только если он сам
    /// полезет её искать.
    pub appearance: Appearance,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            format: Format::default(),
            quality: Quality::default(),
            out_dir: None,
            options: DownloadOptions::default(),
            cookies: CookieSource::default(),
            cookie_file: None,
            smooth: true,
            weather_place: None,
            weather_units: WeatherUnits::default(),
            weather_favorites: Vec::new(),
            lang: Lang::default(),
            appearance: Appearance::default(),
        }
    }
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

    // А вот пропавший файл cookies здесь намеренно **не** проверяется, хотя
    // соседняя проверка папки так и просит дописать вторую. Умолчания у этих
    // двух разные по цене. Забытая папка откатывается к обычному каталогу
    // загрузок — файл всё равно окажется на диске, и человек его найдёт.
    // Забытый вход откатывается к «Не использовать», то есть к молчаливой
    // загрузке без входа в аккаунт: закрытый ролик после этого отвечает «нужен
    // вход», и чинить пойдут не то. Поэтому путь остаётся как есть, а про
    // пропажу говорит `cookie_file_trouble` — с именем файла и перед началом
    // загрузки, пока она ничего не стоила.

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

/// Источник входа на сайт строкой.
///
/// Своя таблица, а не [`CookieSource::browser`], хотя для семи вариантов из
/// девяти они совпадают буква в букву. Совпадение случайное: те строки —
/// словарь `--cookies-from-browser`, они принадлежат yt-dlp и меняются вместе
/// с ним, а этим на диске лежать годами. Переименуй yt-dlp завтра `chromium`,
/// и общая таблица либо перестала бы работать ключом, либо молча забыла бы
/// выбор у всех, кто им пользовался. Вдобавок `browser()` не различает
/// «не использовать» и «из файла» — оба у него `None`.
fn cookies_token(cookies: CookieSource) -> &'static str {
    match cookies {
        CookieSource::None => "none",
        CookieSource::Chrome => "chrome",
        CookieSource::Edge => "edge",
        CookieSource::Firefox => "firefox",
        CookieSource::Opera => "opera",
        CookieSource::Brave => "brave",
        CookieSource::Vivaldi => "vivaldi",
        CookieSource::Chromium => "chromium",
        CookieSource::File => "file",
    }
}

/// Обратное преобразование. `None` — токена такого нет, и тогда сработает
/// общее правило `parse`: остаётся умолчание, то есть [`CookieSource::None`].
///
/// **Откат обязан вести именно туда, а не к первому браузеру списка.** Файл
/// от будущей версии Savio не должен молча включить чтение чужого профиля —
/// вход включается по просьбе человека, а не по непонятной строке из файла.
fn cookies_from_token(token: &str) -> Option<CookieSource> {
    match token {
        "none" => Some(CookieSource::None),
        "chrome" => Some(CookieSource::Chrome),
        "edge" => Some(CookieSource::Edge),
        "firefox" => Some(CookieSource::Firefox),
        "opera" => Some(CookieSource::Opera),
        "brave" => Some(CookieSource::Brave),
        "vivaldi" => Some(CookieSource::Vivaldi),
        "chromium" => Some(CookieSource::Chromium),
        "file" => Some(CookieSource::File),
        _ => None,
    }
}

/// Тема окна строкой.
fn appearance_token(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => "dark",
        Appearance::Light => "light",
    }
}

fn appearance_from_token(token: &str) -> Option<Appearance> {
    match token {
        "dark" => Some(Appearance::Dark),
        "light" => Some(Appearance::Light),
        _ => None,
    }
}

/// Единицы погоды строками.
///
/// Свои короткие метки, а не подписи из домена: «°C» и «мм рт. ст.»
/// принадлежат интерфейсу и вправе смениться, а файлу на диске лежать годами.
fn temp_token(unit: TempUnit) -> &'static str {
    match unit {
        TempUnit::Celsius => "c",
        TempUnit::Fahrenheit => "f",
    }
}

fn temp_from_token(token: &str) -> Option<TempUnit> {
    match token {
        "c" => Some(TempUnit::Celsius),
        "f" => Some(TempUnit::Fahrenheit),
        _ => None,
    }
}

fn wind_token(unit: WindUnit) -> &'static str {
    match unit {
        WindUnit::MetersPerSecond => "ms",
        WindUnit::KilometersPerHour => "kmh",
    }
}

fn wind_from_token(token: &str) -> Option<WindUnit> {
    match token {
        "ms" => Some(WindUnit::MetersPerSecond),
        "kmh" => Some(WindUnit::KilometersPerHour),
        _ => None,
    }
}

fn pressure_token(unit: PressureUnit) -> &'static str {
    match unit {
        PressureUnit::MmHg => "mmhg",
        PressureUnit::Hectopascal => "hpa",
    }
}

fn pressure_from_token(token: &str) -> Option<PressureUnit> {
    match token {
        "mmhg" => Some(PressureUnit::MmHg),
        "hpa" => Some(PressureUnit::Hectopascal),
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
        // Пишем всегда, включая «none», и по той же причине, что и снятые
        // флажки: «выбрал не использовать» и «версия ещё не умела это
        // помнить» — разные вещи, а по отсутствию ключа их не отличить.
        "cookies": cookies_token(settings.cookies),
        "smooth": settings.smooth,
        // Двухбуквенный код языка, а не имя варианта: имена принадлежат коду
        // и переименовываются рефакторингом, а файлу лежать годами.
        "lang": settings.lang.code(),
        "theme": appearance_token(settings.appearance),
        "weather_units": {
            "temp": temp_token(settings.weather_units.temp),
            "wind": wind_token(settings.weather_units.wind),
            "pressure": pressure_token(settings.weather_units.pressure),
        },
        // Пустой список тоже пишется: «убрал всё из избранного» — выбор,
        // а не отсутствие памяти.
        "weather_favorites": settings
            .weather_favorites
            .iter()
            .take(FAVORITES_LIMIT)
            .map(weather::place_to_json)
            .collect::<Vec<_>>(),
    });

    // Места нет — ключа нет: «ещё не выбирали» и есть умолчание.
    if let Some(place) = &settings.weather_place {
        value["weather_place"] = weather::place_to_json(place);
    }

    // Пути кладём только тогда, когда они выражаются в UTF-8. Терять
    // запомненную папку из-за экзотического имени обидно, но `display()` тут
    // не годится: он подменяет непреобразуемые байты знаком вопроса, и при
    // следующем запуске мы бы честно сохранили файл в **другой** каталог.
    // С файлом cookies промах был бы тише и хуже: yt-dlp не жалуется на
    // несуществующий файл вовсе.
    if let Some(dir) = settings.out_dir.as_deref().and_then(Path::to_str) {
        value["out_dir"] = serde_json::Value::String(dir.to_owned());
    }
    if let Some(file) = settings.cookie_file.as_deref().and_then(Path::to_str) {
        value["cookie_file"] = serde_json::Value::String(file.to_owned());
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

    if let Some(cookies) = value
        .get("cookies")
        .and_then(serde_json::Value::as_str)
        .and_then(cookies_from_token)
    {
        settings.cookies = cookies;
    }

    // Пустую строку отбрасываем по той же причине, что и у папки: путь,
    // который есть, но никуда не ведёт, дальше превратился бы в «файл выбран,
    // но не найден» — оговорку про беду, которой не было.
    if let Some(file) = value
        .get("cookie_file")
        .and_then(serde_json::Value::as_str)
        .filter(|file| !file.is_empty())
    {
        settings.cookie_file = Some(PathBuf::from(file));
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

    // Плавные переходы. Умолчание тут не общее «оставить как было», а
    // осмысленное «включено»: файл от версии до 0.24 ключа не содержит, и
    // прочитать его отсутствие как «выключено» значило бы отнять движение
    // у всех, кто просто обновился.
    if let Some(on) = flag(&value, "smooth") {
        settings.smooth = on;
    }

    // Язык. Незнакомый код откатывается к русскому, как и всё остальное
    // непонятное: файл от будущей версии Savio не должен открыть окно на
    // языке, которого эта сборка не знает, — в ней он вышел бы пустыми
    // подписями.
    if let Some(lang) = value
        .get("lang")
        .and_then(serde_json::Value::as_str)
        .and_then(Lang::from_code)
    {
        settings.lang = lang;
    }

    // Незнакомая тема — та же история: откат к тёмной, а не пустое окно.
    if let Some(appearance) = value
        .get("theme")
        .and_then(serde_json::Value::as_str)
        .and_then(appearance_from_token)
    {
        settings.appearance = appearance;
    }

    // Погода. Каждое поле само по себе, как и всё выше: битое место не
    // стирает единицы, а незнакомая единица — соседние.
    if let Some(place) = value
        .get("weather_place")
        .and_then(weather::place_from_json)
    {
        settings.weather_place = Some(place);
    }
    if let Some(units) = value.get("weather_units") {
        let units_ref = &mut settings.weather_units;
        if let Some(temp) = units
            .get("temp")
            .and_then(serde_json::Value::as_str)
            .and_then(temp_from_token)
        {
            units_ref.temp = temp;
        }
        if let Some(wind) = units
            .get("wind")
            .and_then(serde_json::Value::as_str)
            .and_then(wind_from_token)
        {
            units_ref.wind = wind;
        }
        if let Some(pressure) = units
            .get("pressure")
            .and_then(serde_json::Value::as_str)
            .and_then(pressure_from_token)
        {
            units_ref.pressure = pressure;
        }
    }
    if let Some(list) = value
        .get("weather_favorites")
        .and_then(serde_json::Value::as_array)
    {
        // Битое место выпадает само, уцелевшие остаются. Потолок — и здесь,
        // а не только при записи: файл могли поправить руками.
        settings.weather_favorites = list
            .iter()
            .filter_map(weather::place_from_json)
            .take(FAVORITES_LIMIT)
            .collect();
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
            cookies: CookieSource::File,
            cookie_file: Some(PathBuf::from(r"C:\Users\Вася\cookies.txt")),
            smooth: false,
            weather_place: Some(nizhny()),
            weather_units: WeatherUnits {
                temp: TempUnit::Fahrenheit,
                wind: WindUnit::KilometersPerHour,
                pressure: PressureUnit::Hectopascal,
            },
            weather_favorites: vec![nizhny(), new_york()],
            lang: Lang::Am,
            appearance: Appearance::Light,
        };
        assert_eq!(parse(&to_json(&settings)), settings);
    }

    /// Файл от версии до появления второй темы обязан открываться тёмным.
    ///
    /// Умолчание тут не общее «оставить как было», а осмысленное: тёмная была
    /// у Savio единственной, и обновление не имеет права молча перекрасить
    /// окно у всех, кто просто обновился. Незнакомая тема из файла будущей
    /// версии откатывается туда же.
    #[test]
    fn a_file_without_a_theme_opens_dark() {
        for text in [
            r#"{"version":1,"format":"mp3"}"#,
            r#"{"theme":"solarized"}"#,
            r#"{"theme":""}"#,
            r#"{"theme":42}"#,
            r#"{"theme":null}"#,
        ] {
            assert_eq!(parse(text).appearance, Appearance::Dark, "на входе {text}");
        }
    }

    /// Файл от версии до появления языка обязан открываться по-русски.
    ///
    /// Умолчание тут не общее «оставить как было», а осмысленное: русский был
    /// у Savio единственным языком, и обновление не имеет права молча сменить
    /// язык окна у всех, кто просто обновился.
    #[test]
    fn a_file_without_a_language_opens_in_russian() {
        let text = r#"{"version":1,"format":"mp3"}"#;
        assert_eq!(parse(text).lang, Lang::Ru);
    }

    /// Незнакомый код языка откатывается к русскому, а не к первому попавшемуся.
    ///
    /// Файл от будущей версии Savio не должен открыть окно на языке, которого
    /// эта сборка не знает: в ней он вышел бы пустыми подписями, то есть окном
    /// без единого читаемого слова.
    #[test]
    fn an_unknown_language_falls_back_to_russian() {
        for text in [
            r#"{"lang":"zz"}"#,
            r#"{"lang":""}"#,
            r#"{"lang":42}"#,
            r#"{"lang":null}"#,
        ] {
            assert_eq!(parse(text).lang, Lang::Ru, "на входе {text}");
        }
    }

    /// Выбранный язык переживает закрытие окна — ради этого поле и заведено.
    #[test]
    fn the_chosen_language_survives_a_restart() {
        for lang in Lang::ALL {
            let settings = Settings {
                lang,
                ..Settings::default()
            };
            assert_eq!(parse(&to_json(&settings)).lang, lang, "{lang:?}");
        }
    }

    fn nizhny() -> Place {
        Place {
            name: "Нижний Новгород".to_owned(),
            region: Some("Нижегородская Область".to_owned()),
            country: Some("Россия".to_owned()),
            latitude: 56.32867,
            longitude: 44.00205,
        }
    }

    /// Западное полушарие и место без области: минус в координате и пустой
    /// ключ обязаны пережить запись так же, как всё остальное.
    fn new_york() -> Place {
        Place {
            name: "New York".to_owned(),
            region: None,
            country: None,
            latitude: 40.7128,
            longitude: -74.006,
        }
    }

    #[test]
    fn a_file_without_weather_leaves_it_unset() {
        // Файл от версии до 0.26: погоды в нём нет вовсе. Место остаётся
        // невыбранным — вкладка сама определит его при первом открытии, — а
        // единицы остаются привычными: градусы Цельсия, м/с и мм рт. ст.
        let text = r#"{"version": 1, "format": "mp4", "quality": "best"}"#;
        let settings = parse(text);
        assert_eq!(settings.weather_place, None);
        assert_eq!(settings.weather_units, WeatherUnits::default());
        assert!(settings.weather_favorites.is_empty());
    }

    #[test]
    fn weather_fields_fall_back_one_by_one() {
        // Незнакомая единица не стирает соседние, битое место в избранном —
        // уцелевшие, а место без координат — не место.
        let text = r#"{
            "weather_units": {"temp": "kelvin", "wind": "kmh", "pressure": 5},
            "weather_favorites": [
                {"name": "Нижний Новгород", "region": "Нижегородская Область",
                 "country": "Россия", "latitude": 56.32867, "longitude": 44.00205},
                {"name": "Без координат"},
                "не место"
            ],
            "weather_place": {"name": "Ереван"}
        }"#;
        let settings = parse(text);
        assert_eq!(settings.weather_units.temp, TempUnit::Celsius);
        assert_eq!(settings.weather_units.wind, WindUnit::KilometersPerHour);
        assert_eq!(settings.weather_units.pressure, PressureUnit::MmHg);
        assert_eq!(settings.weather_favorites, vec![nizhny()]);
        assert_eq!(settings.weather_place, None);
    }

    #[test]
    fn favorites_stay_bounded_even_in_a_hand_edited_file() {
        let many: Vec<serde_json::Value> = (0..FAVORITES_LIMIT + 5)
            .map(|i| {
                serde_json::json!({"name": format!("Город {i}"), "latitude": i, "longitude": i})
            })
            .collect();
        let text = serde_json::json!({ "weather_favorites": many }).to_string();
        assert_eq!(parse(&text).weather_favorites.len(), FAVORITES_LIMIT);
    }

    #[test]
    fn a_file_without_the_motion_switch_keeps_transitions_on() {
        // Файл от версии до 0.24: ключа `smooth` в нём нет. Общее правило
        // разбора — «нет ключа, остаётся умолчание», и умолчание тут
        // единственное осмысленное: тот, кто просто обновил Savio, движения
        // не отключал, а молча отнятое движение выглядело бы поломкой.
        let text = r#"{"version": 1, "format": "mp4", "quality": "best"}"#;
        assert!(parse(text).smooth);
    }

    #[test]
    fn switching_motion_off_survives_a_restart() {
        // Выключенные переходы — такой же выбор, как снятая галочка, и
        // отсутствие ключа означало бы «версия ещё не умела это помнить».
        let settings = Settings {
            smooth: false,
            ..Settings::default()
        };
        let json = to_json(&settings);
        assert!(json.contains("smooth"), "в файле нет ключа smooth");
        assert!(
            !parse(&json).smooth,
            "выключенное движение не пережило запись"
        );
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
    fn a_file_without_login_leaves_it_off() {
        // Файл от версии до 0.22: входа в нём нет вовсе, и включиться сам
        // он не должен — иначе обновление Savio начало бы читать профиль
        // браузера у того, кто об этом не просил.
        let text = r#"{"version": 1, "format": "mp4", "quality": "best"}"#;
        let settings = parse(text);
        assert_eq!(settings.cookies, CookieSource::None);
        assert_eq!(settings.cookie_file, None);
    }

    #[test]
    fn an_unknown_login_falls_back_to_none_not_to_a_browser() {
        // Самая дорогая ошибка отката во всём модуле: источник от будущей
        // версии Savio не должен превратиться в первый попавшийся браузер,
        // то есть в незапрошенное чтение чужого профиля.
        for token in [
            r#""safari""#,
            r#""SAFARI""#,
            r#""""#,
            r#""chrome ""#,
            "42",
            "true",
            "null",
            r#"["firefox"]"#,
        ] {
            let text = format!(r#"{{"cookies": {token}}}"#);
            assert_eq!(
                parse(&text).cookies,
                CookieSource::None,
                "на входе {token}"
            );
        }
    }

    #[test]
    fn a_login_from_a_file_survives_without_the_file_itself() {
        // Путь мог не выразиться в UTF-8 — источник от этого не пропадает.
        // В окне это законное состояние: под списком стоит приглашение
        // выбрать файл, а перед загрузкой Savio скажет, что файла нет.
        let text = r#"{"cookies": "file"}"#;
        let settings = parse(text);
        assert_eq!(settings.cookies, CookieSource::File);
        assert_eq!(settings.cookie_file, None);
    }

    #[test]
    fn the_cookie_file_is_kept_even_when_the_login_is_off() {
        // Намеренно, и «прибрать» это тянет: зачем путь, если вход выключен?
        // Затем же, зачем он держится в окне при переключении списка на
        // «Не использовать» и обратно — чтобы не искать тот же файл заново.
        // Обрывать эту память на закрытии окна было бы странностью без
        // причины, а заметить пропажу можно только вернувшись к «Из файла…».
        let settings = Settings {
            cookies: CookieSource::None,
            cookie_file: Some(PathBuf::from("/home/me/cookies.txt")),
            ..Settings::default()
        };
        let json = to_json(&settings);
        assert!(json.contains("cookies.txt"), "путь обязан уехать в файл");
        assert_eq!(parse(&json), settings);
    }

    #[test]
    fn an_empty_cookie_file_is_not_a_file() {
        // `PathBuf::from("")` дальше стал бы «файл выбран, но не найден» —
        // оговоркой про беду, которой не было.
        let text = r#"{"cookies": "file", "cookie_file": ""}"#;
        assert_eq!(parse(text).cookie_file, None);
    }

    #[test]
    fn round_trip_without_folder() {
        let settings = Settings {
            format: Format::Mp4,
            quality: Quality::Best,
            out_dir: None,
            options: DownloadOptions::default(),
            cookies: CookieSource::None,
            cookie_file: None,
            smooth: true,
            ..Settings::default()
        };
        let json = to_json(&settings);
        assert!(!json.contains("out_dir"), "пустого пути в файле быть не должно");
        assert!(
            !json.contains("weather_place"),
            "невыбранному месту погоды в файле места нет"
        );
        assert!(
            !json.contains("cookie_file"),
            "невыбранному файлу cookies в файле места нет"
        );
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
                       "embed_metadata": "да", "auto_subs": 1,
                       "cookies": 7, "cookie_file": ["/tmp/c.txt"]}"#;
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
            lang: Lang::En,
            options: DownloadOptions {
                embed_thumbnail: true,
                ..DownloadOptions::default()
            },
            cookies: CookieSource::Firefox,
            cookie_file: None,
            smooth: false,
            // Место настоящим диском — ради ровно той записи, что уходит
            // в `settings.json`: без него тест не видел бы, что координаты
            // переживают текст и обратно.
            weather_place: Some(nizhny()),
            weather_units: WeatherUnits {
                temp: TempUnit::Celsius,
                wind: WindUnit::KilometersPerHour,
                pressure: PressureUnit::MmHg,
            },
            weather_favorites: vec![new_york()],
            appearance: Appearance::Light,
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
            cookies: CookieSource::None,
            cookie_file: None,
            smooth: true,
            ..Settings::default()
        });
        saver.save(Settings {
            format: Format::Mp3,
            quality: Quality::P720,
            out_dir: Some(dir.clone()),
            options: DownloadOptions::default(),
            cookies: CookieSource::Chrome,
            cookie_file: None,
            smooth: true,
            ..Settings::default()
        });
        saver.save(Settings {
            format: Format::Mp3,
            quality: Quality::P2160,
            out_dir: Some(gone),
            options: DownloadOptions {
                embed_metadata: true,
                ..DownloadOptions::default()
            },
            cookies: CookieSource::Edge,
            cookie_file: None,
            smooth: true,
            ..Settings::default()
        });
        saver.flush();

        let loaded = load_from(&path);
        assert_eq!(loaded.format, Format::Mp3);
        assert_eq!(loaded.quality, Quality::P2160);
        assert!(loaded.options.embed_metadata);
        assert_eq!(loaded.cookies, CookieSource::Edge);
        // Папки на диске нет — UI должен получить `None` и взять свой обычный
        // каталог загрузок, а не пытаться сохранить в никуда.
        assert_eq!(loaded.out_dir, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cookie_file_that_vanished_is_remembered_anyway() {
        // Обратная сторона предыдущего теста, и разница намеренная. Забытая
        // папка откатывается к каталогу загрузок — файл всё равно найдётся.
        // Забытый вход откатился бы к «Не использовать», то есть к молчаливой
        // загрузке без входа в аккаунт, а закрытый ролик после этого отвечает
        // «нужен вход» — и чинить пойдут не то. Поэтому путь остаётся, а про
        // пропажу говорит `cookie_file_trouble` перед загрузкой.
        let dir = scratch("cookie-vanished");
        std::fs::create_dir_all(&dir).expect("временный каталог");
        let path = dir.join(FILE_NAME);

        let gone = dir.join("файл унесли.txt");

        let mut saver = Saver::spawn_to(path.clone());
        saver.save(Settings {
            cookies: CookieSource::File,
            cookie_file: Some(gone.clone()),
            ..Settings::default()
        });
        saver.flush();

        let loaded = load_from(&path);
        assert_eq!(loaded.cookies, CookieSource::File);
        assert_eq!(loaded.cookie_file, Some(gone));

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
        for cookies in CookieSource::ALL {
            assert_eq!(cookies_from_token(cookies_token(cookies)), Some(cookies));
        }
    }
}
