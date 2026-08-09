//! Тесты сбора снимка.
//!
//! Сам `collect` проверяем осторожно: он спрашивает настоящую машину, и
//! утверждать про её начинку нечего — на машине сборщика может не быть ни
//! батареи, ни USB-устройств, ни второго тома. Поэтому проверяется не
//! содержимое ответа, а его форма: что пункты на месте, что «нет данных»
//! не превратилось в ноль и что отчёт переживает сохранение в текст.

use super::*;
use crate::model::{Check, CheckRow, CheckStatus, SystemReport};

fn sample() -> SystemReport {
    SystemReport {
        checks: vec![
            Check {
                name: "Процессор".to_owned(),
                status: CheckStatus::Ok,
                summary: "Тестовый камень".to_owned(),
                rows: vec![
                    CheckRow::new("Модель", "Тестовый камень"),
                    CheckRow::missing("Частота"),
                ],
                advice: None,
            },
            Check {
                name: "Батарея".to_owned(),
                status: CheckStatus::Warning,
                summary: "Заряд 50%, износ 60%".to_owned(),
                rows: vec![CheckRow::missing("Циклов заряда")],
                advice: Some("Батарея изношена.".to_owned()),
            },
        ],
    }
}

#[test]
fn collect_names_every_check_it_returns() {
    let report = collect(None);

    // Пункты, которые есть на любой машине: без них отчёт бессмысленен.
    for expected in ["Система", "Процессор", "Память", "Сеть", "Батарея"] {
        assert!(
            report.checks.iter().any(|c| c.name == expected),
            "в отчёте нет пункта «{expected}»"
        );
    }

    // Ни у одного пункта не должно быть пустого заголовка или пустого итога:
    // карточка без подписи выглядит поломкой вёрстки.
    for check in &report.checks {
        assert!(!check.name.is_empty(), "пункт без имени");
        assert!(!check.summary.is_empty(), "пункт «{}» без итога", check.name);
    }
}

#[test]
fn missing_value_never_becomes_a_zero() {
    // Главная проверка модуля. Значение, которого нет, обязано доехать до
    // UI как `None`, а не как «0», «0 МГц» или пустая строка: подставленный
    // ноль читается как измеренная величина.
    let row = CheckRow::missing("Частота");
    assert!(row.value.is_none());

    // Ноль частоты — это «не спросили или не ответили», и наружу он выходит
    // прочерком, а не «0 МГц».
    assert_eq!(human_mhz(0), None);
    assert_eq!(human_mhz(2400).as_deref(), Some("2.40 ГГц"));
    assert_eq!(human_mhz(800).as_deref(), Some("800 МГц"));
}

#[test]
fn worn_out_battery_math_survives_a_driver_that_reports_nothing() {
    // Драйвер не сообщил ни текущую, ни проектную ёмкость: крейт делит
    // ноль на ноль и отдаёт NaN. В окне это стало бы «NaN%».
    assert_eq!(human_percent(f32::NAN), None);
    // Ёмкость нулевая — значит, её не сообщили, а не «батарея пустая».
    assert_eq!(capacity(0.0), None);
    assert_eq!(capacity(f32::NAN), None);
    assert_eq!(capacity(52.5).as_deref(), Some("52.5 Вт·ч"));
}

#[test]
fn health_is_trusted_only_when_both_capacities_are_real() {
    // Именно так `battery_check` и решает, верить ли износу: по самим
    // ёмкостям, а не по конечности их частного.
    //
    // Проверять частное недостаточно, и это ровно та ловушка, ради которой
    // тест написан. `state_of_health` — это `energy_full / energy_full_design`,
    // зажатое крейтом в 0…1. NaN оттуда выходит, только когда нулевые ОБЕ
    // ёмкости; при нулевой одной лишь проектной деление даёт `+inf`, а зажим
    // превращает его ровно в 1.0. То есть батарея, о которой ничего не
    // известно, притворяется совершенно исправной.
    let trust = |full: f32, design: f32| capacity(full).is_some() && capacity(design).is_some();

    assert!(trust(45.0, 50.0), "обе ёмкости настоящие — износ считаем");
    assert!(
        !trust(45.0, 0.0),
        "проектной ёмкости нет: частное даст +inf, зажим — ровно 100%, \
         и батарея без данных выглядела бы исправной"
    );
    assert!(!trust(0.0, 50.0), "текущей ёмкости нет — делить нечего");
    assert!(!trust(0.0, 0.0), "нет обеих: тот самый NaN");

    // И то, что превращает «здоровье» в «износ». Без вычитания исправная
    // батарея на 90% проектной ёмкости подписывалась бы «Износ 90%».
    let wear = |health: f32| 100.0 - health;
    assert_eq!(human_percent(wear(90.0)).as_deref(), Some("10%"));
    assert_eq!(human_percent(wear(100.0)).as_deref(), Some("0%"));

    // Порог замечания — про потерянную долю, и он должен срабатывать
    // при РОСТЕ износа, а не при его уменьшении.
    assert!(wear(75.0) > BATTERY_WORN, "потеряна четверть — это замечание");
    assert!(wear(95.0) < BATTERY_WORN, "потеряна двадцатая — всё в порядке");
}

#[test]
fn tally_counts_each_outcome_apart() {
    let t = sample().tally();
    assert_eq!(t.ok, 1);
    assert_eq!(t.warning, 1);
    assert_eq!(t.failed, 0);
    assert_eq!(t.unknown, 0);
}

#[test]
fn headline_never_promises_that_nothing_is_wrong() {
    let all_fine = SystemReport {
        checks: vec![Check {
            name: "Память".to_owned(),
            status: CheckStatus::Ok,
            summary: "много".to_owned(),
            rows: Vec::new(),
            advice: None,
        }],
    };

    let text = all_fine.headline();
    // Отчёт видит малую часть железа, и обещать по нему исправность машины
    // нельзя даже когда все пункты зелёные.
    assert!(
        !text.contains("роблем"),
        "итог не должен обещать отсутствие проблем: {text}"
    );
    assert!(text.contains('1'), "итог должен пересчитывать пункты: {text}");

    // А замечания и отсутствие данных обязаны быть названы отдельно.
    let mixed = sample().headline();
    assert!(mixed.contains("замечанием"), "{mixed}");
}

#[test]
fn saved_report_keeps_the_dash_where_there_was_no_value() {
    let text = sample().to_text();

    // Файл уходит в чужие руки, и «нет данных», ставшее при сохранении
    // пустотой, там уже не восстановить.
    assert!(
        text.contains("Частота: —"),
        "пропавшее значение должно сохраниться прочерком:\n{text}"
    );
    assert!(text.contains("Циклов заряда: —"), "{text}");
    // Совет доезжает до файла: без него предупреждение теряет половину смысла.
    assert!(text.contains("Совет: Батарея изношена."), "{text}");
    // Статус пункта виден и в файле.
    assert!(text.contains("[Внимание] Батарея"), "{text}");
}

#[test]
fn usb_version_reads_the_binary_coded_field() {
    assert_eq!(usb_version(0x0200), "2.0");
    assert_eq!(usb_version(0x0110), "1.1");
    assert_eq!(usb_version(0x0300), "3.0");
    assert_eq!(usb_version(0x0310), "3.1");
    // Третья цифра появляется, только если она не ноль.
    assert_eq!(usb_version(0x0201), "2.0.1");
}

#[test]
fn uptime_is_written_the_way_a_person_reads_it() {
    // Ради этого функция и заведена: `human_duration` напечатала бы
    // «172:04:11», а машина работает днями.
    assert_eq!(human_uptime(0), "0 минут");
    assert_eq!(human_uptime(60), "1 минута");
    assert_eq!(human_uptime(3 * 60), "3 минуты");
    assert_eq!(human_uptime(3600), "1 час");
    assert_eq!(human_uptime(3600 + 120), "1 час 2 минуты");
    assert_eq!(human_uptime(86_400), "1 день");
    assert_eq!(human_uptime(2 * 86_400 + 4 * 3600), "2 дня 4 часа");
    assert_eq!(human_uptime(5 * 86_400), "5 дней");
    // Одиннадцать-четырнадцать — то самое исключение, на котором правило
    // «по последней цифре» ошибается.
    assert_eq!(human_uptime(11 * 86_400), "11 дней");
    assert_eq!(human_uptime(21 * 86_400), "21 день");
}
