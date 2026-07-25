//! Загрузка обложки ролика и разбор её в пиксели.
//!
//! Про UI не знает ничего, как и весь движок: наружу отдаётся `model::Thumbnail`
//! — готовый RGBA, который остаётся только залить в текстуру. Слова `egui`
//! в этом файле нет ни разу.
//!
//! Разбор идёт здесь, на потоке движка, и это не мелочь. Декодирование JPEG
//! 1280×720 — это десяток миллисекунд и несколько мегабайт памяти; в потоке
//! отрисовки, который обязан успевать шестьдесят раз в секунду, такому места
//! нет (Правило 1). UI получает уже уменьшенную картинку.
//!
//! Ни одна неудача здесь не считается ошибкой загрузки: обложка — украшение,
//! и мёртвый адрес, незнакомый формат или молчащий сервер приводят лишь
//! к строке в журнале (см. `engine::run`).

use std::io::Cursor;
use std::time::Duration;

use crate::model::Thumbnail;

/// До какой ширины уменьшаем картинку.
///
/// Превью в окне рисуется примерно на 240 точек, здесь двойной запас — под
/// экраны с двойной плотностью, где 240 логических точек это 480 настоящих.
/// Больше держать незачем: текстура 480×270 занимает полмегабайта, а
/// 1920×1080 — восемь, и разницы на превью не видно.
const TARGET_WIDTH: u32 = 480;

/// Потолок на объём скачиваемого.
///
/// Не от жадности сервера, а от подмены: адрес приходит из чужого JSON, и
/// читать по нему в память «сколько дадут» нельзя. Обложка весит десятки,
/// от силы сотни килобайт — восемь мегабайт это запас в разы.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Потолок на размеры разбираемой картинки.
///
/// Заголовок PNG в сотню байт может объявить 40000×40000, и декодировщик честно
/// попытается выделить под это шесть гигабайт: ни ошибки, ни исключения —
/// приложение просто исчезнет с экрана. `Limits` — единственное, что от такого
/// защищает, и по умолчанию ограничения на стороны в `image` **выключены**.
const MAX_SIDE: u32 = 8192;

/// Сколько всего ждём сервер с картинкой.
///
/// На порядки меньше, чем при установке (там час на файл в сотню мегабайт):
/// запуск yt-dlp стоит за этим запросом в очереди на том же потоке, и медленный
/// CDN не должен задерживать саму загрузку. Не приехало за шесть секунд —
/// значит, превью не будет; это дешевле, чем задержка перед началом работы.
const TIMEOUT: Duration = Duration::from_secs(6);

/// Сколько ждём установления соединения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Качает обложку по адресу и отдаёт её готовой к заливке в текстуру.
///
/// Повторов, в отличие от установки инструментов, здесь нет намеренно: от
/// обложки ничего не зависит, а каждая лишняя попытка — это задержка перед
/// началом загрузки ролика.
pub fn fetch(url: &str) -> Result<Thumbnail, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // Часть сайтов отвечает на запрос без User-Agent отказом.
        .user_agent(concat!("savio/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();

    // `https_only`, в отличие от установки, не требуем: там по ссылке приходит
    // программа, которую мы потом запускаем, а здесь — картинка, которую мы
    // только показываем, и часть сайтов до сих пор отдаёт обложки по http.
    // Отказать им значило бы остаться без превью там, где оно безобидно.
    let mut body = agent
        .get(url)
        .call()
        .map_err(|e| format!("сервер не отдал обложку ({e})"))?
        .into_body();

    let bytes = body
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .map_err(|e| format!("не удалось прочитать обложку ({e})"))?;

    decode(&bytes)
}

/// Разбирает байты картинки и уменьшает её до `TARGET_WIDTH`.
///
/// Формат определяется по содержимому, а не по расширению в адресе: у обложек
/// оно врёт регулярно — YouTube отдаёт WebP по ссылке, которая кончается
/// на `.jpg`.
fn decode(bytes: &[u8]) -> Result<Thumbnail, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("не удалось определить формат обложки ({e})"))?;

    // Поля правим по одному: `Limits` помечен `non_exhaustive`, и собрать его
    // выражением структуры нельзя — за пределами `image` таких полей может
    // оказаться больше, чем мы знаем.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    reader.limits(limits);

    let image = reader
        .decode()
        .map_err(|e| format!("не удалось разобрать обложку ({e})"))?;

    // Уменьшаем только вниз. Растянуть мелкую обложку до 480 точек значит
    // занять в разы больше памяти, не добавив ни одной детали.
    let image = if image.width() > TARGET_WIDTH {
        // Высоту считаем сами, хотя `thumbnail` умеет вписывать в рамку:
        // так в коде видно, что ограничение только по ширине, а пропорция
        // сохраняется. Считаем в `u64` — у стороны до 8192 произведение
        // в `u32` ещё влезает, но запас тут бесплатный.
        let height = (u64::from(image.height()) * u64::from(TARGET_WIDTH)
            / u64::from(image.width()))
        .max(1) as u32;
        image.thumbnail_exact(TARGET_WIDTH, height)
    } else {
        image
    };

    let rgba = image.into_rgba8();
    // Размеры снимаем до `into_raw`: он забирает буфер себе.
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let thumbnail = Thumbnail {
        width,
        height,
        rgba: rgba.into_raw(),
    };

    // Согласованность проверяем на выходе, а не надеемся на неё: несовпадение
    // размеров с длиной буфера UI встретит паникой внутри `ColorImage`.
    if !thumbnail.is_valid() {
        return Err(format!(
            "обложка разобрана неправдоподобно: {width}×{height} при {} байтах",
            thumbnail.rgba.len()
        ));
    }

    Ok(thumbnail)
}

#[cfg(test)]
mod tests {
    use super::*;

    use image::{DynamicImage, ImageFormat, RgbaImage};

    /// Собирает картинку заданного размера в памяти — ровно то, что приходит
    /// из сети, только без сети.
    ///
    /// Кодировщик берётся из того же `image`, поэтому тест проверяет и то, что
    /// нужный формат вообще собран в сборку.
    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        // Не однотонная заливка: на равномерном поле любая ошибка в масштабе
        // и порядке байт выглядит правильным результатом.
        let image = RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });

        // JPEG прозрачности не знает, и кодировщик отказывается от RGBA8
        // ошибкой `Color(Rgba8)`. Для него убираем альфу — на проверку это
        // не влияет: у настоящих обложек её тоже нет, а разбор всё равно
        // приводит любую картинку к RGBA.
        let image = DynamicImage::ImageRgba8(image);
        let image = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgb8(image.to_rgb8())
        } else {
            image
        };

        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("картинка обязана закодироваться");
        bytes
    }

    /// Декодеры обязаны быть в сборке. Проверка машинная, потому что потеря
    /// фичи в `Cargo.toml` не ломает ни сборку, ни остальные тесты: обложки
    /// просто перестают показываться — молча и у всех сразу.
    #[test]
    fn required_decoders_are_compiled_in() {
        for format in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::WebP] {
            assert!(
                format.reading_enabled(),
                "декодер {format:?} не собран — проверьте features у `image` в Cargo.toml"
            );
        }
    }

    #[test]
    fn big_cover_is_scaled_down_to_the_target_width() {
        // 1920×1080 — это `maxresdefault` у YouTube, самый частый крупный вход.
        let thumbnail = decode(&encoded(1920, 1080, ImageFormat::Png)).expect("PNG обязан читаться");

        assert_eq!(thumbnail.width, TARGET_WIDTH as usize);
        // Пропорция обязана сохраниться: 1920×1080 → 480×270.
        assert_eq!(thumbnail.height, 270);
        assert!(thumbnail.is_valid(), "размеры не сошлись с буфером");
    }

    /// Настоящие обложки — почти всегда JPEG, и разбираются они другим
    /// декодером, чем PNG. Формат проверяем отдельным тестом, иначе потеря
    /// фичи `jpeg` осталась бы незамеченной.
    #[test]
    fn jpeg_cover_is_decoded() {
        let thumbnail = decode(&encoded(640, 480, ImageFormat::Jpeg)).expect("JPEG обязан читаться");
        assert_eq!(thumbnail.width, TARGET_WIDTH as usize);
        assert_eq!(thumbnail.height, 360);
        assert!(thumbnail.is_valid());
    }

    /// Мелкую обложку не растягиваем: памяти уйдёт в разы больше, а деталей
    /// не прибавится.
    #[test]
    fn small_cover_keeps_its_size() {
        let thumbnail = decode(&encoded(120, 90, ImageFormat::Png)).expect("PNG обязан читаться");
        assert_eq!((thumbnail.width, thumbnail.height), (120, 90));
        assert!(thumbnail.is_valid());
    }

    /// Непропорциональные обложки бывают: вертикальные ролики, квадратные
    /// обложки треков. Уменьшение не должно их искажать.
    #[test]
    fn portrait_and_square_covers_keep_their_shape() {
        // Вертикальный ролик 1080×1920 → 480×853 (округление вниз).
        let portrait = decode(&encoded(1080, 1920, ImageFormat::Png)).expect("PNG обязан читаться");
        assert_eq!(portrait.width, 480);
        assert_eq!(portrait.height, 853);

        // Квадрат остаётся квадратом.
        let square = decode(&encoded(1000, 1000, ImageFormat::Png)).expect("PNG обязан читаться");
        assert_eq!((square.width, square.height), (480, 480));
    }

    /// Настоящая обложка с YouTube: от ответа `-J` до готовых пикселей.
    ///
    /// В обычном прогоне отключён, как и остальные сетевые тесты: `cargo test`
    /// обязан проходить без интернета. Запуск вручную:
    /// `cargo test -- --ignored --nocapture`.
    ///
    /// Проверяет ровно то, чего синтетика увидеть не может: что `thumbnails[]`
    /// у живого ответа устроен так, как считает `parse_thumbnail_url`, что
    /// выбранный адрес отвечает, а не 404 (у YouTube так умеет `maxresdefault`
    /// на части роликов), и что пришедшие байты мы действительно умеем
    /// разбирать. Ни одно из трёх не ловится ни сборкой, ни `clippy`.
    #[test]
    #[ignore = "требует доступа в сеть и установленного yt-dlp"]
    fn real_youtube_cover_is_fetched_and_decoded() {
        const URL: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

        let tools = crate::engine::discover().expect("yt-dlp обязан быть установлен");
        let info = super::super::probe(URL, &tools).expect("метаданные обязаны прийти");
        let url = info
            .thumbnail_url
            .expect("у ролика с YouTube обложка есть всегда");
        println!("выбрана обложка: {url}");

        let cover = fetch(&url).expect("обложка обязана скачаться и разобраться");
        println!("разобрано: {}×{}", cover.width, cover.height);

        assert!(cover.is_valid(), "размеры не сошлись с буфером");
        // Широкоэкранная, а не 4:3: последнее означало бы, что выбрался
        // `hqdefault` или `sddefault` с впечатанными чёрными полосами.
        let ratio = cover.width as f64 / cover.height as f64;
        assert!(
            ratio > 1.6,
            "обложка не широкоэкранная ({}×{}) — похоже, выбран вариант с полосами",
            cover.width,
            cover.height
        );
    }

    /// Мусор вместо картинки — это ошибка, а не паника: по адресу из чужого
    /// JSON может прийти что угодно, вплоть до страницы с ошибкой сервера.
    #[test]
    fn junk_is_an_error_not_a_panic() {
        for junk in [
            &b""[..],
            &b"<html><body>404 Not Found</body></html>"[..],
            // Правильная подпись PNG и обрубок вместо содержимого: формат
            // определится, а разбор обязан честно не получиться.
            &b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR"[..],
        ] {
            assert!(decode(junk).is_err(), "мусор принят за обложку: {junk:?}");
        }
    }
}
