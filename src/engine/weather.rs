//! Погода: где человек находится и что там с небом.
//!
//! Слой «Движок»: здесь ходят в сеть и разбирают ответы. Про `egui` модуль не
//! знает ничего — наружу уезжают готовые `WeatherReport` и `Place`, строки из
//! них собирает домен, а рисует `app.rs`.
//!
//! # Откуда данные
//!
//! Прогноз, поиск города и качество воздуха — Open-Meteo, три хоста одного
//! сервиса; место по IP — ipwho.is и запасной ipapi.co. Ключей, регистрации
//! и учётных записей не требует ни один (проверено вживую 2026-09-14).
//!
//! Место по IP спрашивается **только по просьбе окна** — при первом открытии
//! вкладки или по кнопке, — а не при запуске Savio: запрос отдаёт чужому
//! серверу IP-адрес человека, а запускал он загрузчик роликов.
//!
//! # Правило 6 в этом модуле
//!
//! Три ответа выглядят успехом и успехом не являются. Все проверены вживую
//! 2026-09-14:
//!
//! * Поиск города без находок отвечает `200` и JSON **без ключа `results`** —
//!   не пустым массивом. Разбор, ждущий массив, объявил бы обычное «ничего не
//!   нашлось» ответом в незнакомом виде.
//! * Геолокаторы называют город по-английски («Yerevan», «Armenia»), а поиск
//!   Open-Meteo по английскому названию с `language=ru` первым ставит не тот
//!   город: на «New York» первым приходит «Йорк» в Небраске. Поэтому русское
//!   имя берётся у найденного места, **ближайшего к координатам**, а не у
//!   первого в списке; дальше `NAME_MATCH_KM` — остаётся английское. Молча
//!   подставить чужой город хуже, чем показать своё название по-английски.
//! * ipapi.co на запрос без `User-Agent` отвечает `429` и
//!   `{"reason": "RateLimited"}` — тем самым, что выглядит исчерпанным
//!   лимитом. С `User-Agent` та же машина в ту же минуту получает `200`.
//!   Источников всё равно два: настоящий лимит по IP провайдера у бесплатных
//!   геолокаторов тоже бывает, и «у соседа работает» тут обычное дело.
//!
//! # Время
//!
//! `chrono` не нужен. Смещение места Open-Meteo присылает числом, даты —
//! строками, а смещение часов самой машины (для «обновлено в 19:45»)
//! спрашивается у системы одной функцией — [`local_offset`].

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::binaries;
use crate::model::{
    AirQuality, DayForecast, Event, HourForecast, NO_DOWNLOAD, Place, WeatherNow, WeatherReport,
    parse_local_time,
};

const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const SEARCH_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const AIR_URL: &str = "https://air-quality-api.open-meteo.com/v1/air-quality";

/// Что спрашиваем про «сейчас». `uv_index` здесь есть, хотя в записи задачи
/// его не было: проверено вживую, что поле отдаётся и в блоке `current`.
const CURRENT_FIELDS: &str = "temperature_2m,apparent_temperature,relative_humidity_2m,\
is_day,precipitation,weather_code,cloud_cover,surface_pressure,wind_speed_10m,\
wind_direction_10m,wind_gusts_10m,uv_index";
/// `is_day` по часам нужен значкам: иначе в ночных столбиках светило бы солнце.
const HOURLY_FIELDS: &str = "temperature_2m,weather_code,precipitation_probability,is_day";
const DAILY_FIELDS: &str = "weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,\
uv_index_max,precipitation_probability_max";

/// Геолокатор: адрес и разбор его ответа.
struct IpSource {
    url: &'static str,
    parse: fn(&Value) -> Option<Place>,
}

/// Геолокаторы по порядку — основной и запасной (см. заголовок модуля).
///
/// Оба по https: `ip-api.com`, который первым приходит на ум, бесплатно
/// отвечает только по http, а по https — `403` (проверено при постановке
/// задачи), и агент с `https_only` его не пустил бы.
const IP_SOURCES: [IpSource; 2] = [
    IpSource {
        url: "https://ipwho.is/",
        parse: parse_ipwho,
    },
    IpSource {
        url: "https://ipapi.co/json/",
        parse: parse_ipapi,
    },
];

/// Как часто обновлять прогноз, пока вкладка открыта, в секундах.
///
/// Пятнадцать минут — не круглое число ради красоты, а шаг самого источника:
/// блок `current` у Open-Meteo приходит с `"interval": 900`, и спрашивать
/// чаще значило бы получать те же числа.
pub const REFRESH_SECS: f64 = 15.0 * 60.0;

/// Сколько мест показывать в найденном.
const SEARCH_COUNT: usize = 8;

/// Насколько далеко от координат геолокатора может лежать место, чьё русское
/// название мы берём.
///
/// Геолокатор по IP точен до города, а не до адреса: координаты у него — это
/// обычно центр города или узел провайдера. Полсотни километров покрывают
/// любой крупный город целиком и не дотягиваются до соседнего.
const NAME_MATCH_KM: f64 = 50.0;

/// Сколько всего ждём ответа.
///
/// Секунды, а не час, как у агента установки (`setup::agent`): там сотня
/// мегабайт ffmpeg, здесь семь килобайт прогноза. С часовым таймаутом вкладка
/// при мёртвой сети «загружалась» бы час.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько ждём соединения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Сколько раз пробуем при обрыве связи или отказе сервера.
///
/// Меньше, чем у установки (`REQUEST_ATTEMPTS`): там без файла не будет
/// работы, а здесь человек смотрит на экран и сам нажмёт «Обновить».
const ATTEMPTS: u32 = 2;

const RETRY_PAUSE: Duration = Duration::from_millis(700);

/// Потолок на размер ответа.
///
/// Прогноз на неделю весит семь килобайт; мегабайт — запас в сто с лишним
/// раз, но всё же потолок: читать в память «сколько дадут» нельзя.
const MAX_BYTES: u64 = 1024 * 1024;

/// Имя файла с последним отчётом — рядом с `settings.json`.
const CACHE_FILE: &str = "weather.json";

/// Версия схемы файла отчёта.
const CACHE_SCHEMA: u64 = 1;

/// Добывает прогноз в отдельном потоке.
///
/// `place` — `None`: сначала определить место по IP. `saved` — прежде сети
/// показать отчёт, лежащий на диске, если он про это же место: без интернета
/// вкладка тогда не пустая, а с прошлыми числами и честной подписью об этом.
///
/// Приёмник свой на каждый запуск. Смена места в окне бросает прежний, и
/// поток, не сумев отправить, выходит — медленный ответ про прошлый город
/// не ляжет поверх нового (тот же приём, что у предпросмотра ссылки).
pub fn start(
    place: Option<Place>,
    saved: bool,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        if saved
            && let Some(wanted) = &place
            && let Some(report) = load_cache().filter(|report| report.place.same_as(wanted))
            && send(&tx, &notify, Event::Weather(Box::new(report))).is_err()
        {
            return;
        }

        let agent = agent();
        let place = match place {
            Some(place) => place,
            None => {
                if send(&tx, &notify, Event::Stage("Определяю место по IP-адресу…".into())).is_err()
                {
                    return;
                }
                match locate(&agent) {
                    Ok(place) => {
                        if send(&tx, &notify, Event::WeatherPlace(place.clone())).is_err() {
                            return;
                        }
                        place
                    }
                    Err(message) => {
                        let _ = send(&tx, &notify, failed(message));
                        return;
                    }
                }
            }
        };

        if send(&tx, &notify, Event::Stage("Запрашиваю прогноз…".into())).is_err() {
            return;
        }
        let event = match fetch(&agent, place) {
            Ok(report) => Event::Weather(Box::new(report)),
            Err(message) => failed(message),
        };
        let _ = send(&tx, &notify, event);
    });
}

/// Ищет места по названию в отдельном потоке.
///
/// Свой приёмник, отдельный от прогноза: искать другой город, пока грузится
/// прогноз этого, — законный сценарий.
pub fn start_search(query: String, tx: Sender<Event>, notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let event = match get_json(&agent(), &search_url(query.trim()), Service::Search) {
            Ok(value) => Event::WeatherPlaces(parse_places(&value)),
            Err(message) => failed(message),
        };
        let _ = send(&tx, &notify, event);
    });
}

/// Отправляет событие и будит окно. `Err` — приёмник брошен, работать больше
/// не на кого.
fn send(tx: &Sender<Event>, notify: &impl Fn(), event: Event) -> Result<(), ()> {
    tx.send(event).map_err(|_| ())?;
    notify();
    Ok(())
}

/// Неудача погоды. Номер загрузки — `NO_DOWNLOAD`: приёмник у погоды свой,
/// разводить в нём нечего.
fn failed(message: String) -> Event {
    Event::Failed {
        id: NO_DOWNLOAD,
        message,
    }
}

/// Сейчас, секундами Unix.
///
/// `None` — часы машины стоят раньше 1970 года. `duration_since` тогда
/// возвращает ошибку, и `unwrap` на ней обернулся бы упавшим окном у человека
/// с севшей батарейкой BIOS.
pub fn now_unix() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
}

/// Смещение часов машины от UTC в момент `unix`, в секундах.
///
/// Стандартная библиотека про часовой пояс не знает ничего, а `chrono` ради
/// одной подписи «обновлено в 19:45» не нужен: у каждой системы это одна
/// функция. Показать же это время по часам *места* нельзя — см.
/// `model::weather_view`.
///
/// На Windows момент не учитывается: `GetTimeZoneInformation` отвечает про
/// «сейчас», а спрашивают у нас про моменты минутной давности, так что
/// переход на летнее время между ними — пренебрежимая редкость.
#[cfg(windows)]
pub fn local_offset(_unix: i64) -> Option<i64> {
    /// `SYSTEMTIME`: восемь `WORD` подряд. Поля не читаются — структура нужна
    /// ради размера.
    #[repr(C)]
    struct SystemTime16 {
        _fields: [u16; 8],
    }

    /// `TIME_ZONE_INFORMATION`, поле в поле.
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime16,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime16,
        daylight_bias: i32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }

    const TIME_ZONE_ID_UNKNOWN: u32 = 0;
    const TIME_ZONE_ID_STANDARD: u32 = 1;
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;

    // SAFETY: структура из чисел и массивов чисел, нули для неё — законное
    // значение, а функция лишь заполняет переданную память своего размера.
    let mut info: TimeZoneInformation = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut info) };

    // Смещение Windows — «сколько минут прибавить к местному, чтобы вышло
    // UTC», то есть с обратным знаком против привычного.
    let extra = match id {
        TIME_ZONE_ID_UNKNOWN => 0,
        TIME_ZONE_ID_STANDARD => info.standard_bias,
        TIME_ZONE_ID_DAYLIGHT => info.daylight_bias,
        // `TIME_ZONE_ID_INVALID`: система не ответила. Выдумывать пояс нельзя.
        _ => return None,
    };
    Some(-i64::from(info.bias + extra) * 60)
}

/// Смещение часов машины от UTC в момент `unix`, в секундах.
///
/// `localtime_r` из libc: она учитывает и `TZ`, и летнее время на заданный
/// момент, а `tm_gmtoff` — готовое смещение. Поле есть и в glibc, и в musl,
/// и в macOS, и лежит у всех троих сразу за `tm_isdst`.
#[cfg(not(windows))]
pub fn local_offset(unix: i64) -> Option<i64> {
    use std::ffi::{c_char, c_int, c_long};

    #[repr(C)]
    struct Tm {
        tm_sec: c_int,
        tm_min: c_int,
        tm_hour: c_int,
        tm_mday: c_int,
        tm_mon: c_int,
        tm_year: c_int,
        tm_wday: c_int,
        tm_yday: c_int,
        tm_isdst: c_int,
        tm_gmtoff: c_long,
        tm_zone: *const c_char,
    }

    unsafe extern "C" {
        fn localtime_r(time: *const c_long, result: *mut Tm) -> *mut Tm;
    }

    // `time_t` на 64-разрядных Linux и macOS — это `long`.
    let time = c_long::try_from(unix).ok()?;
    // SAFETY: в структуре числа и указатель, нули для неё законны; функция
    // только читает `time` и заполняет `result`.
    let mut tm: Tm = unsafe { std::mem::zeroed() };
    if unsafe { localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }
    // Без `i64::from`: на 64-разрядных Linux и macOS `c_long` уже `i64`, и
    // clippy там называет преобразование бесполезным и роняет проверку.
    // Отсюда, из Windows, этого не видно — нашлось сборкой в Docker.
    Some(tm.tm_gmtoff)
}

/// Настроенный HTTP-клиент погоды.
///
/// `http_status_as_error(false)` — ради ответа `400`: Open-Meteo объясняет
/// отказ в теле (`{"reason": "Latitude must be in range…"}`), а ureq по
/// умолчанию превращает любой код от 400 в ошибку и тело выбрасывает.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // Без него ipapi.co отвечает «RateLimited» — см. заголовок модуля.
        .user_agent(concat!("savio/", env!("CARGO_PKG_VERSION")))
        // Все источники отвечают по https, и перехода на http по редиректу
        // здесь не ждём: координаты человека по открытому каналу не ходят.
        .https_only(true)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into()
}

/// К какому серверу обращались — ради сообщения об ошибке.
#[derive(Clone, Copy)]
enum Service {
    Forecast,
    Search,
    Locate,
}

impl Service {
    fn name(self) -> &'static str {
        match self {
            Service::Forecast => "Сервер погоды",
            Service::Search => "Сервер поиска городов",
            Service::Locate => "Сервер определения места",
        }
    }
}

/// Одна неудавшаяся попытка: что сказать человеку и есть ли смысл повторять.
struct Failure {
    message: String,
    retry: bool,
}

/// GET и разбор JSON, с повтором при обрыве связи и отказе сервера.
///
/// На `4xx` не повторяем: неверный запрос второй раз не станет верным, а
/// `429` («слишком часто») повтор через секунду только продлит.
fn get_json(agent: &ureq::Agent, url: &str, service: Service) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match request(agent, url, service) {
            Ok(value) => return Ok(value),
            Err(failure) => {
                last = failure.message;
                if !failure.retry {
                    break;
                }
                if attempt < ATTEMPTS {
                    std::thread::sleep(RETRY_PAUSE);
                }
            }
        }
    }
    Err(last)
}

fn request(agent: &ureq::Agent, url: &str, service: Service) -> Result<Value, Failure> {
    let name = service.name();
    let response = agent.get(url).call().map_err(|err| Failure {
        message: transport_message(&err, name),
        retry: true,
    })?;

    let status = response.status().as_u16();
    let mut body = response.into_body();
    let text = body
        .with_config()
        .limit(MAX_BYTES)
        .read_to_string()
        .map_err(|_| Failure {
            message: format!("{name} прислал ответ, который не удалось дочитать."),
            retry: true,
        })?;
    let value = serde_json::from_str::<Value>(&text).ok();

    if (200..300).contains(&status) {
        return value.ok_or_else(|| Failure {
            message: format!("{name} прислал ответ в незнакомом виде. Попробуйте позже."),
            retry: false,
        });
    }

    let reason = value
        .as_ref()
        .and_then(|value| value.get("reason"))
        .and_then(Value::as_str);
    Err(Failure {
        message: status_message(status, reason, name),
        retry: status >= 500,
    })
}

/// Обрыв связи — на человеческий.
fn transport_message(err: &ureq::Error, name: &str) -> String {
    match err {
        ureq::Error::Timeout(_) => {
            format!("{name} не ответил вовремя. Проверьте подключение к интернету.")
        }
        _ => format!("{name} недоступен. Проверьте подключение к интернету."),
    }
}

/// Отказ сервера — на человеческий.
///
/// Причину Open-Meteo пишет по-английски, и пересказывать её как есть — тот же
/// промах, что разбирает `explain_failure`: для узнаваемых бед у Savio свои
/// слова, а сырой текст остаётся фолбэком, а не единственным вариантом.
/// Приметы — фразы Open-Meteo, проверенные вживую 2026-09-14; промах подстроки
/// не поймает ни компилятор, ни тест, объяснение просто перестанет
/// появляться — и тогда сработает фолбэк.
fn status_message(status: u16, reason: Option<&str>, name: &str) -> String {
    if reason.is_some_and(|reason| {
        reason.contains("Latitude must be") || reason.contains("Longitude must be")
    }) {
        return "У выбранного места неверные координаты. Найдите его заново через поиск."
            .to_owned();
    }
    match (status, reason) {
        (429, _) => format!(
            "{name} просит подождать: с вашего адреса слишком много запросов. \
             Попробуйте через минуту."
        ),
        (500.., _) => format!("{name} сейчас не работает (код {status}). Попробуйте позже."),
        (_, Some(reason)) if !reason.is_empty() => format!("{name} отказал: {reason}"),
        _ => format!("{name} ответил кодом {status}."),
    }
}

/// Адрес запроса прогноза.
fn forecast_url(place: &Place) -> String {
    format!(
        "{FORECAST_URL}?latitude={:.4}&longitude={:.4}&current={CURRENT_FIELDS}\
         &hourly={HOURLY_FIELDS}&daily={DAILY_FIELDS}&timezone=auto&forecast_days=7",
        place.latitude, place.longitude
    )
}

fn air_url(place: &Place) -> String {
    format!(
        "{AIR_URL}?latitude={:.4}&longitude={:.4}&current=european_aqi,pm10,pm2_5&timezone=auto",
        place.latitude, place.longitude
    )
}

fn search_url(query: &str) -> String {
    format!(
        "{SEARCH_URL}?name={}&count={SEARCH_COUNT}&language=ru&format=json",
        percent_encode(query)
    )
}

/// Кодирует текст для строки запроса.
///
/// Без этого «Ереван» и «Нижний Новгород» не уйдут вовсе: ureq строит
/// `http::Uri`, а у того в таблице разрешённых байтов всё выше 127 и пробел —
/// недопустимые, и запрос обрывается ещё до обращения к серверу. Своими
/// руками, а не крейтом: это десяток строк.
fn percent_encode(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    out
}

/// Прогноз и воздух для места, с записью на диск.
fn fetch(agent: &ureq::Agent, place: Place) -> Result<WeatherReport, String> {
    let forecast = get_json(agent, &forecast_url(&place), Service::Forecast)?;
    // Воздух — отдельный хост, и его неудача прогноз не роняет: погода
    // показывается, воздух — нет. Об этом скажет вкладка.
    let air = get_json(agent, &air_url(&place), Service::Forecast).ok();
    let fetched_at = now_unix();

    let report = build_report(place, &forecast, air.as_ref(), fetched_at, false)?;
    // Сырые ответы, а не собранный отчёт: читать файл будет тот же разбор,
    // что читает сеть, и второго формата, который разойдётся с первым, нет.
    save_cache(&report.place, fetched_at, &forecast, air.as_ref());
    Ok(report)
}

/// Собирает отчёт из ответов сервера.
///
/// Частичный успех законен: отчёт без почасового блока лучше пустого экрана.
/// Отказ — только когда в ответе нет ни одного из трёх блоков.
fn build_report(
    place: Place,
    forecast: &Value,
    air: Option<&Value>,
    fetched_at: Option<i64>,
    saved: bool,
) -> Result<WeatherReport, String> {
    // Без смещения времена в ответе — по Гринвичу: так Open-Meteo отвечает,
    // когда часовой пояс не спрошен. Ноль тут не выдуманное значение, а
    // ровно смысл такого ответа.
    let offset = forecast
        .get("utc_offset_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let now = forecast.get("current").map(parse_now);
    let hours = forecast
        .get("hourly")
        .map_or_else(Vec::new, |block| parse_hours(block, offset));
    let days = forecast
        .get("daily")
        .map_or_else(Vec::new, |block| parse_days(block, offset));

    if now.is_none() && hours.is_empty() && days.is_empty() {
        return Err("Сервер погоды прислал ответ без прогноза. Попробуйте обновить позже.".into());
    }

    Ok(WeatherReport {
        place,
        utc_offset: offset,
        fetched_at,
        saved,
        now,
        hours,
        days,
        air: air.and_then(parse_air),
    })
}

/// Число из поля. Нечисловое, `null` и бесконечность — `None`, а не ноль.
fn number(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

/// Непустая строка из поля.
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Код погоды из значения.
fn weather_code(value: Option<&Value>) -> Option<u16> {
    value
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
}

/// Координата, если она вообще координата.
fn coordinate(value: &Value, key: &str, limit: f64) -> Option<f64> {
    number(value, key).filter(|coordinate| coordinate.abs() <= limit)
}

fn parse_now(block: &Value) -> WeatherNow {
    WeatherNow {
        temperature: number(block, "temperature_2m"),
        feels_like: number(block, "apparent_temperature"),
        humidity: number(block, "relative_humidity_2m"),
        is_day: number(block, "is_day").map(|flag| flag != 0.0),
        precipitation: number(block, "precipitation"),
        code: weather_code(block.get("weather_code")),
        cloud_cover: number(block, "cloud_cover"),
        pressure: number(block, "surface_pressure"),
        wind_speed: number(block, "wind_speed_10m"),
        wind_direction: number(block, "wind_direction_10m"),
        wind_gusts: number(block, "wind_gusts_10m"),
        uv_index: number(block, "uv_index"),
    }
}

/// Столбец блока `hourly` или `daily`.
///
/// Open-Meteo отдаёт эти блоки не списком записей, а объектом **параллельных
/// массивов**: `"time": [...]`, `"temperature_2m": [...]`. Сшивать их по
/// индексу вслепую нельзя — массивы вправе оказаться разной длины и с `null`
/// внутри, — поэтому ячейка берётся через [`cell`], и промах в ней — это
/// `None`, а не паника и не соседнее значение.
fn column<'a>(block: &'a Value, key: &str) -> &'a [Value] {
    block
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn cell(column: &[Value], index: usize) -> Option<f64> {
    column
        .get(index)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

/// Время из ячейки — секундами Unix.
fn cell_time(column: &[Value], index: usize, offset: i64) -> Option<i64> {
    column
        .get(index)
        .and_then(Value::as_str)
        .and_then(parse_local_time)
        .map(|local| local - offset)
}

/// Почасовой прогноз.
///
/// Ключ строки — время: час без разбираемого времени отбрасывается целиком,
/// потому что показать его негде. Длина ограничена ответом, а тот — `MAX_BYTES`.
fn parse_hours(block: &Value, offset: i64) -> Vec<HourForecast> {
    let times = column(block, "time");
    let temperatures = column(block, "temperature_2m");
    let codes = column(block, "weather_code");
    let chances = column(block, "precipitation_probability");
    let daylight = column(block, "is_day");

    (0..times.len())
        .filter_map(|index| {
            Some(HourForecast {
                at: cell_time(times, index, offset)?,
                temperature: cell(temperatures, index),
                code: weather_code(codes.get(index)),
                precipitation_chance: cell(chances, index),
                is_day: cell(daylight, index).map(|flag| flag != 0.0),
            })
        })
        .collect()
}

/// Недельный прогноз.
fn parse_days(block: &Value, offset: i64) -> Vec<DayForecast> {
    let times = column(block, "time");
    let codes = column(block, "weather_code");
    let maxima = column(block, "temperature_2m_max");
    let minima = column(block, "temperature_2m_min");
    let sunrises = column(block, "sunrise");
    let sunsets = column(block, "sunset");
    let uv = column(block, "uv_index_max");
    let chances = column(block, "precipitation_probability_max");

    (0..times.len())
        .filter_map(|index| {
            // Дата дня — местная полночь, без вычета смещения: номер дня
            // нужен по календарю места, а не по Гринвичу.
            let day = times
                .get(index)
                .and_then(Value::as_str)
                .and_then(parse_local_time)?
                .div_euclid(86_400);
            Some(DayForecast {
                day,
                code: weather_code(codes.get(index)),
                temperature_max: cell(maxima, index),
                temperature_min: cell(minima, index),
                sunrise: cell_time(sunrises, index, offset),
                sunset: cell_time(sunsets, index, offset),
                uv_index_max: cell(uv, index),
                precipitation_chance: cell(chances, index),
            })
        })
        .collect()
}

/// Качество воздуха. `None` — в ответе нет ни одного из трёх чисел.
fn parse_air(value: &Value) -> Option<AirQuality> {
    let current = value.get("current")?;
    let air = AirQuality {
        european_aqi: number(current, "european_aqi"),
        pm2_5: number(current, "pm2_5"),
        pm10: number(current, "pm10"),
    };
    (air != AirQuality::default()).then_some(air)
}

/// Найденные по названию места.
fn parse_places(value: &Value) -> Vec<Place> {
    // Без находок ключа `results` в ответе нет вовсе — это «ничего не нашлось»,
    // а не ответ в незнакомом виде (см. заголовок модуля).
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|result| {
            Some(Place {
                name: text(result, "name")?,
                region: text(result, "admin1"),
                country: text(result, "country"),
                latitude: coordinate(result, "latitude", 90.0)?,
                longitude: coordinate(result, "longitude", 180.0)?,
            })
        })
        .take(SEARCH_COUNT)
        .collect()
}

/// Ответ ipwho.is: `success`, `city`, `region`, `country`, координаты.
fn parse_ipwho(value: &Value) -> Option<Place> {
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    ip_place(value, "country")
}

/// Ответ ipapi.co: то же, но страна в `country_name`, а отказ — `error`.
fn parse_ipapi(value: &Value) -> Option<Place> {
    if value.get("error").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    ip_place(value, "country_name")
}

fn ip_place(value: &Value, country_key: &str) -> Option<Place> {
    let region = text(value, "region");
    let country = text(value, country_key);
    let latitude = coordinate(value, "latitude", 90.0)?;
    let longitude = coordinate(value, "longitude", 180.0)?;
    // Точка «0, 0» — это Гвинейский залив, а у геолокатора обычно «адрес
    // неизвестен». Показать человеку погоду посреди океана под видом его
    // собственной — ровно та молчаливая подстановка, которой нельзя.
    if latitude == 0.0 && longitude == 0.0 {
        return None;
    }
    Some(Place {
        name: text(value, "city")
            .or_else(|| region.clone())
            .or_else(|| country.clone())?,
        region,
        country,
        latitude,
        longitude,
    })
}

/// Место по IP-адресу, с русским названием, если его удалось найти.
fn locate(agent: &ureq::Agent) -> Result<Place, String> {
    for source in &IP_SOURCES {
        let Ok(value) = get_json(agent, source.url, Service::Locate) else {
            continue;
        };
        if let Some(place) = (source.parse)(&value) {
            return Ok(in_russian(agent, place));
        }
    }
    Err(
        "Не удалось определить место по IP-адресу: серверы определения места не ответили. \
         Найдите свой город через поиск."
            .to_owned(),
    )
}

/// То же место, но названное по-русски — если такое нашлось рядом.
///
/// Любая неудача оставляет английское название: оно хуже русского, но честное.
fn in_russian(agent: &ureq::Agent, place: Place) -> Place {
    let Ok(value) = get_json(agent, &search_url(&place.name), Service::Search) else {
        return place;
    };
    nearest(&parse_places(&value), &place, NAME_MATCH_KM)
        .cloned()
        .unwrap_or(place)
}

/// Ближайшее к `to` из найденного, но не дальше `limit_km`.
fn nearest<'a>(candidates: &'a [Place], to: &Place, limit_km: f64) -> Option<&'a Place> {
    candidates
        .iter()
        .map(|candidate| (distance_km(candidate, to), candidate))
        .filter(|(distance, _)| *distance <= limit_km)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, candidate)| candidate)
}

/// Расстояние по поверхности Земли — формула гаверсинусов.
fn distance_km(a: &Place, b: &Place) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let (lat_a, lat_b) = (a.latitude.to_radians(), b.latitude.to_radians());
    let d_lat = lat_b - lat_a;
    let d_lon = (b.longitude - a.longitude).to_radians();
    let h = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

// ---------------------------------------------------------------------------
// Отчёт на диске
// ---------------------------------------------------------------------------

/// Место строкой JSON — для файла отчёта и для `settings.json`.
pub fn place_to_json(place: &Place) -> Value {
    serde_json::json!({
        "name": place.name,
        "region": place.region,
        "country": place.country,
        "latitude": place.latitude,
        "longitude": place.longitude,
    })
}

/// Место из JSON. `None` — нет названия или координат.
pub fn place_from_json(value: &Value) -> Option<Place> {
    Some(Place {
        name: text(value, "name")?,
        region: text(value, "region"),
        country: text(value, "country"),
        latitude: coordinate(value, "latitude", 90.0)?,
        longitude: coordinate(value, "longitude", 180.0)?,
    })
}

fn cache_path() -> Option<PathBuf> {
    Some(binaries::app_dir()?.join(CACHE_FILE))
}

/// Последний отчёт с диска. Любая неудача — просто «отчёта нет»: это
/// удобство, а не данные, и ругаться из-за него незачем.
fn load_cache() -> Option<WeatherReport> {
    parse_cache(&std::fs::read_to_string(cache_path()?).ok()?)
}

fn parse_cache(text: &str) -> Option<WeatherReport> {
    let value: Value = serde_json::from_str(text).ok()?;
    let place = place_from_json(value.get("place")?)?;
    let fetched_at = value.get("fetched_at").and_then(Value::as_i64);
    let air = value.get("air").filter(|air| !air.is_null());
    build_report(place, value.get("forecast")?, air, fetched_at, true).ok()
}

fn cache_json(place: &Place, fetched_at: Option<i64>, forecast: &Value, air: Option<&Value>) -> String {
    serde_json::json!({
        "version": CACHE_SCHEMA,
        "place": place_to_json(place),
        "fetched_at": fetched_at,
        "forecast": forecast,
        "air": air,
    })
    .to_string()
}

/// Кладёт отчёт на диск — один файл, заменяемый целиком.
///
/// Через временный файл и переименование, как настройки: обрыв записи
/// оставил бы обрезанный JSON. Неудачи глушатся по той же причине, что
/// у чтения. Размер у файла свой потолок имеет сам: отчёт один, и в нём
/// ровно то, что прислал сервер, — не больше `MAX_BYTES` на два ответа.
fn save_cache(place: &Place, fetched_at: Option<i64>, forecast: &Value, air: Option<&Value>) {
    if let Some(path) = cache_path() {
        write_cache(&path, &cache_json(place, fetched_at, forecast, air));
    }
}

fn write_cache(path: &Path, text: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests;
