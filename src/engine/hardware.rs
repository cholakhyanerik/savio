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

use crate::i18n::{self, Key, Lang};
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
pub fn start(
    gpu: Option<GpuInfo>,
    lang: Lang,
    tx: Sender<Event>,
    notify: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let _ = tx.send(Event::Stage(i18n::t(lang, Key::StageProbingSystem).into()));
        notify();

        let _ = tx.send(Event::SystemReport(collect(gpu.as_ref(), lang)));
        notify();
    });
}

/// Весь снимок целиком.
///
/// Порядок пунктов — порядок карточек на экране: сначала то, что есть у
/// всех и всегда, потом то, чего на конкретной машине может не быть.
pub fn collect(gpu: Option<&GpuInfo>, lang: Lang) -> SystemReport {
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

    let mut checks = vec![
        system_check(lang),
        cpu_check(&sys, lang),
        memory_check(&sys, lang),
    ];
    checks.extend(disk_checks(lang));
    checks.push(network_check(lang));
    checks.push(battery_check(lang));
    checks.push(usb_check(lang));
    if let Some(gpu) = gpu {
        checks.push(gpu_check(gpu, lang));
    }

    SystemReport { checks }
}

/// Что за система и сколько работает.
fn system_check(lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
    // Все эти — ассоциированные функции, а не методы: экземпляр `System`
    // им не нужен, и каждая пересчитывается при вызове.
    let name = sysinfo::System::long_os_version().or_else(sysinfo::System::name);
    let uptime = sysinfo::System::uptime();

    // Названия системы среди строк нет намеренно: оно уже стоит итогом
    // карточки, и повторённое слово в слово читается как задвоение вёрстки.
    let rows = vec![
        CheckRow::maybe(name_of(Key::HwKernel), sysinfo::System::kernel_version()),
        CheckRow::maybe(name_of(Key::HwHostName), sysinfo::System::host_name()),
        // `cpu_arch` не возвращает `Option`: не ответившая система подменяется
        // архитектурой, под которую собран сам Savio. Врать этим нельзя,
        // но и отличить подмену нечем — берём как есть, оговорки не будет.
        CheckRow::new(name_of(Key::HwBitness), sysinfo::System::cpu_arch()),
        CheckRow::new(name_of(Key::HwUptime), human_uptime(uptime, lang)),
    ];

    // Пустой `System::name()` бывает на неизвестной платформе. Пункт при этом
    // не пропадает: у отчёта должна быть строка про систему в любом случае.
    let (status, summary) = match &name {
        Some(name) => (CheckStatus::Ok, name.clone()),
        None => (
            CheckStatus::Unknown,
            name_of(Key::HwSystemUnnamed).to_owned(),
        ),
    };

    Check {
        name: name_of(Key::HwSystem).to_owned(),
        status,
        summary,
        rows,
        advice: None,
    }
}

fn cpu_check(sys: &sysinfo::System, lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
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
        CheckRow::maybe(name_of(Key::HwVendor), vendor),
        CheckRow::maybe(
            name_of(Key::HwPhysicalCores),
            physical.map(|n| n.to_string()),
        ),
        CheckRow::new(name_of(Key::HwLogicalCores), logical.to_string()),
        // Частота приезжает нулём и когда её не спросили, и когда система
        // не ответила, — `human_mhz` превращает такой ноль в прочерк.
        CheckRow::maybe(
            name_of(Key::HwFrequency),
            first
                .map(sysinfo::Cpu::frequency)
                .and_then(|mhz| human_mhz(mhz, lang)),
        ),
        CheckRow::maybe(name_of(Key::HwLoad), human_percent(usage)),
    ];

    let (status, summary) = match &brand {
        Some(brand) => (CheckStatus::Ok, brand.clone()),
        // Пустой список процессоров означает «не обновляли», но обновление
        // тут заведомо было — значит, система действительно промолчала.
        None => (
            CheckStatus::Unknown,
            name_of(Key::HwCpuUnnamed).to_owned(),
        ),
    };

    Check {
        name: name_of(Key::HwCpu).to_owned(),
        status,
        summary,
        rows,
        advice: None,
    }
}

fn memory_check(sys: &sysinfo::System, lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
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
            name: name_of(Key::HwMemory).to_owned(),
            status: CheckStatus::Unknown,
            summary: name_of(Key::HwMemoryUnknown).to_owned(),
            rows: vec![CheckRow::missing(name_of(Key::HwTotal))],
            advice: None,
        };
    }

    let free_share = available as f64 / total as f64;
    let rows = vec![
        CheckRow::new(name_of(Key::HwTotal), human_bytes(total, lang)),
        CheckRow::new(name_of(Key::HwUsed), human_bytes(used, lang)),
        CheckRow::new(name_of(Key::HwAvailable), human_bytes(available, lang)),
        // Своп, выключенный пользователем, — это честный ноль, а не «нет
        // данных»: так и пишем словами, а не прочерком.
        CheckRow::new(
            name_of(Key::HwSwap),
            if total_swap == 0 {
                name_of(Key::HwSwapOff).to_owned()
            } else {
                i18n::fill(
                    name_of(Key::AmountOfTotal),
                    &[
                        &human_bytes(sys.used_swap(), lang),
                        &human_bytes(total_swap, lang),
                    ],
                )
            },
        ),
    ];

    let low = free_share < DISK_LOW_FREE;
    Check {
        name: name_of(Key::HwMemory).to_owned(),
        status: if low {
            CheckStatus::Warning
        } else {
            CheckStatus::Ok
        },
        summary: i18n::fill(
            name_of(Key::HwFreeOfTotal),
            &[&human_bytes(available, lang), &human_bytes(total, lang)],
        ),
        rows,
        advice: low.then(|| name_of(Key::HwMemoryLowAdvice).to_owned()),
    }
}

/// По карточке на том.
///
/// Именно на **том**, а не на накопитель: `sysinfo` перечисляет точки
/// монтирования, поэтому два раздела одного SSD дадут две записи, а диск
/// без буквы не покажется вовсе. Называть это «дисками» было бы неправдой,
/// и подпись карточки говорит «Том».
fn disk_checks(lang: Lang) -> Vec<Check> {
    let name_of = |key| i18n::t(lang, key);
    let volume = |mount: &str| i18n::fill(name_of(Key::HwVolume), &[mount]);

    let disks = sysinfo::Disks::new_with_refreshed_list();
    if disks.is_empty() {
        return vec![Check {
            name: name_of(Key::HwDisks).to_owned(),
            status: CheckStatus::Unknown,
            summary: name_of(Key::HwNoVolumes).to_owned(),
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
                    name: volume(&mount),
                    status: CheckStatus::Unknown,
                    summary: name_of(Key::HwVolumeUnknown).to_owned(),
                    rows: vec![CheckRow::missing(name_of(Key::HwTotal))],
                    advice: None,
                };
            }

            let free_share = available as f64 / total as f64;
            let low = free_share < DISK_LOW_FREE;

            let rows = vec![
                CheckRow::new(name_of(Key::HwMountPoint), mount.clone()),
                CheckRow::new(
                    name_of(Key::HwFileSystem),
                    disk.file_system().to_string_lossy(),
                ),
                CheckRow::new(name_of(Key::HwTotal), human_bytes(total, lang)),
                CheckRow::new(name_of(Key::HwFree), human_bytes(available, lang)),
                // `DiskKind::Unknown` — честный признак «не спросили или не
                // ответили», и у NVMe он выпадает часто. Прочерк вместо
                // выдумки: показать «HDD» на SSD хуже, чем не показать ничего.
                CheckRow::maybe(
                    name_of(Key::HwKind),
                    match disk.kind() {
                        sysinfo::DiskKind::HDD => Some(name_of(Key::HwHardDisk).to_owned()),
                        // «SSD» — сокращение, а не слово: перевода у него нет.
                        sysinfo::DiskKind::SSD => Some("SSD".to_owned()),
                        _ => None,
                    },
                ),
                CheckRow::new(
                    name_of(Key::HwRemovable),
                    name_of(if disk.is_removable() {
                        Key::WordYes
                    } else {
                        Key::WordNo
                    }),
                ),
            ];

            Check {
                name: volume(&mount),
                status: if low {
                    CheckStatus::Warning
                } else {
                    CheckStatus::Ok
                },
                summary: i18n::fill(
                    name_of(Key::HwFreeOfTotal),
                    &[&human_bytes(available, lang), &human_bytes(total, lang)],
                ),
                rows,
                advice: low.then(|| name_of(Key::HwDiskLowAdvice).to_owned()),
            }
        })
        .collect()
}

fn network_check(lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
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
            name: name_of(Key::HwNetwork).to_owned(),
            status: CheckStatus::Unknown,
            summary: name_of(Key::HwNoInterfaces).to_owned(),
            rows,
            advice: None,
        };
    }

    Check {
        name: name_of(Key::HwNetwork).to_owned(),
        status: CheckStatus::Ok,
        summary: format!(
            "{count} {}",
            i18n::plural(
                lang,
                count as u64,
                name_of(Key::HwInterfaceOne),
                name_of(Key::HwInterfaceFew),
                name_of(Key::HwInterfaceMany),
            )
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
fn battery_check(lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
    let name = name_of(Key::HwBattery).to_owned();

    let manager = match starship_battery::Manager::new() {
        Ok(manager) => manager,
        Err(err) => {
            return Check {
                name,
                status: CheckStatus::Failed,
                summary: i18n::fill(
                    name_of(Key::HwBatteryManagerFailed),
                    &[&err.to_string()],
                ),
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
                summary: i18n::fill(name_of(Key::HwBatteryListFailed), &[&err.to_string()]),
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
            summary: name_of(Key::HwNoBattery).to_owned(),
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
    let full = capacity(battery.energy_full().get::<watt_hour>(), lang);
    let design = capacity(battery.energy_full_design().get::<watt_hour>(), lang);

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
        CheckRow::maybe(name_of(Key::HwCharge), charge.clone()),
        CheckRow::new(
            name_of(Key::HwState),
            name_of(match battery.state() {
                starship_battery::State::Charging => Key::HwCharging,
                starship_battery::State::Discharging => Key::HwDischarging,
                starship_battery::State::Empty => Key::HwDrained,
                starship_battery::State::Full => Key::HwOnMains,
                // `Unknown` — и умолчание крейта, и «драйвер не сказал».
                // Перечислено полностью, без `_`: enum не помечен
                // `#[non_exhaustive]`, и новый вариант должен ломать сборку.
                starship_battery::State::Unknown => Key::HwStateUnknown,
            }),
        ),
        CheckRow::maybe(name_of(Key::HwCapacityNow), full.clone()),
        CheckRow::maybe(name_of(Key::HwCapacityDesign), design.clone()),
        CheckRow::maybe(name_of(Key::HwWear), wear_text.clone()),
        // Ноль циклов наружу не выходит никогда: крейт превращает его в
        // `None`. Поэтому прочерк здесь означает «драйвер счётчик не ведёт»,
        // и на большинстве ноутбуков под Windows это обычный случай, а не
        // редкий. Написать «0 циклов» было бы неправдой.
        CheckRow::maybe(
            name_of(Key::HwCycles),
            battery.cycle_count().map(|n| n.to_string()),
        ),
        CheckRow::maybe(
            name_of(Key::HwVoltage),
            Some(format!(
                "{:.2} {}",
                battery.voltage().get::<volt>(),
                name_of(Key::UnitVolt)
            )),
        ),
        CheckRow::maybe(
            name_of(Key::HwPowerDraw),
            Some(format!(
                "{:.1} {}",
                battery.energy_rate().get::<watt>(),
                name_of(Key::UnitWatt)
            )),
        ),
        CheckRow::maybe(
            name_of(Key::HwVendor),
            battery.vendor().filter(|s| !s.trim().is_empty()).map(str::to_owned),
        ),
        CheckRow::maybe(
            name_of(Key::HwModel),
            battery.model().filter(|s| !s.trim().is_empty()).map(str::to_owned),
        ),
    ];

    // Ёмкости не сообщили — считать износ не из чего, и «в порядке» про
    // такую батарею сказать нельзя: про неё просто ничего не известно.
    let (Some(wear), Some(wear_text)) = (wear, wear_text) else {
        return Check {
            name,
            status: CheckStatus::Unknown,
            summary: name_of(Key::HwBatteryNoCapacity).to_owned(),
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
        summary: i18n::fill(
            name_of(Key::HwBatterySummary),
            &[
                &charge.unwrap_or_else(|| name_of(Key::HwChargeUnknown).to_owned()),
                &wear_text,
            ],
        ),
        rows,
        advice: worn.then(|| name_of(Key::HwBatteryWornAdvice).to_owned()),
    }
}

/// Ёмкость в ватт-часах, если она вообще сообщена.
///
/// Ноль здесь — не разряженная батарея, а «драйвер не заполнил поле». Именно
/// этот ноль стоит в знаменателе и у износа, и у заряда, и именно по нему
/// решается, можно ли им верить: результат деления об этом уже не расскажет —
/// крейт зажимает его в диапазон, превращая бесконечность в правдоподобные
/// «100 %».
fn capacity(watt_hours: f32, lang: Lang) -> Option<String> {
    (watt_hours.is_finite() && watt_hours > 0.0)
        .then(|| format!("{watt_hours:.1} {}", i18n::t(lang, Key::UnitWattHour)))
}

fn usb_check(lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
    let name = name_of(Key::HwUsb).to_owned();

    let devices = match nusb::list_devices().wait() {
        Ok(devices) => devices,
        Err(err) => {
            return Check {
                name,
                status: CheckStatus::Failed,
                summary: i18n::fill(name_of(Key::HwUsbListFailed), &[&err.to_string()]),
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
                    i18n::fill(
                        name_of(Key::HwUsbUnnamedDevice),
                        &[&format!("{:04x}:{:04x}", d.vendor_id(), d.product_id())],
                    )
                });

            // Число и единица врозь: «Мбит/с» переводится, а «480» — нет.
            let speed = match d.speed() {
                Some(nusb::Speed::Low) => Some(("1.5", Key::UnitMegabitPerSecond)),
                Some(nusb::Speed::Full) => Some(("12", Key::UnitMegabitPerSecond)),
                Some(nusb::Speed::High) => Some(("480", Key::UnitMegabitPerSecond)),
                Some(nusb::Speed::Super) => Some(("5", Key::UnitGigabitPerSecond)),
                Some(nusb::Speed::SuperPlus) => Some(("10", Key::UnitGigabitPerSecond)),
                // `Speed` помечен `#[non_exhaustive]`, ветка обязательна.
                // `None` — скорость не опознана, и это прочерк, а не «медленно».
                _ => None,
            };

            let version = usb_version(d.usb_version());
            let value = match speed {
                Some((number, unit)) => i18n::fill(
                    name_of(Key::HwUsbVersionWithSpeed),
                    &[&version, &format!("{number} {}", name_of(unit))],
                ),
                None => i18n::fill(name_of(Key::HwUsbVersion), &[&version]),
            };
            CheckRow::new(label, value)
        })
        .collect();

    // Обрезали список — говорим об этом. Молча укоротить значит показать
    // неполный перечень как полный.
    let devices_word = |n: usize| {
        i18n::plural(
            lang,
            n as u64,
            name_of(Key::HwDeviceOne),
            name_of(Key::HwDeviceFew),
            name_of(Key::HwDeviceMany),
        )
    };

    if total > USB_LIMIT {
        let rest = total - USB_LIMIT;
        rows.push(CheckRow::new(
            name_of(Key::HwAndMore),
            format!("{rest} {}", devices_word(rest)),
        ));
    }

    if total == 0 {
        return Check {
            name,
            status: CheckStatus::Unknown,
            summary: name_of(Key::HwNoUsb).to_owned(),
            rows,
            advice: None,
        };
    }

    Check {
        name,
        status: CheckStatus::Ok,
        summary: format!("{total} {}", devices_word(total)),
        rows,
        advice: None,
    }
}

/// Видеокарта — та самая, которой eframe рисует это окно.
///
/// Второго адаптера не открываем: сведения снимаются с уже готового при
/// старте приложения, и стоят они ноль. Поэтому здесь только раскладка
/// по строкам, без единого запроса к системе.
fn gpu_check(gpu: &GpuInfo, lang: Lang) -> Check {
    let name_of = |key| i18n::t(lang, key);
    Check {
        name: name_of(Key::HwGpu).to_owned(),
        status: CheckStatus::Ok,
        summary: i18n::fill(
            name_of(Key::HwNameAndKind),
            &[&gpu.name, i18n::t(lang, gpu.kind)],
        ),
        // Модели и типа среди строк нет: они и есть итог карточки.
        rows: vec![
            CheckRow::maybe(name_of(Key::HwVendor), gpu.vendor.clone()),
            // Драйвер приходит пустой строкой на Metal и на GL через ANGLE —
            // это штатно, и `app.rs` превращает пустоту в `None` заранее.
            CheckRow::maybe(name_of(Key::HwDriver), gpu.driver.clone()),
            // Имя движка отрисовки (`dx12`, `vulkan`, `metal`) — не слово,
            // а название: переводить его нечем.
            CheckRow::new(name_of(Key::HwRendering), gpu.backend.clone()),
        ],
        advice: None,
    }
}

#[cfg(test)]
mod tests;
