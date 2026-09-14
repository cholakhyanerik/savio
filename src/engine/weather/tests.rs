use super::*;

use serde_json::json;

/// Прогноз для Еревана — настоящий ответ Open-Meteo от 2026-09-14, сокращённый
/// до трёх часов и двух дней. Форма у него та же, что у полного: `current` —
/// объект, `hourly` и `daily` — объекты параллельных массивов.
const YEREVAN_FORECAST: &str = r#"{
    "latitude": 40.2, "longitude": 44.5,
    "utc_offset_seconds": 14400, "timezone": "Asia/Yerevan",
    "current": {
        "time": "2026-09-14T19:45", "interval": 900,
        "temperature_2m": 25.7, "apparent_temperature": 24.8,
        "relative_humidity_2m": 37, "is_day": 0, "precipitation": 0.00,
        "weather_code": 1, "cloud_cover": 29, "surface_pressure": 904.5,
        "wind_speed_10m": 6.1, "wind_direction_10m": 73, "wind_gusts_10m": 11.2,
        "uv_index": 0.00
    },
    "hourly": {
        "time": ["2026-09-14T19:00", "2026-09-14T20:00", "2026-09-14T21:00"],
        "temperature_2m": [26.1, 24.9, 23.0],
        "weather_code": [1, 2, 3],
        "precipitation_probability": [0, 5, 25],
        "is_day": [1, 0, 0]
    },
    "daily": {
        "time": ["2026-09-14", "2026-09-15"],
        "weather_code": [3, 80],
        "temperature_2m_max": [31.3, 32.2],
        "temperature_2m_min": [17.3, 19.4],
        "sunrise": ["2026-09-14T06:41", "2026-09-15T06:42"],
        "sunset": ["2026-09-14T19:12", "2026-09-15T19:11"],
        "uv_index_max": [6.60, 6.60],
        "precipitation_probability_max": [25, 0]
    }
}"#;

/// Качество воздуха там же — ответ целиком.
const YEREVAN_AIR: &str = r#"{
    "latitude": 40.2, "longitude": 44.5, "utc_offset_seconds": 14400,
    "current": {"time": "2026-09-14T19:00", "interval": 3600,
                "european_aqi": 33, "pm10": 7.6, "pm2_5": 4.9}
}"#;

fn yerevan() -> Place {
    Place {
        name: "Ереван".to_owned(),
        region: Some("Ереван".to_owned()),
        country: Some("Армения".to_owned()),
        latitude: 40.17765,
        longitude: 44.5126,
    }
}

fn value(text: &str) -> Value {
    serde_json::from_str(text).expect("образец ответа обязан быть JSON")
}

/// Кириллица и пробел уходят в адрес закодированными.
///
/// Без кодирования запрос не уходит вовсе: `http::Uri` считает недопустимым
/// всё выше 127 и пробел, и ureq обрывает запрос ещё до сервера. Строка
/// сверена с той, на которую Open-Meteo вживую ответил «Нижний Новгород».
#[test]
fn a_cyrillic_city_is_percent_encoded() {
    assert_eq!(
        percent_encode("Нижний Новгород"),
        "%D0%9D%D0%B8%D0%B6%D0%BD%D0%B8%D0%B9%20%D0%9D%D0%BE%D0%B2%D0%B3%D0%BE%D1%80%D0%BE%D0%B4"
    );
    // Разрешённое оставляем как есть, а разделители строки запроса — нет:
    // «&» внутри названия иначе начал бы новый параметр.
    assert_eq!(percent_encode("Saint-Louis_1.~"), "Saint-Louis_1.~");
    assert_eq!(percent_encode("A&B+C=D?"), "A%26B%2BC%3DD%3F");
}

/// Каждый адрес, который мы строим, `http::Uri` принимает.
///
/// Проверка ровно той ловушки, что обрывает запрос до сервера: без
/// `percent_encode` адрес поиска «Ереван» здесь не разбирается.
#[test]
fn every_request_url_is_a_valid_uri() {
    let new_york = Place {
        name: "New York".to_owned(),
        region: None,
        country: None,
        latitude: 40.71,
        longitude: -74.01,
    };
    for url in [
        search_url("Ереван"),
        search_url("Нижний Новгород"),
        forecast_url(&new_york),
        air_url(&yerevan()),
    ] {
        assert!(
            url.parse::<ureq::http::Uri>().is_ok(),
            "адрес не разбирается как URI: {url}"
        );
    }
}

/// «Ничего не нашлось» — пустой список, а не ошибка.
///
/// Проверено вживую: без находок Open-Meteo отвечает `200` и JSON без ключа
/// `results` вовсе — не пустым массивом.
#[test]
fn a_search_without_results_is_an_empty_list() {
    assert!(parse_places(&json!({"generationtime_ms": 0.0859499})).is_empty());
    assert!(parse_places(&json!({"results": []})).is_empty());
}

/// Найденное читается с областью и страной, а битые записи отбрасываются.
#[test]
fn search_results_carry_region_and_country() {
    let places = parse_places(&json!({"results": [
        {"id": 616052, "name": "Ереван", "latitude": 40.17765, "longitude": 44.5126,
         "country": "Армения", "admin1": "Ереван"},
        // Без координат показывать нечего: погоду для такого места не спросить.
        {"id": 1, "name": "Без координат", "country": "Нигде"},
        {"id": 520555, "name": "Нижний Новгород", "latitude": 56.32867,
         "longitude": 44.00205, "country": "Россия", "admin1": "Нижегородская Область"}
    ]}));

    assert_eq!(places.len(), 2);
    assert_eq!(places[0], yerevan());
    assert_eq!(places[1].title(), "Нижний Новгород, Россия");
    assert_eq!(places[1].detail(), "Нижегородская Область, Россия");
}

/// Русское имя берётся у ближайшего места, а не у первого в списке.
///
/// Проверено вживую: на «New York» с `language=ru` первым приходит «Йорк»
/// в Небраске, а настоящего Нью-Йорка в выдаче нет вовсе. Взять первое —
/// значит показать человеку чужой город под видом его собственного.
#[test]
fn the_russian_name_comes_from_the_nearest_place_only() {
    let from_ip = Place {
        name: "New York".to_owned(),
        region: Some("New York".to_owned()),
        country: Some("United States".to_owned()),
        latitude: 40.7128,
        longitude: -74.006,
    };
    let answer = parse_places(&json!({"results": [
        {"name": "Йорк", "latitude": 40.86807, "longitude": -97.592,
         "country": "США", "admin1": "Небраска"},
        {"name": "New York", "latitude": 53.07897, "longitude": -0.14008,
         "country": "Британия", "admin1": "Англия"}
    ]}));
    assert_eq!(nearest(&answer, &from_ip, NAME_MATCH_KM), None);

    let from_ip = Place {
        name: "Yerevan".to_owned(),
        region: Some("Yerevan".to_owned()),
        country: Some("Armenia".to_owned()),
        latitude: 40.1776484,
        longitude: 44.5125866,
    };
    let answer = parse_places(&json!({"results": [
        {"name": "Yerevanly", "latitude": 40.33226, "longitude": 47.02693,
         "country": "Азербайджан", "admin1": "Бардинский район"},
        {"name": "Ереван", "latitude": 40.17765, "longitude": 44.5126,
         "country": "Армения", "admin1": "Ереван"}
    ]}));
    assert_eq!(nearest(&answer, &from_ip, NAME_MATCH_KM), Some(&yerevan()));
}

/// Расстояние считается по сфере, а не по плоской карте.
#[test]
fn distance_between_cities_is_plausible() {
    let moscow = Place {
        name: "Москва".to_owned(),
        region: None,
        country: None,
        latitude: 55.7558,
        longitude: 37.6173,
    };
    let km = distance_km(&yerevan(), &moscow);
    assert!((1700.0..1900.0).contains(&km), "Ереван — Москва: {km} км");
    assert!(distance_km(&yerevan(), &yerevan()) < 0.001);
}

/// Оба геолокатора разбираются, отказы и «адрес неизвестен» — нет.
#[test]
fn both_ip_locators_are_understood() {
    let ipwho = json!({
        "ip": "5.77.143.2", "success": true, "country": "Armenia", "region": "Yerevan",
        "city": "Yerevan", "latitude": 40.1776484, "longitude": 44.5125866
    });
    let place = parse_ipwho(&ipwho).expect("ответ ipwho.is обязан читаться");
    assert_eq!(place.name, "Yerevan");
    assert_eq!(place.country.as_deref(), Some("Armenia"));

    let ipapi = json!({
        "ip": "5.77.143.2", "city": "Yerevan", "region": "Yerevan",
        "country": "AM", "country_name": "Armenia",
        "latitude": 40.181593, "longitude": 44.514206
    });
    let place = parse_ipapi(&ipapi).expect("ответ ipapi.co обязан читаться");
    assert_eq!(place.country.as_deref(), Some("Armenia"), "страна — из country_name");

    // Ровно так ipapi.co отвечает на запрос без User-Agent.
    let limited = json!({"error": true, "reason": "RateLimited",
                         "message": "Visit https://ipapi.co/ratelimited/ for details"});
    assert_eq!(parse_ipapi(&limited), None);
    assert_eq!(parse_ipwho(&json!({"success": false, "message": "Reserved range"})), None);

    // «0, 0» — это Гвинейский залив, а не место человека.
    let nowhere = json!({"success": true, "city": "", "latitude": 0, "longitude": 0});
    assert_eq!(parse_ipwho(&nowhere), None);
}

/// Настоящий ответ разбирается целиком: сейчас, часы, дни и воздух.
#[test]
fn a_real_forecast_is_parsed_completely() {
    let report = build_report(
        yerevan(),
        &value(YEREVAN_FORECAST),
        Some(&value(YEREVAN_AIR)),
        Some(1_789_398_000),
        false,
    )
    .expect("настоящий ответ обязан разбираться");

    assert_eq!(report.utc_offset, 14_400);
    let now = report.now.as_ref().expect("блок current есть");
    assert_eq!(now.temperature, Some(25.7));
    assert_eq!(now.is_day, Some(false));
    assert_eq!(now.code, Some(1));
    assert_eq!(now.pressure, Some(904.5), "давление — у поверхности");
    assert_eq!(now.uv_index, Some(0.0));

    assert_eq!(report.hours.len(), 3);
    // 19:00 по Еревану — это 15:00 по Гринвичу.
    assert_eq!(
        report.hours[0].at,
        parse_local_time("2026-09-14T15:00").expect("время")
    );
    assert_eq!(report.hours[2].precipitation_chance, Some(25.0));
    assert_eq!(report.hours[1].is_day, Some(false));

    assert_eq!(report.days.len(), 2);
    let today = &report.days[0];
    assert_eq!(
        today.day * 86_400,
        parse_local_time("2026-09-14").expect("дата"),
        "номер дня — по календарю места"
    );
    assert_eq!(
        today.sunrise,
        Some(parse_local_time("2026-09-14T02:41").expect("время"))
    );
    assert_eq!(report.days[1].code, Some(80));

    let air = report.air.expect("воздух пришёл");
    assert_eq!(air.european_aqi, Some(33.0));
    assert_eq!(air.pm2_5, Some(4.9));
}

/// Массивы разной длины и `null` внутри — пропуски, а не паника и не
/// соседнее значение.
#[test]
fn ragged_columns_become_gaps() {
    let forecast = json!({
        "utc_offset_seconds": -14400,
        "hourly": {
            "time": ["2026-09-14T00:00", "2026-09-14T01:00", "не время", "2026-09-14T03:00"],
            "temperature_2m": [20.5, null],
            "weather_code": [0, 2, 3, 61, 95],
            "precipitation_probability": []
        }
    });
    let report = build_report(yerevan(), &forecast, None, None, false).expect("часы есть");

    assert!(report.now.is_none(), "блока current не было");
    assert!(report.days.is_empty());
    // Час с неразбираемым временем отброшен целиком: показать его негде.
    assert_eq!(report.hours.len(), 3);
    assert_eq!(report.hours[0].temperature, Some(20.5));
    assert_eq!(report.hours[1].temperature, None, "null — пропуск");
    assert_eq!(report.hours[2].temperature, None, "массив короче — пропуск");
    // Код погоды у третьего часа — четвёртый в массиве, а не третий:
    // сшивка идёт по индексу исходного часа.
    assert_eq!(report.hours[2].code, Some(61));
    assert_eq!(report.hours[0].precipitation_chance, None);
    // Западнее Гринвича полночь по местному — это четыре утра по UTC.
    assert_eq!(
        report.hours[0].at,
        parse_local_time("2026-09-14T04:00").expect("время")
    );
    assert!(report.air.is_none());
}

/// Ответ без прогноза — отказ с объяснением, а не пустой экран.
#[test]
fn an_answer_without_any_forecast_is_refused() {
    let err = build_report(yerevan(), &json!({"utc_offset_seconds": 0}), None, None, false)
        .expect_err("пустой ответ — не прогноз");
    assert!(err.contains("без прогноза"), "{err}");
}

/// Отказ сервера объясняется словами Savio там, где беда узнаваема,
/// и сырым текстом — там, где нет.
#[test]
fn server_refusals_are_explained() {
    // Настоящий ответ Open-Meteo на широту 100.
    let latitude = status_message(
        400,
        Some("Latitude must be in range of -90 to 90°. Given: 100.0."),
        "Сервер погоды",
    );
    assert!(latitude.contains("неверные координаты"), "{latitude}");
    assert!(!latitude.contains("Latitude"), "английский текст пересказан как есть");

    let busy = status_message(429, None, "Сервер погоды");
    assert!(busy.contains("слишком много запросов"), "{busy}");

    let down = status_message(503, Some("Service Unavailable"), "Сервер погоды");
    assert!(down.contains("не работает") && down.contains("503"), "{down}");

    // Незнакомая беда — сырой текст, а не молчание.
    let other = status_message(400, Some("Parameter 'foo' is invalid"), "Сервер погоды");
    assert!(other.contains("Parameter 'foo' is invalid"), "{other}");

    let bare = status_message(404, None, "Сервер погоды");
    assert!(bare.contains("404"), "{bare}");
}

/// Отчёт с диска — тот же отчёт, только помеченный сохранённым.
///
/// На диск кладутся сырые ответы, и читает их тот же разбор, что читает сеть:
/// второго формата, который разошёлся бы с первым, нет.
#[test]
fn a_saved_report_reads_back_as_the_same_report() {
    let forecast = value(YEREVAN_FORECAST);
    let air = value(YEREVAN_AIR);
    let fresh = build_report(yerevan(), &forecast, Some(&air), Some(1_789_398_000), false)
        .expect("разбор");

    let text = cache_json(&yerevan(), Some(1_789_398_000), &forecast, Some(&air));
    let saved = parse_cache(&text).expect("сохранённый отчёт обязан читаться");

    assert!(saved.saved);
    assert_eq!(WeatherReport { saved: false, ..saved }, fresh);

    // Воздух мог и не прийти — отчёт без него читается тоже.
    let text = cache_json(&yerevan(), None, &forecast, None);
    let saved = parse_cache(&text).expect("без воздуха тоже");
    assert!(saved.air.is_none());
    assert_eq!(saved.fetched_at, None);

    for junk in ["", "{", "[]", r#"{"place": {"name": "Ереван"}}"#] {
        assert!(parse_cache(junk).is_none(), "мусор принят за отчёт: {junk:?}");
    }
}

/// Место переживает запись в JSON, а без координат не читается.
#[test]
fn a_place_survives_json() {
    let place = yerevan();
    assert_eq!(place_from_json(&place_to_json(&place)), Some(place));

    let bare = Place {
        region: None,
        country: None,
        ..yerevan()
    };
    assert_eq!(place_from_json(&place_to_json(&bare)), Some(bare));

    assert_eq!(place_from_json(&json!({"name": "Ереван", "latitude": 40.1})), None);
    assert_eq!(
        place_from_json(&json!({"name": "Ереван", "latitude": 400, "longitude": 44})),
        None
    );
}

/// Часовой пояс машины система называет, и называет правдоподобно.
///
/// Смещения на Земле — от −12 до +14 часов и кратны четверти часа (Непал —
/// +5:45). Число вне этого значит, что поле структуры прочитано не с того
/// места: на Unix это `tm_gmtoff`, на Windows — сумма двух `Bias`.
#[test]
fn the_machine_time_zone_is_plausible() {
    let now = now_unix().expect("часы машины после 1970 года");
    let offset = local_offset(now).expect("система обязана назвать часовой пояс");
    assert!(
        (-12 * 3600..=14 * 3600).contains(&offset),
        "смещение {offset} с"
    );
    assert_eq!(offset % 900, 0, "смещение {offset} с не кратно четверти часа");
}

/// Настоящие серверы: поиск, прогноз, воздух и место по IP.
///
/// В обычном прогоне отключён, как и остальные сетевые тесты: `cargo test`
/// обязан проходить без интернета. Запуск вручную:
/// `cargo test weather -- --ignored --nocapture`.
///
/// Проверяет то, чего не видит разбор сохранённых образцов: что адреса
/// по-прежнему отвечают, что форма ответа не уехала и что поиск по-русски
/// всё ещё отвечает по-русски. Отдаёт геолокатору IP-адрес машины.
#[test]
#[ignore = "ходит в сеть и отдаёт IP-адрес машины геолокатору"]
fn real_services_answer_as_expected() {
    let agent = agent();

    let found = get_json(&agent, &search_url("Ереван"), Service::Search).expect("поиск ответил");
    let places = parse_places(&found);
    println!("найдено: {places:?}");
    let place = places.first().cloned().expect("Ереван находится");
    assert_eq!(place.name, "Ереван");
    assert_eq!(place.country.as_deref(), Some("Армения"));

    let report = fetch(&agent, place).expect("прогноз пришёл");
    println!("сейчас: {:?}", report.now);
    assert_eq!(report.utc_offset, 14_400);
    assert!(report.now.as_ref().is_some_and(|now| now.temperature.is_some()));
    assert_eq!(report.hours.len(), 7 * 24);
    assert_eq!(report.days.len(), 7);
    assert!(report.air.is_some(), "воздух не пришёл");

    let empty = get_json(&agent, &search_url("qqqzzzxx"), Service::Search).expect("поиск ответил");
    assert!(parse_places(&empty).is_empty());

    let here = locate(&agent).expect("место по IP определилось");
    println!("по IP: {} ({}, {})", here.title(), here.latitude, here.longitude);
}
