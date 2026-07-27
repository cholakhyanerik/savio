//! Доменные типы. Ничего не знают ни про UI, ни про yt-dlp.

use std::path::PathBuf;

/// Что именно скачиваем.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    /// Видео со звуком, максимум в пределах MP4-контейнера. Значение по
    /// умолчанию: с ним Savio открывался всегда, и запомненные настройки
    /// откатываются именно к нему.
    #[default]
    Mp4,
    /// Только аудиодорожка, перекодированная в MP3.
    Mp3,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Mp4 => "MP4 — видео",
            Format::Mp3 => "MP3 — аудио",
        }
    }

    /// Подпись поля над переключателем качества.
    ///
    /// Ступени качества у видео и звука общие, а единицы у них разные, и
    /// назвать единицу можно только здесь: на самом сегменте для неё места
    /// нет — их шесть, и в окне шириной 520 каждому достаётся около 70 точек.
    pub fn quality_label(self) -> &'static str {
        match self {
            Format::Mp4 => "Качество",
            Format::Mp3 => "Битрейт, кбит/с",
        }
    }
}

/// Насколько тяжёлый файл берём.
///
/// Отдельный тип рядом с `Format`, а не новые варианты внутри него: формат и
/// качество выбираются независимо, и внутри `Format` их пришлось бы
/// перечислять всеми комбинациями сразу — двенадцатью вместо двух и шести.
///
/// Шкала одна на оба формата, а единица у неё своя: у видео это высота кадра,
/// у звука — битрейт. Ступени идут строго сверху вниз, поэтому переключение
/// MP4 ↔ MP3 не сбивает выбранное положение в списке.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Quality {
    /// Максимум, который отдаёт источник, — ровно то, что Savio делал до
    /// появления выбора. Значение по умолчанию: старое поведение обязано
    /// остаться поведением по умолчанию.
    #[default]
    Best,
    P2160,
    P1440,
    P1080,
    P720,
    P480,
}

impl Quality {
    /// Все ступени сверху вниз — в этом порядке их и рисует переключатель.
    pub const ALL: [Quality; 6] = [
        Quality::Best,
        Quality::P2160,
        Quality::P1440,
        Quality::P1080,
        Quality::P720,
        Quality::P480,
    ];

    /// Потолок высоты кадра. `None` у `Best` — ограничения нет.
    ///
    /// Именно потолок, а не точное значение: у ролика может не быть дорожки
    /// ровно этой высоты, и требовать её означало бы отказать в загрузке.
    pub fn max_height(self) -> Option<u32> {
        match self {
            Quality::Best => None,
            Quality::P2160 => Some(2160),
            Quality::P1440 => Some(1440),
            Quality::P1080 => Some(1080),
            Quality::P720 => Some(720),
            Quality::P480 => Some(480),
        }
    }

    /// Битрейт звука в том виде, в каком его понимает `--audio-quality`.
    ///
    /// `None` у `Best`: там значение другого рода — не битрейт, а верх шкалы
    /// VBR, и подставляет его движок. Смешивать их в одном типе нельзя,
    /// иначе «0» из шкалы VBR однажды уедет в аргумент как «0 кбит/с».
    pub fn audio_bitrate(self) -> Option<&'static str> {
        match self {
            Quality::Best => None,
            Quality::P2160 => Some("320K"),
            Quality::P1440 => Some("256K"),
            Quality::P1080 => Some("192K"),
            Quality::P720 => Some("128K"),
            Quality::P480 => Some("96K"),
        }
    }

    /// Подпись на сегменте. Зависит от формата: одна и та же ступень у видео
    /// значит высоту кадра, у звука — килобиты в секунду.
    ///
    /// Подписи короткие намеренно: шесть сегментов делят ширину окна, а окно
    /// бывает шириной 520 — «Максимум» и «320 кбит/с» распёрли бы дорожку
    /// шире окна. Единицу называет `Format::quality_label` над переключателем.
    pub fn label(self, format: Format) -> &'static str {
        match format {
            Format::Mp4 => match self {
                Quality::Best => "Макс.",
                Quality::P2160 => "2160p",
                Quality::P1440 => "1440p",
                Quality::P1080 => "1080p",
                Quality::P720 => "720p",
                Quality::P480 => "480p",
            },
            Format::Mp3 => match self {
                Quality::Best => "Макс.",
                Quality::P2160 => "320",
                Quality::P1440 => "256",
                Quality::P1080 => "192",
                Quality::P720 => "128",
                Quality::P480 => "96",
            },
        }
    }
}

/// Что за локальный файл выбрал пользователь во вкладке «Метаданные».
///
/// Определяется по расширению, а не по содержимому: пользователь выбирает файл
/// сам, и наша задача — не угадать формат вопреки имени, а честно сказать, с чем
/// мы работать умеем. Настоящую проверку сигнатуры делает уже разборщик:
/// `.jpg` с PNG внутри отвергнет он, а не эта функция.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetaKind {
    Mp3,
    Jpeg,
    Png,
    WebP,
    Gif,
    /// Читать умеем (EXIF лежит на виду), а чистить — нет: метаданные TIFF
    /// это и есть его структура каталогов, и вычистить их можно только
    /// пересобрав файл целиком с пересчётом всех смещений на дорожки данных.
    /// Испортить снимок здесь куда легче, чем вычистить, поэтому честное
    /// «не умею» лучше молчаливой порчи.
    Tiff,
    /// Видео — вне области действия инструмента.
    Video,
    Unsupported,
}

impl MetaKind {
    /// Можно ли у этого файла прочитать метаданные.
    pub fn readable(self) -> bool {
        !matches!(self, MetaKind::Video | MetaKind::Unsupported)
    }

    /// Можно ли у этого файла удалить метаданные.
    pub fn cleanable(self) -> bool {
        self.readable() && !matches!(self, MetaKind::Tiff)
    }
}

/// Определяет тип файла по расширению.
pub fn meta_kind(path: &std::path::Path) -> MetaKind {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return MetaKind::Unsupported;
    };
    // Регистр в расширении произвольный: «IMG_0001.JPG» с телефона — обычное дело.
    let ext = ext.to_ascii_lowercase();

    match ext.as_str() {
        "mp3" => MetaKind::Mp3,
        "jpg" | "jpeg" | "jpe" | "jfif" => MetaKind::Jpeg,
        "png" | "apng" => MetaKind::Png,
        "webp" => MetaKind::WebP,
        "gif" => MetaKind::Gif,
        "tif" | "tiff" => MetaKind::Tiff,
        // Видео перечисляем явно, чтобы отличить «не поддерживаем пока»
        // от «вообще не знаем, что это»: сообщения у них разные.
        "mp4" | "m4v" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "mpg" | "mpeg"
        | "m2ts" | "ts" | "3gp" | "ogv" => MetaKind::Video,
        _ => MetaKind::Unsupported,
    }
}

/// Одна строка в списке метаданных: имя и значение, уже готовые к показу.
///
/// Значение — всегда строка: EXIF-рациональные, GPS-координаты и ID3-теги
/// приводятся к человекочитаемому виду там, где их разбирают. UI ничего
/// не форматирует и не пересобирает в кадре.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub value: String,
}

impl Tag {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Откуда взять cookies для роликов, которые без входа в аккаунт не отдаются.
///
/// Перечисление, а не строка из поля ввода: значение уходит прямо в аргумент
/// `--cookies-from-browser`, и список имён, которые yt-dlp понимает, закрытый.
/// Со свободным вводом опечатка оборачивалась бы английской руганью вместо
/// выбора из того, что заведомо работает.
///
/// Safari в списке нет, хотя yt-dlp его знает, и это не забывчивость. На
/// Windows и Linux он отвечает `unsupported platform` (проверено вживую,
/// yt-dlp 2026.07.04), а на macOS база cookies лежит под защитой системы и
/// читается только приложением с «Полным доступом к диску» — у неподписанного
/// Savio его нет. Пункт, который заведомо не сработает, хуже отсутствующего.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CookieSource {
    /// Не передавать cookies вовсе — ровно то, что Savio делал до появления
    /// этого выбора. Значение по умолчанию и единственное безопасное: читать
    /// чужой профиль браузера мы начинаем только по прямой просьбе.
    #[default]
    None,
    Chrome,
    Edge,
    Firefox,
    Opera,
    Brave,
    Vivaldi,
    Chromium,
}

impl CookieSource {
    /// Все источники в том порядке, в каком их рисует выпадающий список.
    ///
    /// «Не использовать» первым: это и значение по умолчанию, и то, к чему
    /// возвращаются, когда cookies сделали хуже (см. `explain_failure`).
    pub const ALL: [CookieSource; 8] = [
        CookieSource::None,
        CookieSource::Chrome,
        CookieSource::Edge,
        CookieSource::Firefox,
        CookieSource::Opera,
        CookieSource::Brave,
        CookieSource::Vivaldi,
        CookieSource::Chromium,
    ];

    /// Имя браузера в том виде, в каком его понимает `--cookies-from-browser`.
    ///
    /// `None` — cookies не нужны, и ключа в командной строке не будет вовсе.
    /// Имена строчные и без пробелов: yt-dlp сверяет их со своим списком
    /// дословно и на любое другое написание отвечает отказом.
    pub fn browser(self) -> Option<&'static str> {
        match self {
            CookieSource::None => Option::None,
            CookieSource::Chrome => Some("chrome"),
            CookieSource::Edge => Some("edge"),
            CookieSource::Firefox => Some("firefox"),
            CookieSource::Opera => Some("opera"),
            CookieSource::Brave => Some("brave"),
            CookieSource::Vivaldi => Some("vivaldi"),
            CookieSource::Chromium => Some("chromium"),
        }
    }

    /// Подпись в списке. Полные имена, а не токены yt-dlp: в списке человек
    /// ищет свой браузер глазами, и «Mozilla Firefox» узнаётся быстрее, чем
    /// «firefox».
    pub fn label(self) -> &'static str {
        match self {
            CookieSource::None => "Не использовать",
            CookieSource::Chrome => "Google Chrome",
            CookieSource::Edge => "Microsoft Edge",
            CookieSource::Firefox => "Mozilla Firefox",
            CookieSource::Opera => "Opera",
            CookieSource::Brave => "Brave",
            CookieSource::Vivaldi => "Vivaldi",
            CookieSource::Chromium => "Chromium",
        }
    }
}

/// Что дополнительно вшить в готовый файл.
///
/// Плоская структура из трёх флажков, а не варианты перечисления: галочки
/// независимы, и любая их комбинация осмысленна. Значение по умолчанию —
/// все выключены, то есть ровно то, что Savio делал до появления опций.
///
/// Вшивание целиком лежит на `ffmpeg`, и это не деталь реализации, а свойство,
/// от которого зависит поведение: без него запрошенное вшивание не просто
/// не сработает, а **уронит всю загрузку** (подробности — в `download_args`).
/// Отсюда `any()`: спрашивать про ffmpeg надо один раз на все три флажка.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DownloadOptions {
    pub embed_metadata: bool,
    pub embed_thumbnail: bool,
    pub embed_subs: bool,
}

impl DownloadOptions {
    /// Просят ли вшить хоть что-нибудь.
    pub fn any(self) -> bool {
        self.embed_metadata || self.embed_thumbnail || self.embed_subs
    }
}

/// Фрагмент ролика: с какой секунды по какую его вырезать.
///
/// Обе границы независимы и обе необязательны: «с 1:30 и до конца» и «с начала
/// по 4:00» — такие же законные просьбы, как полный диапазон. Обе пустые
/// значат «скачать целиком» — ровно то, что Savio делал до появления обрезки,
/// и потому это значение по умолчанию.
///
/// Секунды, а не набранные строки: разбор — работа домена, и до движка должно
/// доезжать уже проверенное число. Строка «4:00» в аргументе `yt-dlp` была бы
/// не ошибкой, а тихой бедой (см. `section_arg`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Section {
    pub start: Option<u64>,
    pub end: Option<u64>,
}

impl Section {
    /// Просят ли вырезать фрагмент.
    ///
    /// Нужна по той же причине, что и `DownloadOptions::any()`: обрезка целиком
    /// лежит на ffmpeg, и спросить «а есть ли он» надо один раз на обе границы.
    pub fn any(self) -> bool {
        self.start.is_some() || self.end.is_some()
    }
}

/// Почему набранный фрагмент не годится.
///
/// Перечисление, а не готовая строка: под полями показывают текст, а красной
/// рамкой — конкретное поле, и знать, какое именно, можно только отсюда.
/// И оно `Copy`: значение живёт в поле экрана, а собирать `String` на каждое
/// нажатие клавиши незачем.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionError {
    /// Начало не разобралось.
    Start,
    /// Конец не разобрался.
    End,
    /// Конец не позже начала — вырезать нечего.
    Order,
}

impl SectionError {
    /// Что показать под полями.
    pub fn message(self) -> &'static str {
        match self {
            SectionError::Start => {
                "Начало не похоже на время. Нужно «1:30», «1:02:03» или число секунд."
            }
            SectionError::End => {
                "Конец не похож на время. Нужно «4:00», «1:02:03» или число секунд."
            }
            SectionError::Order => "Конец должен быть позже начала.",
        }
    }

    /// Подсвечивать ли поле начала.
    pub fn at_start(self) -> bool {
        matches!(self, SectionError::Start | SectionError::Order)
    }

    /// Подсвечивать ли поле конца. Перевёрнутый диапазон — беда обоих полей
    /// сразу: какое из двух чисел человек имел в виду поправить, мы не знаем.
    pub fn at_end(self) -> bool {
        matches!(self, SectionError::End | SectionError::Order)
    }
}

/// Разбирает «1:30», «1:02:03» или «90» в секунды — обратная к `human_duration`.
///
/// Принимает от одного до трёх полей через двоеточие: секунды, «минуты:секунды»
/// и «часы:минуты:секунды». Всё остальное — `None`. Вытащить из строки первое
/// попавшееся число было бы хуже честного отказа: человек получил бы не тот
/// кусок ролика и узнал бы об этом, только открыв файл.
///
/// Поля после первого ограничены шестьюдесятью: «1:75» — это либо опечатка,
/// либо «минута и 75 секунд», и угадывать, что имелось в виду, не наше дело.
/// У первого поля потолка нет: «90» — законные полторы минуты, а «120:00» —
/// два часа.
pub fn parse_timecode(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let mut secs: u64 = 0;
    let mut fields = 0;
    for field in text.split(':') {
        fields += 1;
        if fields > 3 {
            return None;
        }

        // Проверяем цифры сами, а не полагаемся на `parse`: он принимает
        // ведущий плюс, и «+5» стало бы законным временем.
        let field = field.trim();
        if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: u64 = field.parse().ok()?;
        if fields > 1 && value > 59 {
            return None;
        }

        // `checked_*`, а не `*`: строка приходит из поля ввода, и «999999999999»
        // в часах — это переполнение, то есть паника в отладочной сборке
        // и тихо неверное число в выпускной.
        secs = secs.checked_mul(60)?.checked_add(value)?;
    }

    Some(secs)
}

/// Собирает фрагмент из того, что человек набрал в двух полях.
///
/// Пустое поле — не ошибка, а «границы нет»: оба пустых дают `Section` по
/// умолчанию, то есть загрузку целиком.
///
/// Проверка порядка живёт здесь, до запуска, и это не перестраховка:
/// перевёрнутый диапазон yt-dlp ошибкой не считает — он выходит с кодом 0
/// и отдаёт файл, в котором лежит не то, что просили.
pub fn parse_section(start: &str, end: &str) -> Result<Section, SectionError> {
    let start = parse_bound(start).ok_or(SectionError::Start)?;
    let end = parse_bound(end).ok_or(SectionError::End)?;

    // Пустое начало — это ноль, и сравнивать с ним надо тоже: «до 0:00» —
    // такой же пустой фрагмент, как «с 4:00 по 1:30».
    if let Some(end) = end
        && end <= start.unwrap_or(0)
    {
        return Err(SectionError::Order);
    }

    // «0:00» в поле начала — это и есть начало ролика. Держать его отдельной
    // границей незачем: с ней `any()` отвечал бы «фрагмент просят» на просьбу
    // скачать всё, и целый ролик поехал бы медленным путём обрезки.
    let start = start.filter(|&secs| secs > 0);

    Ok(Section { start, end })
}

/// Одна граница: пусто — `Some(None)`, время — `Some(Some(секунды))`,
/// мусор — `None`. Три исхода, и различать их обязательно: пустое поле
/// и опечатка в нём значат противоположное.
fn parse_bound(text: &str) -> Option<Option<u64>> {
    if text.trim().is_empty() {
        return Some(None);
    }
    parse_timecode(text).map(Some)
}

/// Что именно просят скачать.
///
/// Отдельная структура, а не тройка параметров: формат и качество ходят
/// вместе через движок, поток и сборку аргументов, и каждый новый признак
/// загрузки удлинял бы там четыре сигнатуры разом.
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    pub format: Format,
    pub quality: Quality,
    pub options: DownloadOptions,
    /// Какой кусок ролика нужен. Пустой `Section` — весь целиком.
    ///
    /// Поле запроса, а не флажок внутри `DownloadOptions`: там собрано то,
    /// что вшивается в готовый файл, и на `any()` этой структуры держится
    /// единственная проверка «нужен ли ffmpeg» для вшивания. Обрезке ffmpeg
    /// нужен тоже, но по своей причине и с другим исходом при его нехватке,
    /// так что и спрашивать про неё надо отдельно.
    pub section: Section,
    /// Откуда взять вход в аккаунт. Поле запроса, а не четвёртый флажок внутри
    /// `DownloadOptions`: там собрано то, что вшивается в готовый файл, и
    /// держится на этом `any()` — единственная проверка «нужен ли ffmpeg».
    /// Cookies к ffmpeg отношения не имеют, и попади они в ту же структуру,
    /// первый же, кто честно допишет их в `any()`, начнёт ругаться на
    /// отсутствие ffmpeg там, где он не нужен.
    pub cookies: CookieSource,
}

/// Метаданные ролика, полученные до начала загрузки.
#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub title: Option<String>,
    pub uploader: Option<String>,
    pub duration_secs: Option<f64>,
    /// Высоты кадра, которые источник действительно отдаёт: по убыванию, без
    /// повторов. Пусто — либо видеодорожек нет вовсе, либо экстрактор их
    /// не перечислил; и то и другое законно, показывать тогда просто нечего.
    pub heights: Vec<u32>,
    /// Адрес обложки, если экстрактор её назвал. Сама картинка приезжает
    /// отдельным событием: `-J` отдаёт только ссылку, а тянуть по ней байты —
    /// это ещё один запрос в сеть.
    pub thumbnail_url: Option<String>,
    /// Есть ли у ролика собственные субтитры — те, что выложил автор.
    ///
    /// Нужно ровно для одного: сказать человеку, что вшивать нечего. `false`
    /// значит «мы точно знаем, что их нет»; когда `probe` не прошёл вовсе,
    /// `MediaInfo` до UI не доезжает, и молчать — правильно.
    pub has_subtitles: bool,
}

impl MediaInfo {
    /// Самая большая доступная высота кадра.
    ///
    /// Список отсортирован по убыванию там, где собирается, — здесь только
    /// первый элемент, потому что вызывается это из кадра отрисовки.
    pub fn max_height(&self) -> Option<u32> {
        self.heights.first().copied()
    }
}

/// Обложка ролика, уже разобранная в пиксели.
///
/// Хранится готовым RGBA, а не байтами файла, и это не мелочь: своего
/// разборщика картинок у egui нет, а декодировать JPEG в потоке отрисовки
/// нельзя — он обязан успевать 60 раз в секунду (Правило 1). Поэтому разбор
/// и уменьшение делает движок, а UI остаётся одно движение: залить готовые
/// байты в текстуру.
///
/// Размеры в `usize` не для красоты: `ColorImage::from_rgba_unmultiplied`
/// принимает `[usize; 2]`, и приведение типов пришлось бы делать в UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: usize,
    pub height: usize,
    /// По четыре байта на точку (R, G, B, A), слева направо и сверху вниз.
    pub rgba: Vec<u8>,
}

impl Thumbnail {
    /// Сходятся ли размеры с длиной буфера.
    ///
    /// Проверять обязательно, и вот почему: `ColorImage::from_rgba_unmultiplied`
    /// на несовпадении не возвращает ошибку, а **паникует**. Уронить приложение
    /// из-за украшения — самый обидный из возможных исходов, поэтому UI
    /// спрашивает об этом перед заливкой в текстуру, а движок — сразу после
    /// разбора. Нулевую сторону тоже не пропускаем: текстура нулевого размера
    /// egui не нужна.
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self
                .width
                .checked_mul(self.height)
                .and_then(|pixels| pixels.checked_mul(4))
                == Some(self.rgba.len())
    }
}

/// Состояние текущей загрузки.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub speed_bps: Option<f64>,
    pub eta_secs: Option<u64>,
}

impl Progress {
    /// Доля выполнения, если общий размер известен.
    ///
    /// Для потоковых источников `total` приходит нулём — в этом случае
    /// показывать нечего, и UI рисует неопределённый индикатор.
    pub fn fraction(&self) -> Option<f32> {
        if self.total > 0 {
            Some((self.downloaded as f32 / self.total as f32).clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

/// События, которые движок отдаёт наружу. Единственный канал связи с UI.
///
/// Один и тот же набор обслуживает и загрузку ролика, и установку внешних
/// инструментов при первом запуске: обе задачи описываются одинаково —
/// стадия, прогресс, журнал, исход. Отдельный канал под установку заводить
/// не нужно, иначе движок перестал бы быть пригодным для CLI без правок.
#[derive(Clone, Debug)]
pub enum Event {
    /// Метаданные подъехали.
    Info(MediaInfo),
    /// Обложка ролика разобрана и уменьшена — можно заводить текстуру.
    ///
    /// Отдельно от `Info`, а не полем внутри него: адрес обложки приезжает
    /// вместе с метаданными, а сама картинка — отдельным запросом в сеть, и
    /// ждать её, чтобы отправить всё одним событием, значило бы задержать
    /// название ролика на время загрузки картинки. Неудача этого запроса
    /// ничего не ломает: события просто не будет (см. `engine::run`).
    Thumbnail(Thumbnail),
    /// Сменилась фаза работы — человекочитаемая строка для статус-бара.
    Stage(String),
    Progress(Progress),
    /// Строка диагностики (stderr yt-dlp, аргументы запуска и т.п.).
    Log(String),
    /// Готово, файл лежит здесь.
    Done(PathBuf),
    Failed(String),
    /// Установка завершена: всё необходимое на месте, можно работать.
    /// Отдельный вариант, а не `Done`, потому что файла здесь нет —
    /// путь показывать нечего, и UI просто закрывает модалку.
    Ready,
    /// Работать можно, но с оговоркой — например, не поставился `ffmpeg`.
    ///
    /// Отдельно от `Log`: журнал свёрнут и очищается перед каждой загрузкой,
    /// поэтому предупреждение в нём пропадает ровно тогда, когда становится
    /// нужным. И отдельно от `Failed`: это не отказ, работа продолжается.
    Warning(String),
    /// Всё прошло хорошо, и об этом надо сказать словами: например, до какой
    /// версии обновился yt-dlp или что он и так был последним.
    ///
    /// Отдельно от `Ready` по той же причине, по какой `Warning` отделён от
    /// `Log`: `Ready` только закрывает модалку, и без текста рядом обновление
    /// выглядело бы как «ничего не произошло» — а именно это и надо было
    /// отличить от настоящего обновления.
    Notice(String),
    /// Метаданные локального файла прочитаны. Пустой список — законный исход
    /// («Метаданные не найдены»), а не ошибка: чистый файл выглядит именно так.
    Tags(Vec<Tag>),
    /// Метаданные удалены, файл стал легче на столько байт.
    ///
    /// Ноль здесь тоже законен: чистить было нечего, файл не тронут. Отдельно
    /// от `Notice`, потому что число надо форматировать по-человечески, а это
    /// работа UI: движок числами и оперирует.
    Cleaned(u64),
}

/// Похожа ли строка на ссылку, которую есть смысл отдавать yt-dlp.
///
/// Проверка намеренно грубая: её задача — поймать очевидную опечатку
/// (забытый протокол, случайный текст из буфера обмена), а не отфильтровать
/// всё неверное. Список поддерживаемых сайтов знает yt-dlp, а не Savio,
/// поэтому UI только подсвечивает поле и **не** блокирует кнопку: решение
/// всегда остаётся за пользователем.
pub fn looks_like_url(text: &str) -> bool {
    let text = text.trim();
    let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    else {
        return false;
    };

    // Хост — всё до первого разделителя пути. Он обязан существовать и
    // содержать точку: «https://» и «https://youtube» ссылками ещё не являются.
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();

    host.contains('.') && !host.starts_with('.') && !host.ends_with('.') && !host.contains(' ')
}

/// Единицы объёма, шаг 1024. Терабайт — не запас на будущее: качая
/// плейлист целиком, за него выходят уже сегодня, а без верхней единицы
/// такой размер показывался бы как «5120.0 ГБ» — число, которое глазами
/// не читается.
const BYTE_UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];

/// Подбирает единицу под значение: делит на 1024, пока влезает.
///
/// Общая часть `human_bytes` и `human_speed`: шкала у них одна, и разъезжаться
/// ей нельзя. Стоит одной из функций остановиться на единицу раньше — и рядом
/// окажутся «5.0 ГБ из 10.0 ГБ» и «5120.0 МБ/с», числа одного порядка,
/// выглядящие как разные.
///
/// Не число и не положительное значение схлопываются в ноль: делить на 1024
/// такое бессмысленно, а до цикла эти случаи всё равно надо отсечь, иначе
/// `NaN >= 1024.0` даёт `false` и NaN уходит прямиком в вывод.
fn scale_bytes(value: f64) -> (f64, usize) {
    let mut value = if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    };
    let mut unit = 0;
    while value >= 1024.0 && unit < BYTE_UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    (value, unit)
}

pub fn human_bytes(bytes: u64) -> String {
    let (value, unit) = scale_bytes(bytes as f64);
    if unit == 0 {
        format!("{bytes} {}", BYTE_UNITS[0])
    } else {
        format!("{value:.1} {}", BYTE_UNITS[unit])
    }
}

/// Скорость загрузки: байты в секунду вместе с единицей, готовые к показу.
///
/// Отдельная функция, а не `human_bytes(speed as u64)` на стороне UI: у
/// скорости своя размерность («/с»), и собирать её из двух кусков в кадре
/// незачем. Плюс `as`-приведение отбрасывает дробную часть, и на медленном
/// соединении 0.9 Б/с превращались в «0 Б/с» — то есть в «встало», хотя
/// загрузка идёт.
pub fn human_speed(bytes_per_sec: f64) -> String {
    let (value, unit) = scale_bytes(bytes_per_sec);
    if unit == 0 {
        format!("{value:.0} {}/с", BYTE_UNITS[0])
    } else {
        format!("{value:.1} {}/с", BYTE_UNITS[unit])
    }
}

pub fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_accepts_real_links() {
        assert!(looks_like_url("https://youtube.com/watch?v=abc"));
        assert!(looks_like_url("http://vk.com/video-1_2"));
        assert!(looks_like_url("https://www.kinobase.org/film/123"));
        // Пробелы по краям — обычное дело при вставке из буфера.
        assert!(looks_like_url("  https://youtu.be/abc  "));
    }

    #[test]
    fn url_rejects_obvious_junk() {
        assert!(!looks_like_url(""));
        assert!(!looks_like_url("youtube.com"), "нет протокола");
        assert!(!looks_like_url("https://"), "нет хоста");
        assert!(!looks_like_url("https://youtube"), "хост без точки");
        assert!(!looks_like_url("ftp://example.com"), "не http(s)");
        assert!(!looks_like_url("просто текст"));
        assert!(!looks_like_url("https://.com"), "хост начинается с точки");
        assert!(!looks_like_url("https://example."), "хост кончается точкой");
    }

    #[test]
    fn bytes_switch_units() {
        assert_eq!(human_bytes(0), "0 Б");
        assert_eq!(human_bytes(512), "512 Б");
        assert_eq!(human_bytes(1024), "1.0 КБ");
        assert_eq!(human_bytes(1536), "1.5 КБ");
        assert_eq!(human_bytes(1024 * 1024), "1.0 МБ");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 ГБ");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 ГБ");
        assert_eq!(human_bytes(1024_u64.pow(4)), "1.0 ТБ");
        assert_eq!(human_bytes(5 * 1024_u64.pow(4)), "5.0 ТБ");
        // Терабайт — последняя единица, дальше просто копятся ТБ.
        assert_eq!(human_bytes(5 * 1024_u64.pow(5)), "5120.0 ТБ");
        // Верхняя граница типа не должна ни паниковать, ни переполняться.
        assert!(human_bytes(u64::MAX).ends_with(" ТБ"));
    }

    #[test]
    fn speed_carries_its_unit() {
        assert_eq!(human_speed(0.0), "0 Б/с");
        assert_eq!(human_speed(512.0), "512 Б/с");
        // Дробные байты в секунду — не «стоит»: округляем вверх, а не в ноль.
        assert_eq!(human_speed(0.9), "1 Б/с");
        assert_eq!(human_speed(1024.0), "1.0 КБ/с");
        assert_eq!(human_speed(1_572_864.0), "1.5 МБ/с");
        // Ровно то, что приходит от yt-dlp: 15943362.460976 Б/с.
        assert_eq!(human_speed(15_943_362.460976), "15.2 МБ/с");
    }

    #[test]
    fn speed_survives_nonsense_input() {
        // Оба поставщика `speed_bps` отсекают неположительное и NaN, но
        // формат вывода не должен зависеть от их бдительности: «NaN Б/с»
        // в строке прогресса — это доклад об ошибке чужим языком.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, -0.0] {
            assert_eq!(human_speed(bad), "0 Б/с", "вход: {bad}");
        }
    }

    #[test]
    fn duration_hides_zero_hours() {
        assert_eq!(human_duration(0), "0:00");
        assert_eq!(human_duration(9), "0:09");
        assert_eq!(human_duration(75), "1:15");
        assert_eq!(human_duration(3600), "1:00:00");
        assert_eq!(human_duration(3671), "1:01:11");
    }

    /// Парсер обязан понимать ровно то, что печатает `human_duration`:
    /// человек читает длительность ролика в окне и оттуда же берёт границы.
    #[test]
    fn timecode_reads_back_what_duration_printed() {
        for secs in [0, 9, 75, 600, 3600, 3671, 86_399, 359_999] {
            assert_eq!(
                parse_timecode(&human_duration(secs)),
                Some(secs),
                "{secs} с не прочитались обратно"
            );
        }
    }

    #[test]
    fn timecode_takes_the_usual_ways_of_writing_it() {
        // Голые секунды.
        assert_eq!(parse_timecode("0"), Some(0));
        assert_eq!(parse_timecode("90"), Some(90));
        // Минуты и секунды.
        assert_eq!(parse_timecode("1:30"), Some(90));
        assert_eq!(parse_timecode("1:5"), Some(65), "односимвольные секунды");
        assert_eq!(parse_timecode("01:30"), Some(90), "ведущий ноль");
        // Часы.
        assert_eq!(parse_timecode("1:02:03"), Some(3723));
        assert_eq!(parse_timecode("0:00:07"), Some(7));
        // У первого поля потолка нет: «120:00» — два часа, «90» — полторы минуты.
        assert_eq!(parse_timecode("120:00"), Some(7200));
        // Пробелы по краям — обычное дело при вставке из буфера.
        assert_eq!(parse_timecode("  1:30  "), Some(90));
    }

    #[test]
    fn timecode_rejects_everything_it_cannot_be_sure_about() {
        for bad in [
            "",
            "   ",
            ":",
            "1:",
            ":30",
            "1::30",
            "1:2:3:4", // полей больше трёх
            "1:75",    // 75 секунд — либо опечатка, либо вовсе не время
            "1:60:00", // то же в минутах
            "-5",      // отрицательного времени не бывает
            "+5",      // `parse` такое берёт, а мы не должны
            "1.5",     // не наш разделитель
            "1,5",
            "1:30s",
            "abc",
            "полторы минуты",
            "99999999999999999999999", // не влезает в u64
        ] {
            assert_eq!(parse_timecode(bad), None, "«{bad}» приняли за время");
        }
        // Переполнение при переводе часов в секунды: `checked_mul` обязан
        // вернуть `None`, а не завернуть число по кругу.
        assert_eq!(parse_timecode("99999999999999999999:00:00"), None);
    }

    #[test]
    fn section_takes_both_bounds_and_either_alone() {
        assert_eq!(
            parse_section("1:30", "4:00"),
            Ok(Section {
                start: Some(90),
                end: Some(240)
            })
        );
        assert_eq!(
            parse_section("1:30", ""),
            Ok(Section {
                start: Some(90),
                end: None
            })
        );
        assert_eq!(
            parse_section("", "4:00"),
            Ok(Section {
                start: None,
                end: Some(240)
            })
        );
    }

    /// Оба поля пустые — это «скачать целиком», а не ошибка и не пустой
    /// фрагмент: до появления обрезки Savio вёл себя именно так.
    #[test]
    fn empty_fields_mean_the_whole_video() {
        assert_eq!(parse_section("", ""), Ok(Section::default()));
        assert_eq!(parse_section("  ", " "), Ok(Section::default()));
        assert!(!Section::default().any());
    }

    /// «С 0:00» — это тоже «целиком». Останься ноль отдельной границей,
    /// `any()` ответил бы «просят фрагмент», и весь ролик поехал бы медленным
    /// путём обрезки через ffmpeg вместо обычной загрузки.
    #[test]
    fn zero_start_is_not_a_section() {
        assert_eq!(parse_section("0", ""), Ok(Section::default()));
        assert_eq!(parse_section("0:00", ""), Ok(Section::default()));
        assert!(!parse_section("0:00", "").unwrap().any());
        // А вот с концом ноль уже не пустяк: «с начала по 0:00» — пустой кусок.
        assert_eq!(parse_section("", "0"), Err(SectionError::Order));
    }

    /// Перевёрнутый и вырожденный диапазон обязаны отсекаться здесь: yt-dlp
    /// на них не ругается, а молча отдаёт файл не с тем содержимым.
    #[test]
    fn section_rejects_a_range_that_cuts_nothing() {
        assert_eq!(parse_section("4:00", "1:30"), Err(SectionError::Order));
        assert_eq!(parse_section("90", "90"), Err(SectionError::Order));
        assert_eq!(parse_section("0", "0"), Err(SectionError::Order));
    }

    #[test]
    fn section_says_which_field_to_highlight() {
        assert_eq!(parse_section("абв", "4:00"), Err(SectionError::Start));
        assert_eq!(parse_section("1:30", "абв"), Err(SectionError::End));
        // Начало проверяется первым: когда врут оба поля, красить надо то,
        // с которого человек начал набирать.
        assert_eq!(parse_section("абв", "абв"), Err(SectionError::Start));

        assert!(SectionError::Start.at_start() && !SectionError::Start.at_end());
        assert!(SectionError::End.at_end() && !SectionError::End.at_start());
        // Перевёрнутый диапазон — беда обоих полей сразу.
        assert!(SectionError::Order.at_start() && SectionError::Order.at_end());
    }

    /// Сообщение показывают человеку вместо ввода: пустое или совпадающее
    /// с соседним ничего ему не объяснит.
    #[test]
    fn section_errors_explain_themselves_distinctly() {
        let mut seen: Vec<&str> = Vec::new();
        for err in [SectionError::Start, SectionError::End, SectionError::Order] {
            let message = err.message();
            assert!(!message.trim().is_empty(), "{err:?}: пустое сообщение");
            assert!(!seen.contains(&message), "{err:?}: сообщение повторяется");
            seen.push(message);
        }
    }

    /// Фрагмент по умолчанию не просят. Появись здесь граница — у всех, кто
    /// эти поля не трогал, ролики начали бы приезжать обрезанными.
    #[test]
    fn nothing_is_trimmed_by_default() {
        let section = Section::default();
        assert_eq!(section.start, None);
        assert_eq!(section.end, None);
        assert!(!section.any());
    }

    #[test]
    fn any_notices_each_bound_alone() {
        assert!(
            Section {
                start: Some(90),
                end: None
            }
            .any()
        );
        assert!(
            Section {
                start: None,
                end: Some(240)
            }
            .any()
        );
    }

    #[test]
    fn fraction_is_none_without_total() {
        // Потоковые источники присылают total = 0 — процент показать нечем.
        let p = Progress {
            downloaded: 100,
            total: 0,
            ..Progress::default()
        };
        assert_eq!(p.fraction(), None);
    }

    /// Ступени обязаны идти строго сверху вниз: на этом держится и порядок
    /// сегментов, и то, что переключение MP4 ↔ MP3 сохраняет положение
    /// в списке. Перепутанная местами пара не сломает ни сборку, ни загрузку —
    /// пользователь просто получит не то, что выбрал.
    #[test]
    fn quality_scale_goes_strictly_down() {
        let mut prev: Option<u32> = None;
        for quality in Quality::ALL {
            let Some(height) = quality.max_height() else {
                assert_eq!(quality, Quality::Best, "потолка нет только у «Макс.»");
                assert!(prev.is_none(), "«Макс.» обязан быть первым");
                continue;
            };
            if let Some(prev) = prev {
                assert!(prev > height, "{prev} не выше {height}");
            }
            prev = Some(height);
        }
        assert_eq!(prev, Some(480), "последняя ступень — 480p");
    }

    /// Значение по умолчанию — прежнее поведение Savio: максимум и у видео,
    /// и у звука. Смена умолчания молча изменила бы то, что скачивают
    /// пользователи, ни разу не трогавшие переключатель.
    #[test]
    fn default_quality_is_the_old_behaviour() {
        assert_eq!(Quality::default(), Quality::Best);
        assert_eq!(Quality::Best.max_height(), None);
        assert_eq!(Quality::Best.audio_bitrate(), None);
    }

    /// То же и про формат: к этому значению откатываются запомненные
    /// настройки, когда файла нет или он не читается. Переедь `#[default]`
    /// на MP3 — и первый запуск Savio молча начал бы предлагать звук.
    #[test]
    fn default_format_is_video() {
        assert_eq!(Format::default(), Format::Mp4);
    }

    /// По умолчанию не вшивается ничего. Стоит одному флажку оказаться
    /// включённым по умолчанию — и у всех, кто ни разу их не трогал, молча
    /// изменится содержимое скачанных файлов.
    #[test]
    fn nothing_is_embedded_by_default() {
        let options = DownloadOptions::default();
        assert!(!options.embed_metadata);
        assert!(!options.embed_thumbnail);
        assert!(!options.embed_subs);
        assert!(!options.any());
    }

    /// `any()` обязан замечать каждый флажок по отдельности: на нём держится
    /// проверка наличия ffmpeg, и пропущенная галочка означает загрузку,
    /// сорвавшуюся на постобработке.
    #[test]
    fn any_notices_every_single_checkbox() {
        for options in [
            DownloadOptions {
                embed_metadata: true,
                ..DownloadOptions::default()
            },
            DownloadOptions {
                embed_thumbnail: true,
                ..DownloadOptions::default()
            },
            DownloadOptions {
                embed_subs: true,
                ..DownloadOptions::default()
            },
        ] {
            assert!(options.any(), "{options:?}: галочка не замечена");
        }
    }

    /// Cookies не передаются, пока их не попросили. Переедь `#[default]`
    /// на любой браузер — и Savio у всех, кто ни разу не открывал этот
    /// список, начал бы молча читать профиль браузера.
    #[test]
    fn cookies_are_off_by_default() {
        assert_eq!(CookieSource::default(), CookieSource::None);
        assert_eq!(CookieSource::None.browser(), None);
        assert_eq!(CookieSource::ALL[0], CookieSource::None);
    }

    /// Имя браузера уходит в командную строку дословно, и yt-dlp сверяет его
    /// со своим списком буква в букву. Заглавная буква или пробел — отказ
    /// вместо загрузки, причём такой, которого не увидят ни сборка, ни clippy.
    #[test]
    fn browser_tokens_are_written_the_way_ytdlp_reads_them() {
        // Список yt-dlp 2026.07.04 целиком, минус `safari` и `whale`:
        // первый не работает ни на Windows, ни на Linux, второй — корейский
        // браузер, которого нет в нашем списке.
        const SUPPORTED: [&str; 7] = [
            "brave", "chrome", "chromium", "edge", "firefox", "opera", "vivaldi",
        ];

        let mut seen = Vec::new();
        for source in CookieSource::ALL {
            let Some(browser) = source.browser() else {
                assert_eq!(source, CookieSource::None, "{source:?}: браузер без имени");
                continue;
            };
            assert!(
                SUPPORTED.contains(&browser),
                "{browser}: yt-dlp такого браузера не знает"
            );
            assert!(
                browser.chars().all(|c| c.is_ascii_lowercase()),
                "{browser}: yt-dlp принимает только строчные имена без пробелов"
            );
            assert!(!seen.contains(&browser), "{browser}: повтор в списке");
            seen.push(browser);
        }
        assert_eq!(seen.len(), CookieSource::ALL.len() - 1);
    }

    /// Подписи в списке человек читает глазами: одинаковые или пустые
    /// превратили бы выбор в угадайку.
    #[test]
    fn cookie_labels_are_distinct() {
        let mut seen: Vec<&str> = Vec::new();
        for source in CookieSource::ALL {
            let label = source.label();
            assert!(!label.trim().is_empty(), "{source:?}: пустая подпись");
            assert!(!seen.contains(&label), "{label}: подпись повторяется");
            seen.push(label);
        }
    }

    #[test]
    fn audio_bitrates_are_written_the_way_ffmpeg_reads_them() {
        for quality in Quality::ALL {
            let Some(bitrate) = quality.audio_bitrate() else {
                continue;
            };
            assert!(
                bitrate.ends_with('K'),
                "{bitrate}: без «K» ffmpeg прочитает это как биты в секунду"
            );
            assert!(
                bitrate.trim_end_matches('K').parse::<u32>().is_ok(),
                "{bitrate}: не число"
            );
        }
    }

    /// У видео и звука единицы разные, поэтому и подписи обязаны различаться —
    /// кроме «Макс.», которое одинаково значит «сколько дают».
    #[test]
    fn labels_differ_by_format() {
        for quality in Quality::ALL {
            let (video, audio) = (quality.label(Format::Mp4), quality.label(Format::Mp3));
            assert!(!video.is_empty() && !audio.is_empty());
            if quality != Quality::Best {
                assert_ne!(video, audio, "{quality:?}: подписи совпали");
            }
        }
        assert_ne!(Format::Mp4.quality_label(), Format::Mp3.quality_label());
    }

    #[test]
    fn max_height_takes_the_top_of_the_list() {
        let info = MediaInfo {
            heights: vec![1080, 720, 360],
            ..MediaInfo::default()
        };
        assert_eq!(info.max_height(), Some(1080));
        // Ролик без видеодорожек — законный случай, а не ошибка.
        assert_eq!(MediaInfo::default().max_height(), None);
    }

    /// Обложка с несходящимися размерами обязана отсекаться **до** `ColorImage`:
    /// там несовпадение не ошибка, а паника, то есть исчезнувшее окно.
    #[test]
    fn thumbnail_checks_its_own_size() {
        let good = Thumbnail {
            width: 2,
            height: 1,
            rgba: vec![0; 8],
        };
        assert!(good.is_valid());

        let cases = [
            // Не хватает байта и байт лишний — обычный след обрезанного буфера.
            (2, 1, 7),
            (2, 1, 12),
            // Пустая картинка: текстуры нулевого размера не бывает.
            (0, 0, 0),
            (4, 0, 0),
            // Переполнение при перемножении сторон — ровно то, из-за чего
            // здесь `checked_mul`, а не `*`.
            (usize::MAX, usize::MAX, 4),
        ];
        for (width, height, len) in cases {
            let bad = Thumbnail {
                width,
                height,
                rgba: vec![0; len],
            };
            assert!(
                !bad.is_valid(),
                "пропущена битая обложка: {width}×{height}, {len} байт"
            );
        }
    }

    #[test]
    fn fraction_clamps_to_one() {
        let p = Progress {
            downloaded: 150,
            total: 100,
            ..Progress::default()
        };
        assert_eq!(p.fraction(), Some(1.0));

        let half = Progress {
            downloaded: 50,
            total: 100,
            ..Progress::default()
        };
        assert_eq!(half.fraction(), Some(0.5));
    }
}
