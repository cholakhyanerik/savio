//! Снимок железа: что за машина и в каком она состоянии.
//!
//! Слой «Движок»: здесь спрашивают систему и разбирают ответ. Про `egui`
//! и виджеты модуль не знает ничего — наружу уезжает готовый `SystemReport`,
//! а рисует его `app.rs`.
//!
//! # Чего здесь нет и почему
//!
//! SMART, температуры и обороты вентиляторов не собираются вовсе. Это не
//! недоделка, а решение: без прав администратора их нельзя получить честно
//! ни на одной из трёх систем. Проверено вживую на Windows 11 с обычными
//! правами — `Win32_TemperatureProbe` отдаёт шестнадцать объектов со
//! `Status = OK` и пустым показанием у каждого, `Win32_Fan` — пять таких же,
//! а `MSAcpi_ThermalZoneTemperature` отвечает «Not supported». Чтение SMART
//! упирается в `Access denied` на уровне драйвера, и обойти это не может ни
//! один крейт: `smartctl` требует той же элевации. На Linux нужен
//! `CAP_SYS_RAWIO`, на macOS доступна лишь строка «Verified», которую
//! умирающий SSD отдаёт до последнего дня.
//!
//! Показать «диск здоров» по такому основанию — худший исход из возможных:
//! человек поверит и отдаст накопитель дальше. Пункт про элевацию заведён
//! в `FEATURE_TASKS.txt` отдельной задачей.
//!
//! # Правило 6 в этом модуле
//!
//! Каждый источник здесь умеет соврать нулём. `sysinfo` при неудавшемся
//! запросе частоты кладёт ноль, а не `None`; `state_of_health` батареи при
//! нулевой проектной ёмкости выдаёт `NaN`; `nusb` на Windows никогда не
//! сообщает производителя. Поэтому ноль и пустая строка нигде не уезжают
//! в отчёт значением — только `CheckRow::missing`.

use std::sync::mpsc::Sender;

use nusb::MaybeFuture;

use starship_battery::units::{
    electric_potential::volt, energy::watt_hour, power::watt, ratio::percent,
};

use crate::model::{
    Check, CheckRow, CheckStatus, Event, GpuInfo, SystemReport, human_bytes, human_mhz,
    human_percent, human_uptime, usb_version,
};

/// Ниже этой доли свободного места том считается заполненным.
///
/// Порог, а не точное число байт: на диске в 8 ТБ «мало» наступает совсем
/// не там, где на флешке в 8 ГБ.
const DISK_LOW_FREE: f64 = 0.10;

/// Выше этого износа батарею стоит показать с замечанием.
///
/// Двадцать процентов потерянной ёмкости — общепринятая граница, за которой
/// производители перестают считать батарею исправной; своего смысла Savio
/// в это число не вкладывает. Порог именно про **потерянную** долю: крейт
/// отдаёт сохранившуюся, и её в `battery_check` уже вычли.
const BATTERY_WORN: f32 = 20.0;

/// Сколько устройств показываем в карточке USB.
///
/// Потолок обязателен по тем же соображениям, что `LOG_LIMIT` и
/// `HISTORY_LIMIT`: у машины с несколькими хабами список уходит за сотню,
/// и целиком его всё равно не читают. Сколько отброшено — говорим прямо,
/// молча обрезать список нельзя.
const USB_LIMIT: usize = 24;

/// Собирает снимок в отдельном потоке и присылает его одним событием.
///
/// Устроено по образцу `start_metadata`: свой канал на запуск, `notify`
/// будит кадр. Опрос стоит от долей секунды до пары секунд — в `ui()` ему
/// места нет ни в каком виде (Правило 1).
pub fn start(gpu: Option<GpuInfo>, tx: Sender<Event>, notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let _ = tx.send(Event::Stage("Опрашиваю систему…".into()));
        notify();

        let _ = tx.send(Event::SystemReport(collect(gpu.as_ref())));
        notify();
    });
}

/// Весь снимок целиком.
///
/// Порядок пунктов — порядок карточек на экране: сначала то, что есть у
/// всех и всегда, потом то, чего на конкретной машине может не быть.
pub fn collect(gpu: Option<&GpuInfo>) -> SystemReport {
    let mut sys = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing()
            .with_cpu(sysinfo::CpuRefreshKind::everything())
            .with_memory(sysinfo::MemoryRefreshKind::everything()),
    );

    // Загрузка процессора — это разница между двумя замерами, и первый из
    // них ничего не значит. Без паузы второй замер на Linux и macOS вообще
    // пропускается (крейт сверяет `last_update.elapsed()`), а на Windows
    // даёт мусор. Пауза здесь законна: поток свой, UI её не видит.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();

    let mut checks = vec![system_check(), cpu_check(&sys), memory_check(&sys)];
    checks.extend(disk_checks());
    checks.push(network_check());
    checks.push(battery_check());
    checks.push(usb_check());
    if let Some(gpu) = gpu {
        checks.push(gpu_check(gpu));
    }

    SystemReport { checks }
}

/// Что за система и сколько работает.
fn system_check() -> Check {
    // Все эти — ассоциированные функции, а не методы: экземпляр `System`
    // им не нужен, и каждая пересчитывается при вызове.
    let name = sysinfo::System::long_os_version().or_else(sysinfo::System::name);
    let uptime = sysinfo::System::uptime();

    // Названия системы среди строк нет намеренно: оно уже стоит итогом
    // карточки, и повторённое слово в слово читается как задвоение вёрстки.
    let rows = vec![
        CheckRow::maybe("Ядро", sysinfo::System::kernel_version()),
        CheckRow::maybe("Имя машины", sysinfo::System::host_name()),
        // `cpu_arch` не возвращает `Option`: не ответившая система подменяется
        // архитектурой, под которую собран сам Savio. Врать этим нельзя,
        // но и отличить подмену нечем — берём как есть, оговорки не будет.
        CheckRow::new("Разрядность", sysinfo::System::cpu_arch()),
        CheckRow::new("Работает", human_uptime(uptime)),
    ];

    // Пустой `System::name()` бывает на неизвестной платформе. Пункт при этом
    // не пропадает: у отчёта должна быть строка про систему в любом случае.
    let (status, summary) = match &name {
        Some(name) => (CheckStatus::Ok, name.clone()),
        None => (
            CheckStatus::Unknown,
            "Система себя не назвала.".to_owned(),
        ),
    };

    Check {
        name: "Система".to_owned(),
        status,
        summary,
        rows,
        advice: None,
    }
}

fn cpu_check(sys: &sysinfo::System) -> Check {
    let cpus = sys.cpus();
    let logical = cpus.len();
    let physical = sysinfo::System::physical_core_count();
    let first = cpus.first();

    // `brand` бывает пустой строкой — это `String`, а не `Option`, и «нет
    // данных» выглядит здесь именно так.
    // `trim` здесь не вкусовщина: Intel дополняет строку модели пробелами до
    // фиксированной длины, и «  Intel(R) Xeon(R) CPU E5-1650 v2» в карточке
    // уезжает вправо относительно соседних подписей. Ни сборка, ни тесты
    // этого не видят — заметно только глазами.
    let brand = first
        .map(sysinfo::Cpu::brand)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let vendor = first
        .map(sysinfo::Cpu::vendor_id)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let usage = sys.global_cpu_usage();
    // Модели здесь нет по той же причине, что и названия системы выше: она
    // и есть итог карточки.
    let rows = vec![
        CheckRow::maybe("Производитель", vendor),
        CheckRow::maybe(
            "Физических ядер",
            physical.map(|n| n.to_string()),
        ),
        CheckRow::new("Логических ядер", logical.to_string()),
        // Частота приезжает нулём и когда её не спросили, и когда система
        // не ответила, — `human_mhz` превращает такой ноль в прочерк.
        CheckRow::maybe(
            "Частота",
            first.map(sysinfo::Cpu::frequency).and_then(human_mhz),
        ),
        CheckRow::maybe("Загрузка", human_percent(usage)),
    ];

    let (status, summary) = match &brand {
        Some(brand) => (CheckStatus::Ok, brand.clone()),
        // Пустой список процессоров означает «не обновляли», но обновление
        // тут заведомо было — значит, система действительно промолчала.
        None => (
            CheckStatus::Unknown,
            "Процессор себя не назвал.".to_owned(),
        ),
    };

    Check {
        name: "Процессор".to_owned(),
        status,
        summary,
        rows,
        advice: None,
    }
}

fn memory_check(sys: &sysinfo::System) -> Check {
    // Все значения памяти у `sysinfo` — в байтах. В версии 0.30 единицы
    // сменились с килобайт на байты, и делить на 1024 самим не надо:
    // получилось бы ровно в тысячу раз меньше правды.
    let total = sys.total_memory();
    let available = sys.available_memory();
    let used = sys.used_memory();
    let total_swap = sys.total_swap();

    // Ноль здесь означал бы, что обновления памяти не было, — но оно было.
    if total == 0 {
        return Check {
            name: "Память".to_owned(),
            status: CheckStatus::Unknown,
            summary: "Система не сообщила объём памяти.".to_owned(),
            rows: vec![CheckRow::missing("Всего")],
            advice: None,
        };
    }

    let free_share = available as f64 / total as f64;
    let rows = vec![
        CheckRow::new("Всего", human_bytes(total)),
        CheckRow::new("Занято", human_bytes(used)),
        CheckRow::new("Доступно", human_bytes(available)),
        // Своп, выключенный пользователем, — это честный ноль, а не «нет
        // данных»: так и пишем словами, а не прочерком.
        CheckRow::new(
            "Подкачка",
            if total_swap == 0 {
                "выключена".to_owned()
            } else {
                format!("{} из {}", human_bytes(sys.used_swap()), human_bytes(total_swap))
            },
        ),
    ];

    let low = free_share < DISK_LOW_FREE;
    Check {
        name: "Память".to_owned(),
        status: if low {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        summary: format!(
            "{} из {} свободно",
            human_bytes(available),
            human_bytes(total)
        ),
        rows,
        advice: low.then(|| {
            "Свободной памяти меньше десятой части. Закройте лишние программы: \
             при нехватке система начнёт выгружать их на диск, и всё замедлится."
                .to_owned()
        }),
    }
}

/// По карточке на том.
///
/// Именно на **том**, а не на накопитель: `sysinfo` перечисляет точки
/// монтирования, поэтому два раздела одного SSD дадут две записи, а диск
/// без буквы не покажется вовсе. Называть это «дисками» было бы неправдой,
/// и подпись карточки говорит «Том».
fn disk_checks() -> Vec<Check> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    if disks.is_empty() {
        return vec![Check {
            name: "Диски".to_owned(),
            status: CheckStatus::Unknown,
            summary: "Система не перечислила ни одного тома.".to_owned(),
            rows: Vec::new(),
            advice: None,
        }];
    }

    disks
        .iter()
        .map(|disk| {
            let mount = disk.mount_point().display().to_string();
            let total = disk.total_space();
            let available = disk.available_space();

            // Ноль объёма — настоящий отказ системы: список обновлён целиком,
            // и размер должен был приехать вместе с ним.
            if total == 0 {
                return Check {
                    name: format!("Том {mount}"),
                    status: CheckStatus::Unknown,
                    summary: "Система не сообщила объём тома.".to_owned(),
                    rows: vec![CheckRow::missing("Всего")],
                    advice: None,
                };
            }

            let free_share = available as f64 / total as f64;
            let low = free_share < DISK_LOW_FREE;

            let rows = vec![
                CheckRow::new("Точка монтирования", mount.clone()),
                CheckRow::new("Файловая система", disk.file_system().to_string_lossy()),
                CheckRow::new("Всего", human_bytes(total)),
                CheckRow::new("Свободно", human_bytes(available)),
                // `DiskKind::Unknown` — честный признак «не спросили или не
                // ответили», и у NVMe он выпадает часто. Прочерк вместо
                // выдумки: показать «HDD» на SSD хуже, чем не показать ничего.
                CheckRow::maybe(
                    "Тип",
                    match disk.kind() {
                        sysinfo::DiskKind::HDD => Some("жёсткий диск".to_owned()),
                        sysinfo::DiskKind::SSD => Some("SSD".to_owned()),
                        _ => None,
                    },
                ),
                CheckRow::new("Съёмный", if disk.is_removable() { "да" } else { "нет" }),
            ];

            Check {
                name: format!("Том {mount}"),
                status: if low {
                    CheckStatus::Warning
                } else {
                    CheckStatus::Ok
                },
                summary: format!("{} из {} свободно", human_bytes(available), human_bytes(total)),
                rows,
                advice: low.then(|| {
                    "На томе осталось меньше десятой части места. Скачивать сюда \
                     большие ролики уже рискованно: yt-dlp прервётся на середине."
                        .to_owned()
                }),
            }
        })
        .collect()
}

fn network_check() -> Check {
    let networks = sysinfo::Networks::new_with_refreshed_list();

    let mut rows = Vec::new();
    let mut count = 0usize;
    for (name, data) in &networks {
        count += 1;
        // Неизвестный MAC приезжает не пустым значением, а адресом из одних
        // нулей, который печатается как совершенно правдоподобный
        // «00:00:00:00:00:00». Без этой проверки в отчёт уехал бы фальшивый
        // адрес — тот самый молчаливый отказ.
        let mac = (!data.mac_address().is_unspecified()).then(|| data.mac_address().to_string());
        rows.push(CheckRow::maybe(name.as_str(), mac));
    }

    if count == 0 {
        return Check {
            name: "Сеть".to_owned(),
            status: CheckStatus::Unknown,
            summary: "Система не перечислила сетевые интерфейсы.".to_owned(),
            rows,
            advice: None,
        };
    }

    Check {
        name: "Сеть".to_owned(),
        status: CheckStatus::Ok,
        summary: format!(
            "{count} {}",
            crate::model::plural_ru(count as u64, "интерфейс", "интерфейса", "интерфейсов")
        ),
        rows,
        advice: None,
    }
}

/// Батарея и её износ.
///
/// Три исхода, и различать их обязательно. Пустой итератор — батареи нет
/// (настольная машина), и это не беда. `Err` на создании — спросить не
/// вышло. `Err` на отдельном элементе — драйвер не сообщил ёмкость или
/// напряжение именно этой батареи; такую пропускаем, а не роняем весь опрос.
fn battery_check() -> Check {
    let name = "Батарея".to_owned();

    let manager = match starship_battery::Manager::new() {
        Ok(manager) => manager,
        Err(err) => {
            return Check {
                name,
                status: CheckStatus::Failed,
                summary: format!("Не удалось обратиться к батарее: {err}"),
                rows: Vec::new(),
                advice: None,
            };
        }
    };

    let batteries = match manager.batteries() {
        Ok(batteries) => batteries,
        Err(err) => {
            return Check {
                name,
                status: CheckStatus::Failed,
                summary: format!("Не удалось перечислить батареи: {err}"),
                rows: Vec::new(),
                advice: None,
            };
        }
    };

    let Some(battery) = batteries.flatten().next() else {
        // Ни одной батареи. На настольной машине это норма, и пугать здесь
        // нечем — но и «в порядке» сказать не о чем: мы ничего не проверили.
        return Check {
            name,
            status: CheckStatus::Unknown,
            summary: "Батарея не обнаружена — обычное дело для настольной машины.".to_owned(),
            rows: Vec::new(),
            advice: None,
        };
    };

    // Обе ёмкости берём ПЕРВЫМ делом и решаем по ним, чему вообще можно
    // верить. Проверять конечность частного, как напрашивается, недостаточно:
    // `state_of_health` — это `energy_full / energy_full_design`, зажатое
    // крейтом в диапазон 0…1, и `NaN` из него выходит только когда нулевые
    // ОБЕ ёмкости. Если драйвер сообщил текущую, но не проектную, деление
    // даёт `+inf`, зажим превращает его ровно в 1.0 — и батарея, о которой
    // ничего не известно, получает «100 %» и зелёную плашку «В порядке».
    // На Linux ещё прямее: при нулевой `energy_full` крейт возвращает
    // захардкоженные 100 %, вовсе минуя деление. Ровно то, чего эта вкладка
    // обещает не делать, и ни сборка, ни тесты этого не видят.
    let full = capacity(battery.energy_full().get::<watt_hour>());
    let design = capacity(battery.energy_full_design().get::<watt_hour>());

    // Заряд — тоже частное (`energy / energy_full`) и болеет тем же: при
    // нулевом знаменателе приезжает «100 %» на батарее, о заряде которой
    // не сказано ничего. Делим только когда есть на что.
    let charge = full
        .is_some()
        .then(|| battery.state_of_charge().get::<percent>())
        .and_then(human_percent);

    // Износ — доля ПОТЕРЯННОЙ ёмкости, а крейт отдаёт долю сохранившейся.
    // Вычитание здесь обязательно: без него исправная батарея на 90 %
    // проектной ёмкости подписывалась бы «Износ 90 %», то есть «почти
    // мертва», а предупреждение появлялось бы при уменьшении названного
    // износом числа. Числа были верны, неверным было слово.
    let wear = (full.is_some() && design.is_some())
        .then(|| 100.0 - battery.state_of_health().get::<percent>())
        .filter(|v| v.is_finite());
    let wear_text = wear.and_then(human_percent);

    let rows = vec![
        CheckRow::maybe("Заряд", charge.clone()),
        CheckRow::new(
            "Состояние",
            match battery.state() {
                starship_battery::State::Charging => "заряжается",
                starship_battery::State::Discharging => "разряжается",
                starship_battery::State::Empty => "разряжена",
                starship_battery::State::Full => "от сети, зарядка не идёт",
                // `Unknown` — и умолчание крейта, и «драйвер не сказал».
                // Перечислено полностью, без `_`: enum не помечен
                // `#[non_exhaustive]`, и новый вариант должен ломать сборку.
                starship_battery::State::Unknown => "неизвестно",
            },
        ),
        CheckRow::maybe("Ёмкость сейчас", full.clone()),
        CheckRow::maybe("Ёмкость проектная", design.clone()),
        CheckRow::maybe("Износ", wear_text.clone()),
        // Ноль циклов наружу не выходит никогда: крейт превращает его в
        // `None`. Поэтому прочерк здесь означает «драйвер счётчик не ведёт»,
        // и на большинстве ноутбуков под Windows это обычный случай, а не
        // редкий. Написать «0 циклов» было бы неправдой.
        CheckRow::maybe("Циклов заряда", battery.cycle_count().map(|n| n.to_string())),
        CheckRow::maybe(
            "Напряжение",
            Some(format!("{:.2} В", battery.voltage().get::<volt>())),
        ),
        CheckRow::maybe("Отдаёт", Some(format!("{:.1} Вт", battery.energy_rate().get::<watt>()))),
        CheckRow::maybe(
            "Производитель",
            battery.vendor().filter(|s| !s.trim().is_empty()).map(str::to_owned),
        ),
        CheckRow::maybe(
            "Модель",
            battery.model().filter(|s| !s.trim().is_empty()).map(str::to_owned),
        ),
    ];

    // Ёмкости не сообщили — считать износ не из чего, и «в порядке» про
    // такую батарею сказать нельзя: про неё просто ничего не известно.
    let (Some(wear), Some(wear_text)) = (wear, wear_text) else {
        return Check {
            name,
            status: CheckStatus::Unknown,
            summary: "Батарея есть, но ёмкость драйвер не сообщает — износ посчитать не из чего."
                .to_owned(),
            rows,
            advice: None,
        };
    };

    let worn = wear > BATTERY_WORN;
    Check {
        name,
        status: if worn {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        summary: format!(
            "Заряд {}, износ {wear_text}",
            charge.unwrap_or_else(|| "неизвестен".to_owned())
        ),
        rows,
        advice: worn.then(|| {
            "Батарея держит меньше четырёх пятых от проектной ёмкости. \
             Это не поломка, но время работы от неё будет заметно меньше \
             заявленного, и дальше оно продолжит уменьшаться."
                .to_owned()
        }),
    }
}

/// Ёмкость в ватт-часах, если она вообще сообщена.
///
/// Ноль здесь — не разряженная батарея, а «драйвер не заполнил поле». Именно
/// этот ноль стоит в знаменателе и у износа, и у заряда, и именно по нему
/// решается, можно ли им верить: результат деления об этом уже не расскажет —
/// крейт зажимает его в диапазон, превращая бесконечность в правдоподобные
/// «100 %».
fn capacity(watt_hours: f32) -> Option<String> {
    (watt_hours.is_finite() && watt_hours > 0.0).then(|| format!("{watt_hours:.1} Вт·ч"))
}

fn usb_check() -> Check {
    let name = "USB-устройства".to_owned();

    let devices = match nusb::list_devices().wait() {
        Ok(devices) => devices,
        Err(err) => {
            return Check {
                name,
                status: CheckStatus::Failed,
                summary: format!("Не удалось перечислить устройства: {err}"),
                rows: Vec::new(),
                advice: None,
            };
        }
    };

    // Внешние хабы из списка убираем: они есть в каждой цепочке, полезного
    // не сообщают и вытесняют собой то, ради чего список открывают.
    // Корневых хабов здесь нет и так — их отсеивает сам крейт.
    const CLASS_HUB: u8 = 0x09;
    let devices: Vec<_> = devices.filter(|d| d.class() != CLASS_HUB).collect();

    let total = devices.len();
    let mut rows: Vec<CheckRow> = devices
        .iter()
        .take(USB_LIMIT)
        .map(|d| {
            // Производителя на Windows не будет никогда: крейт намеренно
            // отдаёт `None`, потому что система берёт эту строку из .inf
            // драйвера, а не из дескриптора устройства, и она часто неверна.
            // Осознанный отказ соврать — поэтому подставлять сюда нечего.
            let label = d
                .product_string()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!("Устройство {:04x}:{:04x}", d.vendor_id(), d.product_id())
                });

            let speed = match d.speed() {
                Some(nusb::Speed::Low) => Some("1.5 Мбит/с"),
                Some(nusb::Speed::Full) => Some("12 Мбит/с"),
                Some(nusb::Speed::High) => Some("480 Мбит/с"),
                Some(nusb::Speed::Super) => Some("5 Гбит/с"),
                Some(nusb::Speed::SuperPlus) => Some("10 Гбит/с"),
                // `Speed` помечен `#[non_exhaustive]`, ветка обязательна.
                // `None` — скорость не опознана, и это прочерк, а не «медленно».
                _ => None,
            };

            let value = match speed {
                Some(speed) => format!("USB {}, {speed}", usb_version(d.usb_version())),
                None => format!("USB {}", usb_version(d.usb_version())),
            };
            CheckRow::new(label, value)
        })
        .collect();

    // Обрезали список — говорим об этом. Молча укоротить значит показать
    // неполный перечень как полный.
    if total > USB_LIMIT {
        rows.push(CheckRow::new(
            "И ещё",
            format!(
                "{} {}",
                total - USB_LIMIT,
                crate::model::plural_ru(
                    (total - USB_LIMIT) as u64,
                    "устройство",
                    "устройства",
                    "устройств"
                )
            ),
        ));
    }

    if total == 0 {
        return Check {
            name,
            status: CheckStatus::Unknown,
            summary: "Подключённых устройств не найдено.".to_owned(),
            rows,
            advice: None,
        };
    }

    Check {
        name,
        status: CheckStatus::Ok,
        summary: format!(
            "{total} {}",
            crate::model::plural_ru(total as u64, "устройство", "устройства", "устройств")
        ),
        rows,
        advice: None,
    }
}

/// Видеокарта — та самая, которой eframe рисует это окно.
///
/// Второго адаптера не открываем: сведения снимаются с уже готового при
/// старте приложения, и стоят они ноль. Поэтому здесь только раскладка
/// по строкам, без единого запроса к системе.
fn gpu_check(gpu: &GpuInfo) -> Check {
    Check {
        name: "Видеокарта".to_owned(),
        status: CheckStatus::Ok,
        summary: format!("{} ({})", gpu.name, gpu.kind),
        // Модели и типа среди строк нет: они и есть итог карточки.
        rows: vec![
            CheckRow::maybe("Производитель", gpu.vendor.clone()),
            // Драйвер приходит пустой строкой на Metal и на GL через ANGLE —
            // это штатно, и `app.rs` превращает пустоту в `None` заранее.
            CheckRow::maybe("Драйвер", gpu.driver.clone()),
            CheckRow::new("Отрисовка", gpu.backend.clone()),
        ],
        advice: None,
    }
}

#[cfg(test)]
mod tests;
