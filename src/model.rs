//! Доменные типы. Ничего не знают ни про UI, ни про yt-dlp.

use std::path::PathBuf;

/// Что именно скачиваем.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Видео со звуком, максимум в пределах MP4-контейнера.
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
