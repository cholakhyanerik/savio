//! Загрузка видеокарты и её занятая память — для монитора.
//!
//! Слой «Движок», как и весь `monitor`: здесь спрашивают систему и отдают
//! числа, а строки из них собирает `Sampler`. Механизм на каждой системе
//! свой, и общий у них один вопрос — **какая из видеокарт**.
//!
//! # Какая видеокарта
//!
//! Та, которой eframe рисует окно: её имя уже стоит в строке «Видеокарта»,
//! и число под ним обязано быть про неё же. Узнаётся она по двум числам
//! с шины PCI — производителю и модели (`PciId`): они есть и у адаптера
//! wgpu, и у системы, а имени у системы бывает не найти вовсе (в sysfs его
//! нет). Не нашлась однозначно — «нет данных», а не первая попавшаяся:
//! две видеокарты у ноутбука — обычное дело, и загрузка встроенной под
//! именем дискретной — враньё, которое на глаз не отличить. Адаптер своих
//! чисел не назвал, а настоящая видеокарта в системе одна — берём её:
//! путать не с чем.
//!
//! # Windows: PDH и DXGI
//!
//! Загрузку отдают счётчики производительности `\GPU Engine(*)\Utilization
//! Percentage` — те же, из которых рисует диспетчер задач, — и прав
//! администратора им не нужно. Проверено вживую 2026-09-29 (Windows 11
//! 10.0.26200, обычные права, NVIDIA Quadro P2200).
//!
//! Экземпляр счётчика — один движок одной видеокарты у одного процесса:
//! `pid_10340_luid_0x00000000_0x0000B869_phys_0_eng_0_engtype_3D`. Складывать
//! всё подряд нельзя — выйдет за сотню. Диспетчер задач складывает процессы
//! внутри движка, а видеокарте ставит её самый занятый движок, и так же
//! считает `Busiest`. Движки берутся все, а не только `3D`: ролик, который
//! декодирует видеокарта, занимает `VideoDecode`, и монитор, смотрящий
//! на один `3D`, показал бы при нём простой.
//!
//! Видеокарту в имени экземпляра называет LUID, а не производитель, поэтому
//! LUID нашей видеокарты находит DXGI (`IDXGIAdapter1::GetDesc1` отдаёт
//! `VendorId`, `DeviceId` и `AdapterLuid`). В имени старшая половина LUID
//! идёт первой.
//!
//! Правило 6 здесь молчит четырежды, и всё проверено вживую:
//! - **Программный растеризатор для счётчиков — тоже видеокарта.**
//!   «Microsoft Basic Render Driver» получает свой LUID и по тринадцать
//!   движков `3D` у каждого процесса. На машине с одной настоящей картой
//!   LUID-ов поэтому два, и правило «карта одна — её и берём», посчитанное
//!   по счётчикам, не сработало бы никогда. Считаем по DXGI и выбрасываем
//!   адаптеры с флагом `DXGI_ADAPTER_FLAG_SOFTWARE`.
//! - **Чужой LUID запрос принимает молча.** `PdhAddEnglishCounterW` отвечает
//!   успехом и на видеокарту, которой нет, и лишь сбор говорит `PDH_NO_DATA`.
//!   Ошибись здесь порядок половин LUID — ничего бы не упало, числа просто
//!   не было бы никогда. Поэтому пустой ответ — «нет данных», а не ноль:
//!   у видеокарты, которой рисует сам Savio, экземпляры есть всегда — хотя
//!   бы его собственные.
//! - **Счётчик скоростной.** После первого сбора массив отвечает
//!   `PDH_CSTATUS_INVALID_DATA` целиком: доля — разница двух замеров, как
//!   у процессора. Нулевую точку поэтому снимаем при заведении.
//! - **Новый процесс приходит без числа.** Шаблон со звёздочкой подхватывает
//!   появившиеся процессы сам, переоткрывать запрос не нужно, — но экземпляр,
//!   увиденный впервые, приходит со своим `PDH_CSTATUS_INVALID_DATA` при
//!   общем успехе массива. Считать его нулём нельзя: его пропускаем.
//!
//! И отдельно — `PdhAddEnglishCounterW`, а не `PdhAddCounterW`: второй ищет
//! счётчик по имени на языке системы, и на русской Windows английский путь
//! молча не нашёл бы ничего.
//!
//! Занятую видеопамять отдаёт `\GPU Adapter Memory(*)\Dedicated Usage`
//! (байты, число приходит с первого же сбора), а всю — тот же `GetDesc1`.
//! Замер стоит 2–7 мс (отладочная сборка, 2026-09-29, около двухсот
//! экземпляров, два прогона; печатает это `real_gpu_load_from_this_machine`),
//! то есть 20–70 мс за десять секунд открытого монитора рядом с полусекундой,
//! которую он тратит и без видеокарты. Закрыли монитор — запрос закрыт
//! вместе с опросом, и не стоит ничего.
//!
//! # Linux: sysfs
//!
//! `/sys/class/drm/card*/device/gpu_busy_percent` — готовая доля, но только
//! у драйвера AMD (`amdgpu`); там же `mem_info_vram_used` и
//! `mem_info_vram_total`. Видеокарта находится по `device/vendor` и
//! `device/device` — тем же числам PCI, что у wgpu. NVIDIA отдаёт загрузку
//! только через `nvidia-smi`, Intel — только через `perf`, и оба пути —
//! запуск чужой программы каждую секунду, то есть ровно то, от чего
//! Правило 1 бережёт монитор. Там — «нет данных».
//!
//! # macOS
//!
//! «Нет данных». Загрузку там отдаёт IOKit (`IOAccelerator`, ключ
//! `PerformanceStatistics`), но это небезопасный FFI, а собрать и запустить
//! его здесь не на чем: написанный вслепую, он опаснее честного прочерка.

/// Что узнали о видеокарте за один замер.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reading {
    /// Доля, 0..=100. `None` — показания нет; ноль у простаивающей карты
    /// законен и прочерком не становится.
    pub load: Option<f32>,
    /// Занятая память. `None` — система её не отдала.
    pub memory: Option<VideoMemory>,
}

/// Занятая видеопамять.
// Заводят её только ветки Windows и Linux; на macOS замер всегда пуст, и
// `dead_code` там прав. Гасим его ровно там же — голый `allow` погасил бы
// предупреждение и на системах, где оно должно работать.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoMemory {
    /// Байты.
    pub used: u64,
    /// Сколько её всего, байты. `None` — система не сказала.
    pub total: Option<u64>,
}

#[cfg(windows)]
pub use windows::Probe;

#[cfg(target_os = "linux")]
pub use linux::Probe;

#[cfg(not(any(windows, target_os = "linux")))]
pub use elsewhere::Probe;

/// Какую из видеокарт спрашивать: номер в списке или `None`.
///
/// `ids` — производитель и модель каждой настоящей видеокарты системы
/// (программные растеризаторы вычеркнуты заранее). Совпадение с `target`
/// должно быть ровно одно: две одинаковые карты в одной машине различить
/// по этим числам нельзя, и показать загрузку «какой-то из двух» значило
/// бы соврать про ту, что осталась. Без `target` берём видеокарту, только
/// если она одна.
///
/// Полным путём к `PciId`, а не через `use` наверху: на macOS этой функции
/// нет, и импорт оказался бы там неиспользованным — то есть уронил бы
/// сборку на `clippy -D warnings`.
#[cfg(any(windows, target_os = "linux", test))]
fn choose(ids: &[(u32, u32)], target: Option<crate::model::PciId>) -> Option<usize> {
    let Some(target) = target else {
        return (ids.len() == 1).then_some(0);
    };

    let mut found = None;
    for (index, &(vendor, device)) in ids.iter().enumerate() {
        if (vendor, device) == (target.vendor, target.device) {
            if found.is_some() {
                return None;
            }
            found = Some(index);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Windows: разбор имён экземпляров PDH и выбор адаптера
//
// Чистые функции, поэтому собираются и под тестами на любой системе: разбор
// имени — то место, где ошибка молчит (см. заголовок модуля), и проверять
// его надо и там, где самих счётчиков нет.
// ---------------------------------------------------------------------------

#[cfg(any(windows, test))]
mod pdh {
    use super::choose;
    use crate::model::PciId;

    /// Идентификатор адаптера, которым система называет видеокарту
    /// в счётчиках.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Luid {
        /// Старшая половина. У DXGI она знаковая (`LONG`), но в имени
        /// экземпляра печатается восемью шестнадцатеричными цифрами, то есть
        /// как беззнаковая, — так и храним.
        pub high: u32,
        pub low: u32,
    }

    impl Luid {
        /// Как LUID пишется в имени экземпляра: `luid_0x00000000_0x0000B869`,
        /// старшая половина первой.
        pub fn instance(self) -> String {
            format!("luid_0x{:08X}_0x{:08X}", self.high, self.low)
        }
    }

    /// Видеокарта, как её видит DXGI.
    #[derive(Clone, Copy, Debug)]
    pub struct Adapter {
        pub vendor: u32,
        pub device: u32,
        pub luid: Luid,
        /// Своя память видеокарты, байты. Ноль — своей памяти нет.
        // Читает её только ветка Windows; в тестах на Linux и macOS модуль
        // собирается ради разбора имён, и `dead_code` там прав. Гасим ровно
        // там — как в `power`.
        #[cfg_attr(not(windows), allow(dead_code))]
        pub dedicated: u64,
        /// Программный растеризатор («Microsoft Basic Render Driver»).
        pub software: bool,
    }

    /// Адаптер, которым рисует окно, — или `None`, если он не нашёлся
    /// однозначно.
    ///
    /// Программные вычёркиваются до выбора: у счётчиков у них свой LUID и
    /// свои движки, и без этого «видеокарта одна» не выполнялось бы ни на
    /// одной машине (см. заголовок модуля).
    pub fn pick_adapter(adapters: &[Adapter], target: Option<PciId>) -> Option<Adapter> {
        let hardware: Vec<Adapter> = adapters.iter().copied().filter(|a| !a.software).collect();
        let ids: Vec<(u32, u32)> = hardware.iter().map(|a| (a.vendor, a.device)).collect();
        choose(&ids, target).map(|index| hardware[index])
    }

    /// `luid_0x00000000_0x0000B869_phys_0` → LUID, номер физической части
    /// и остаток строки после неё (без разделителя).
    fn parse_adapter(name: &str) -> Option<(Luid, u32, &str)> {
        let rest = name.strip_prefix("luid_0x")?;
        let (high, rest) = rest.split_once("_0x")?;
        let (low, rest) = rest.split_once("_phys_")?;
        let (phys, rest) = rest.split_once('_').unwrap_or((rest, ""));
        Some((
            Luid {
                high: hex(high)?,
                low: hex(low)?,
            },
            phys.parse().ok()?,
            rest,
        ))
    }

    /// Шестнадцатеричное число без знака и без пустоты: `from_str_radix`
    /// сам по себе принимает и `+1F`.
    fn hex(text: &str) -> Option<u32> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(text, 16).ok()
    }

    /// Движок видеокарты: чей он и какой по счёту.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Engine {
        pub luid: Luid,
        pub phys: u32,
        pub eng: u32,
    }

    /// `pid_10340_luid_0x00000000_0x0000B869_phys_0_eng_0_engtype_3D` → движок.
    ///
    /// Номер процесса разбирается, но не хранится: процессы внутри движка
    /// складываются, и различать их незачем. Тип движка (`3D`, `Copy`,
    /// `VideoDecode`) тоже: видеокарте ставится самый занятый движок любого
    /// типа, а тип бывает и пустым (`engtype_`) — у части драйверов
    /// движки без имени.
    pub fn parse_engine(name: &str) -> Option<Engine> {
        let rest = name.strip_prefix("pid_")?;
        let (pid, rest) = rest.split_once('_')?;
        pid.parse::<u32>().ok()?;
        let (luid, phys, rest) = parse_adapter(rest)?;
        let rest = rest.strip_prefix("eng_")?;
        let (eng, rest) = rest.split_once('_')?;
        rest.strip_prefix("engtype_")?;
        Some(Engine {
            luid,
            phys,
            eng: eng.parse().ok()?,
        })
    }

    /// Загрузка видеокарты по счётчикам её движков — как у диспетчера задач.
    ///
    /// Процессы внутри одного движка складываются, а видеокарте ставится
    /// самый занятый движок. Сложи всё подряд — выйдет больше ста; возьми
    /// один `3D` — видеокарта, декодирующая ролик, покажет простой.
    ///
    /// Живёт между замерами ради одного списка: он чистится, а не
    /// заводится заново каждую секунду.
    pub struct Busiest {
        luid: Luid,
        /// Сумма по процессам на каждый движок: `(phys, eng, сумма)`.
        /// Движков у видеокарты с десяток, так что поиск перебором дешевле
        /// любой таблицы.
        engines: Vec<(u32, u32, f64)>,
    }

    impl Busiest {
        pub fn new(luid: Luid) -> Self {
            Self {
                luid,
                engines: Vec::new(),
            }
        }

        pub fn clear(&mut self) {
            self.engines.clear();
        }

        /// Один экземпляр счётчика. `None` вместо значения — у экземпляра нет
        /// числа (процесс появился только что): пропускаем, а не считаем нулём.
        pub fn add(&mut self, instance: &str, value: Option<f64>) {
            let Some(value) = value.filter(|v| v.is_finite()) else {
                return;
            };
            // Чужой LUID не смешивается: шаблон в пути уже отбирает нашу
            // видеокарту, но проверка здесь дешевле веры в то, как PDH
            // понимает звёздочку посреди имени.
            let Some(engine) = parse_engine(instance).filter(|e| e.luid == self.luid) else {
                return;
            };
            let value = value.max(0.0);
            match self
                .engines
                .iter_mut()
                .find(|(phys, eng, _)| (*phys, *eng) == (engine.phys, engine.eng))
            {
                Some((_, _, sum)) => *sum += value,
                None => self.engines.push((engine.phys, engine.eng, value)),
            }
        }

        /// Доля самого занятого движка, не больше ста. `None` — ни одного
        /// числа про нашу видеокарту не пришло.
        ///
        /// Потолок нужен не для красоты: доли снимаются не в один миг, и на
        /// занятом движке выходят за сотню — `Get-Counter`, который потолка
        /// не ставит, показал 105 у одного процесса под полной нагрузкой
        /// (2026-09-29). Сам Savio форматирует без `PDH_FMT_NOCAP100`, то
        /// есть сотней ограничен каждый экземпляр, но не их сумма.
        pub fn finish(&self) -> Option<f32> {
            self.engines
                .iter()
                .map(|&(_, _, sum)| sum)
                .reduce(f64::max)
                .map(|busiest| busiest.clamp(0.0, 100.0) as f32)
        }
    }

    /// Занятая память видеокарты по счётчику `GPU Adapter Memory`.
    ///
    /// Экземпляр там — физическая часть адаптера (`…_phys_0`); у связанных
    /// видеокарт их несколько, и память складывается.
    pub struct Usage {
        luid: Luid,
        used: u64,
        seen: bool,
    }

    impl Usage {
        pub fn new(luid: Luid) -> Self {
            Self {
                luid,
                used: 0,
                seen: false,
            }
        }

        pub fn clear(&mut self) {
            self.used = 0;
            self.seen = false;
        }

        pub fn add(&mut self, instance: &str, value: Option<i64>) {
            let Some(value) = value else {
                return;
            };
            let Some((luid, _, rest)) = parse_adapter(instance) else {
                return;
            };
            // Остаток должен быть пуст: у счётчика памяти имя кончается
            // номером физической части.
            if luid != self.luid || !rest.is_empty() {
                return;
            }
            self.used = self.used.saturating_add(value.max(0) as u64);
            self.seen = true;
        }

        pub fn finish(&self) -> Option<u64> {
            self.seen.then_some(self.used)
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: сами вызовы
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;

    use super::pdh::{Adapter, Busiest, Luid, Usage, pick_adapter};
    use super::{Reading, VideoMemory};
    use crate::model::PciId;

    type Handle = *mut c_void;

    // Коды возврата и флаги — свои имена, как в `power`: ради дюжины чисел
    // крейт `windows` не тянем, а сами числа не менялись со времён Vista.
    const ERROR_SUCCESS: u32 = 0;
    const PDH_CSTATUS_NEW_DATA: u32 = 1;
    const PDH_MORE_DATA: u32 = 0x8000_07D2;
    const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
    const PDH_FMT_LARGE: u32 = 0x0000_0400;
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

    /// Сколько адаптеров готовы перечислить. Потолок по Правилу 1, как
    /// `PLAN_LIMIT` в `power`: конец цикла задаём не только мы.
    const ADAPTER_LIMIT: u32 = 16;

    /// Сколько байт готовы отдать под ответ счётчика. На обычной машине это
    /// десятки килобайт (триста процессов на восемь движков); потолок —
    /// защита от ответа, который выделил бы гигабайт.
    const BUFFER_LIMIT: u32 = 16 << 20;

    /// Сколько знаков имени экземпляра читаем. Настоящее имя — полсотни.
    const NAME_LIMIT: usize = 256;

    /// `IID_IDXGIFactory1`.
    const IID_IDXGI_FACTORY1: Guid = Guid {
        d1: 0x770a_ae78,
        d2: 0xf26f,
        d3: 0x4dba,
        d4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    };

    // Номера в таблицах методов COM. Считаются от начала с учётом всех
    // предков: IUnknown (3), IDXGIObject (4), затем сам интерфейс.
    const SLOT_RELEASE: usize = 2;
    /// `IDXGIFactory1::EnumAdapters1`: IDXGIFactory занимает 7–11.
    const SLOT_ENUM_ADAPTERS1: usize = 12;
    /// `IDXGIAdapter1::GetDesc1`: IDXGIAdapter занимает 7–9.
    const SLOT_GET_DESC1: usize = 10;

    #[repr(C)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    /// `DXGI_ADAPTER_DESC1`. `LUID` — это `{ DWORD LowPart; LONG HighPart; }`,
    /// выравнивание у него четыре, поэтому двумя полями подряд раскладка та же.
    #[repr(C)]
    struct AdapterDesc1 {
        description: [u16; 128],
        vendor_id: u32,
        device_id: u32,
        sub_sys_id: u32,
        revision: u32,
        dedicated_video_memory: usize,
        dedicated_system_memory: usize,
        shared_system_memory: usize,
        luid_low: u32,
        luid_high: i32,
        flags: u32,
    }

    /// `PDH_FMT_COUNTERVALUE`: состояние и объединение из восьми байт.
    /// Отдельной структурой, а не двумя полями элемента: у объединения
    /// выравнивание восемь, и на 32-битной Windows оно отодвигает значение
    /// от указателя на имя — плоская раскладка там разъехалась бы.
    #[repr(C)]
    struct FmtValue {
        status: u32,
        /// `double` при `PDH_FMT_DOUBLE`, `LONGLONG` при `PDH_FMT_LARGE`.
        bits: u64,
    }

    /// `PDH_FMT_COUNTERVALUE_ITEM_W`.
    #[repr(C)]
    struct FmtItem {
        name: *const u16,
        value: FmtValue,
    }

    type OpenQueryFn = unsafe extern "system" fn(*const u16, usize, *mut Handle) -> u32;
    type AddCounterFn = unsafe extern "system" fn(Handle, *const u16, usize, *mut Handle) -> u32;
    type CollectFn = unsafe extern "system" fn(Handle) -> u32;
    type ArrayFn = unsafe extern "system" fn(Handle, u32, *mut u32, *mut u32, *mut FmtItem) -> u32;
    type CloseQueryFn = unsafe extern "system" fn(Handle) -> u32;
    type CreateFactoryFn = unsafe extern "system" fn(*const Guid, *mut *mut c_void) -> i32;
    type EnumAdapters1Fn = unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
    type GetDesc1Fn = unsafe extern "system" fn(*mut c_void, *mut AdapterDesc1) -> i32;
    type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, reserved: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
    }

    /// Функции `pdh.dll`.
    struct Pdh {
        open: OpenQueryFn,
        add: AddCounterFn,
        collect: CollectFn,
        array: ArrayFn,
        close: CloseQueryFn,
    }

    /// Загружает библиотеку из System32 — и только оттуда.
    ///
    /// `LOAD_LIBRARY_SEARCH_SYSTEM32` по той же причине, что в `power`:
    /// портативная поставка Savio лежит в чужой папке, и одноимённая
    /// библиотека рядом с exe иначе загрузилась бы вместо системной.
    /// Выгружать её незачем: нужна она до конца работы процесса.
    fn system_library(name: &str) -> Option<*mut c_void> {
        const SEARCH_SYSTEM32: u32 = 0x0000_0800;
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { LoadLibraryExW(wide.as_ptr(), null_mut(), SEARCH_SYSTEM32) };
        (!module.is_null()).then_some(module)
    }

    /// Адрес функции; `Option` в типе результата — не украшение: у указателя
    /// на функцию нулевое значение запрещено, и нулём система как раз
    /// говорит «такой функции нет».
    fn symbol(module: *mut c_void, name: &[u8]) -> *const c_void {
        debug_assert!(name.ends_with(b"\0"));
        unsafe { GetProcAddress(module, name.as_ptr()) }
    }

    fn pdh() -> Option<&'static Pdh> {
        static PDH: OnceLock<Option<Pdh>> = OnceLock::new();
        PDH.get_or_init(|| {
            let module = system_library("pdh.dll")?;
            // Каждый `transmute` превращает адрес в `Option` указателя на
            // функцию: нулевой адрес становится `None`, а не неопределённым
            // поведением.
            unsafe {
                Some(Pdh {
                    open: std::mem::transmute::<*const c_void, Option<OpenQueryFn>>(symbol(
                        module,
                        b"PdhOpenQueryW\0",
                    ))?,
                    add: std::mem::transmute::<*const c_void, Option<AddCounterFn>>(symbol(
                        module,
                        b"PdhAddEnglishCounterW\0",
                    ))?,
                    collect: std::mem::transmute::<*const c_void, Option<CollectFn>>(symbol(
                        module,
                        b"PdhCollectQueryData\0",
                    ))?,
                    array: std::mem::transmute::<*const c_void, Option<ArrayFn>>(symbol(
                        module,
                        b"PdhGetFormattedCounterArrayW\0",
                    ))?,
                    close: std::mem::transmute::<*const c_void, Option<CloseQueryFn>>(symbol(
                        module,
                        b"PdhCloseQuery\0",
                    ))?,
                })
            }
        })
        .as_ref()
    }

    fn create_factory() -> Option<CreateFactoryFn> {
        static CREATE: OnceLock<Option<CreateFactoryFn>> = OnceLock::new();
        *CREATE.get_or_init(|| {
            let module = system_library("dxgi.dll")?;
            unsafe {
                std::mem::transmute::<*const c_void, Option<CreateFactoryFn>>(symbol(
                    module,
                    b"CreateDXGIFactory1\0",
                ))
            }
        })
    }

    /// Указатель на объект COM, который отпускается сам.
    struct Com(*mut c_void);

    impl Com {
        /// Метод из таблицы объекта. `None` — в таблице пусто.
        fn method(&self, slot: usize) -> *const c_void {
            // Объект COM начинается с указателя на таблицу методов, а таблица —
            // массив указателей на функции.
            unsafe {
                let table = *self.0.cast::<*const *const c_void>();
                *table.add(slot)
            }
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            let release = unsafe {
                std::mem::transmute::<*const c_void, Option<ReleaseFn>>(self.method(SLOT_RELEASE))
            };
            if let Some(release) = release {
                unsafe { release(self.0) };
            }
        }
    }

    /// Видеокарты, как их видит DXGI. `None` — DXGI не ответил.
    ///
    /// Фабрика заводится без `CoInitialize`: DXGI его не требует, а поток
    /// опроса с COM-квартирами иначе пришлось бы знакомить ради одного
    /// перечисления.
    fn adapters() -> Option<Vec<Adapter>> {
        let create = create_factory()?;
        let mut raw = null_mut();
        if unsafe { create(&IID_IDXGI_FACTORY1, &mut raw) } < 0 || raw.is_null() {
            return None;
        }
        let factory = Com(raw);
        let enum_adapters = unsafe {
            std::mem::transmute::<*const c_void, Option<EnumAdapters1Fn>>(
                factory.method(SLOT_ENUM_ADAPTERS1),
            )
        }?;

        let mut list = Vec::new();
        for index in 0..ADAPTER_LIMIT {
            let mut raw = null_mut();
            // Конец списка — `DXGI_ERROR_NOT_FOUND`, отрицательный код.
            if unsafe { enum_adapters(factory.0, index, &mut raw) } < 0 || raw.is_null() {
                break;
            }
            let adapter = Com(raw);
            let Some(get_desc) = (unsafe {
                std::mem::transmute::<*const c_void, Option<GetDesc1Fn>>(
                    adapter.method(SLOT_GET_DESC1),
                )
            }) else {
                continue;
            };
            let mut desc = AdapterDesc1 {
                description: [0; 128],
                vendor_id: 0,
                device_id: 0,
                sub_sys_id: 0,
                revision: 0,
                dedicated_video_memory: 0,
                dedicated_system_memory: 0,
                shared_system_memory: 0,
                luid_low: 0,
                luid_high: 0,
                flags: 0,
            };
            if unsafe { get_desc(adapter.0, &mut desc) } < 0 {
                continue;
            }
            list.push(Adapter {
                vendor: desc.vendor_id,
                device: desc.device_id,
                luid: Luid {
                    high: desc.luid_high as u32,
                    low: desc.luid_low,
                },
                dedicated: desc.dedicated_video_memory as u64,
                software: desc.flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0,
            });
        }
        Some(list)
    }

    /// Опрос видеокарты. Запрос PDH открывается один раз — при заведении —
    /// и закрывается вместе с опросом.
    pub struct Probe {
        /// `None` — спрашивать нечего: видеокарта не нашлась однозначно или
        /// счётчиков в системе нет.
        query: Option<Query>,
    }

    impl Probe {
        pub fn new(target: Option<PciId>) -> Self {
            Self {
                query: Query::open(target),
            }
        }

        pub fn take(&mut self) -> Reading {
            self.query
                .as_mut()
                .map_or_else(Reading::default, Query::take)
        }
    }

    struct Query {
        api: &'static Pdh,
        handle: Handle,
        /// Счётчик движков. `None` — система его не знает.
        engines: Option<Handle>,
        /// Счётчик памяти и вся своя память видеокарты. `None` — счётчика
        /// нет или своей памяти у видеокарты нет вовсе (встроенная).
        memory: Option<(Handle, u64)>,
        busiest: Busiest,
        usage: Usage,
        /// Под ответ счётчика. `u64`, а не байты: элементы ответа выровнены
        /// по восьми.
        buffer: Vec<u64>,
        /// Имя очередного экземпляра, раскодированное из UTF-16. Одно на
        /// весь опрос: экземпляров сотни, и строка на каждый — сотни
        /// аллокаций в секунду.
        name: String,
    }

    impl Query {
        fn open(target: Option<PciId>) -> Option<Self> {
            let adapter = pick_adapter(&adapters()?, target)?;
            let api = pdh()?;

            let mut handle = null_mut();
            if unsafe { (api.open)(null(), 0, &mut handle) } != ERROR_SUCCESS {
                return None;
            }
            // С этой строки запрос закрывает `Drop`, и ранний выход ниже
            // его не упустит.
            let mut query = Self {
                api,
                handle,
                engines: None,
                memory: None,
                busiest: Busiest::new(adapter.luid),
                usage: Usage::new(adapter.luid),
                buffer: Vec::new(),
                name: String::new(),
            };

            // Звёздочка посреди имени отбирает экземпляры одной видеокарты:
            // без неё каждый процесс нёс бы ещё и тринадцать движков
            // программного растеризатора. Новые процессы шаблон подхватывает
            // сам (проверено вживую), так что открыть его один раз — хватит.
            let luid = adapter.luid.instance();
            query.engines = query.add(&format!(
                "\\GPU Engine(*{luid}_phys*)\\Utilization Percentage"
            ));
            if adapter.dedicated > 0 {
                query.memory = query
                    .add(&format!("\\GPU Adapter Memory(*{luid}_phys*)\\Dedicated Usage"))
                    .map(|counter| (counter, adapter.dedicated));
            }
            if query.engines.is_none() && query.memory.is_none() {
                return None;
            }

            // Нулевая точка: скоростной счётчик отдаёт долю со второго сбора.
            unsafe { (api.collect)(query.handle) };
            Some(query)
        }

        fn add(&self, path: &str) -> Option<Handle> {
            let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            let mut counter = null_mut();
            let status = unsafe { (self.api.add)(self.handle, wide.as_ptr(), 0, &mut counter) };
            (status == ERROR_SUCCESS && !counter.is_null()).then_some(counter)
        }

        fn take(&mut self) -> Reading {
            // Не собралось — не читаем вовсе: массив отдал бы числа прошлого
            // сбора, и застывшая доля выглядела бы живой.
            if unsafe { (self.api.collect)(self.handle) } != ERROR_SUCCESS {
                return Reading::default();
            }

            let api = self.api;
            let Self {
                engines,
                memory,
                busiest,
                usage,
                buffer,
                name,
                ..
            } = self;

            let load = engines.and_then(|counter| {
                busiest.clear();
                let answered = read_array(api, counter, PDH_FMT_DOUBLE, buffer, name, |at, bits| {
                    busiest.add(at, bits.map(f64::from_bits));
                });
                answered.then(|| busiest.finish()).flatten()
            });

            let memory = memory.and_then(|(counter, total)| {
                usage.clear();
                let answered = read_array(api, counter, PDH_FMT_LARGE, buffer, name, |at, bits| {
                    usage.add(at, bits.map(|bits| bits as i64));
                });
                answered
                    .then(|| usage.finish())
                    .flatten()
                    .map(|used| VideoMemory {
                        used,
                        total: Some(total),
                    })
            });

            Reading { load, memory }
        }
    }

    impl Drop for Query {
        fn drop(&mut self) {
            unsafe { (self.api.close)(self.handle) };
        }
    }

    /// Читает ответ счётчика и отдаёт каждый экземпляр: имя и значение,
    /// `None` — у экземпляра нет числа. `false` — ответа нет вовсе.
    ///
    /// Размер спрашивается пустым вызовом каждый раз, а не угадывается по
    /// прошлому: документация прямо запрещает верить размеру, который
    /// вернулся на слишком маленький буфер.
    fn read_array(
        api: &Pdh,
        counter: Handle,
        format: u32,
        buffer: &mut Vec<u64>,
        name: &mut String,
        mut each: impl FnMut(&str, Option<u64>),
    ) -> bool {
        let mut size = 0u32;
        let mut count = 0u32;
        let status = unsafe { (api.array)(counter, format, &mut size, &mut count, null_mut()) };
        if status == ERROR_SUCCESS {
            // Экземпляров нет вовсе.
            return true;
        }
        // `PDH_NO_DATA` (такой видеокарты нет) и `PDH_CSTATUS_INVALID_DATA`
        // (первый сбор скоростного счётчика) приходят сюда же.
        if status != PDH_MORE_DATA || size > BUFFER_LIMIT {
            return false;
        }

        let words = (size as usize).div_ceil(size_of::<u64>());
        if buffer.len() < words {
            buffer.resize(words, 0);
        }
        let mut size = u32::try_from(buffer.len() * size_of::<u64>()).unwrap_or(u32::MAX);
        let status = unsafe {
            (api.array)(
                counter,
                format,
                &mut size,
                &mut count,
                buffer.as_mut_ptr().cast(),
            )
        };
        if status != ERROR_SUCCESS {
            return false;
        }

        // Ответ — массив элементов, а за ним имена; указатели в элементах
        // смотрят в тот же буфер. Всё, что за его пределами, не читаем.
        let used = (size as usize).min(buffer.len() * size_of::<u64>());
        let count = (count as usize).min(used / size_of::<FmtItem>());
        let start = buffer.as_ptr() as usize;
        let end = start + used;
        let items = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<FmtItem>(), count) };

        for item in items {
            let at = item.name as usize;
            if at < start || at >= end || !(at - start).is_multiple_of(2) {
                continue;
            }
            let room = ((end - at) / 2).min(NAME_LIMIT);
            let units = unsafe { std::slice::from_raw_parts(item.name, room) };
            let len = units.iter().position(|&unit| unit == 0).unwrap_or(room);

            name.clear();
            name.extend(
                char::decode_utf16(units[..len].iter().copied())
                    .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
            );
            let valid = matches!(item.value.status, ERROR_SUCCESS | PDH_CSTATUS_NEW_DATA);
            each(name.as_str(), valid.then_some(item.value.bits));
        }
        true
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Раскладка структур совпадает с заголовками Windows.
        ///
        /// Разъехавшаяся раскладка не падает, а читает чужие байты: LUID из
        /// соседнего поля — и вместо загрузки прочерк навсегда (Правило 6).
        /// Числа посчитаны по объявлениям SDK для 64-битной сборки и сверены
        /// вживую: `real_gpu_load_from_this_machine` читает ими те же
        /// `VendorId` и LUID, что показывают `Win32_VideoController` и имена
        /// экземпляров PDH.
        ///
        /// Меряются смещения и размеры полей, а не один размер структуры:
        /// ошибка в четыре байта тонет в выравнивании. Проверено: без поля
        /// `revision` и с `u32` вместо `usize` у `dedicated_video_memory`
        /// размер и все смещения после них остаются прежними — а во втором
        /// случае память видеокарты в пять гигабайт читалась бы как 0.9.
        /// Красной проверку делает размер поля (с `u32` она падает).
        #[test]
        #[cfg(target_pointer_width = "64")]
        fn layouts_match_the_windows_headers() {
            use std::mem::offset_of;

            /// Размер поля: `size_of` у поля напрямую не спросить.
            fn field<T, F>(_: fn(&T) -> &F) -> usize {
                size_of::<F>()
            }

            assert_eq!(size_of::<AdapterDesc1>(), 312);
            assert_eq!(offset_of!(AdapterDesc1, vendor_id), 256);
            assert_eq!(offset_of!(AdapterDesc1, revision), 268);
            assert_eq!(offset_of!(AdapterDesc1, dedicated_video_memory), 272);
            assert_eq!(offset_of!(AdapterDesc1, luid_low), 296);
            assert_eq!(offset_of!(AdapterDesc1, flags), 304);
            assert_eq!(field(|d: &AdapterDesc1| &d.dedicated_video_memory), 8);
            assert_eq!(field(|d: &AdapterDesc1| &d.luid_high), 4);

            assert_eq!(size_of::<FmtItem>(), 24);
            assert_eq!(offset_of!(FmtItem, value), 8);
            assert_eq!(offset_of!(FmtValue, bits), 8);
            assert_eq!(field(|v: &FmtValue| &v.bits), 8);
        }

        /// Настоящие видеокарты этой машины и загрузка каждой.
        ///
        /// Помечен `#[ignore]`: спрашивает живую систему и занимает секунду
        /// на видеокарту. Запускать руками —
        /// `cargo test -- --ignored --nocapture real_gpu_load_from_this_machine`.
        ///
        /// Проверяет то, чего не видит ни один чистый тест, — что LUID из
        /// DXGI и путь счётчика сошлись: при ошибке в порядке половин или
        /// в регистре цифр PDH молча отвечает «нет данных» (см. заголовок
        /// модуля), и загрузки не было бы ни у одной карты.
        #[test]
        #[ignore = "спрашивает живую систему и занимает секунду на видеокарту"]
        fn real_gpu_load_from_this_machine() {
            let adapters = adapters().expect("DXGI не ответил");
            for adapter in &adapters {
                println!("{adapter:?}");
            }
            let hardware: Vec<&Adapter> = adapters.iter().filter(|a| !a.software).collect();
            assert!(!hardware.is_empty(), "у машины нет ни одной настоящей видеокарты");

            for adapter in hardware {
                let target = PciId {
                    vendor: adapter.vendor,
                    device: adapter.device,
                };
                let mut probe = Probe::new(Some(target));
                std::thread::sleep(crate::engine::monitor::INTERVAL);
                let reading = probe.take();
                println!("{target:?}: {reading:?}");

                let load = reading.load.expect("у видеокарты нет загрузки");
                assert!((0.0..=100.0).contains(&load), "загрузка {load} вне шкалы");
                if adapter.dedicated > 0 {
                    let memory = reading.memory.expect("у видеокарты нет памяти");
                    assert!(memory.used <= adapter.dedicated, "занято больше, чем есть");
                }

                // Цена замера — ради Правила 1: опрос идёт каждую секунду,
                // пока открыт монитор.
                let started = std::time::Instant::now();
                for _ in 0..10 {
                    probe.take();
                }
                println!("один замер: {:?}", started.elapsed() / 10);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: sysfs
//
// Под тестами собирается на любой системе: это чтение обычных файлов, и
// дерево каталогов для проверки заводится во временной папке.
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", test))]
mod sysfs {
    use std::path::{Path, PathBuf};

    use super::{Reading, VideoMemory, choose};
    use crate::model::PciId;

    /// Сколько записей `/sys/class/drm` готовы перебрать. Потолок по
    /// Правилу 1: на обычной машине их с десяток (карты, их разъёмы,
    /// узлы отрисовки).
    const ENTRY_LIMIT: usize = 256;

    /// Каталог устройства видеокарты, которой рисует окно, — или `None`.
    ///
    /// Кандидаты — только `cardN`: рядом лежат их разъёмы (`card0-DP-1`)
    /// и узлы отрисовки (`renderD128`), и это не отдельные видеокарты.
    /// Карта без чисел PCI (`simpledrm` до загрузки настоящего драйвера)
    /// видеокартой для выбора не считается.
    pub fn find_card(root: &Path, target: Option<PciId>) -> Option<PathBuf> {
        let mut cards: Vec<(PathBuf, (u32, u32))> = Vec::new();
        for entry in std::fs::read_dir(root).ok()?.flatten().take(ENTRY_LIMIT) {
            let file_name = entry.file_name();
            if !file_name.to_str().is_some_and(is_card) {
                continue;
            }
            let device = entry.path().join("device");
            let vendor = read(&device.join("vendor")).and_then(|text| parse_hex(&text));
            let model = read(&device.join("device")).and_then(|text| parse_hex(&text));
            if let (Some(vendor), Some(model)) = (vendor, model) {
                cards.push((device, (vendor, model)));
            }
        }

        let ids: Vec<(u32, u32)> = cards.iter().map(|(_, id)| *id).collect();
        let index = choose(&ids, target)?;
        Some(cards.swap_remove(index).0)
    }

    /// Показания одной карты.
    ///
    /// `gpu_busy_percent` есть только у `amdgpu`; нет файла — нет и числа,
    /// и это не ошибка, а «нет данных». Читается он каждый замер заново:
    /// sysfs отдаёт свежее значение на каждое открытие.
    pub fn read_card(device: &Path) -> Reading {
        let load = read(&device.join("gpu_busy_percent"))
            .and_then(|text| parse_count(&text))
            .map(|percent| percent.min(100) as f32);
        let memory = read(&device.join("mem_info_vram_used"))
            .and_then(|text| parse_count(&text))
            .map(|used| VideoMemory {
                used,
                total: read(&device.join("mem_info_vram_total"))
                    .and_then(|text| parse_count(&text))
                    .filter(|&total| total > 0),
            });
        Reading { load, memory }
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    /// `card0` — да, `card0-DP-1` и `renderD128` — нет.
    pub fn is_card(name: &str) -> bool {
        name.strip_prefix("card")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    }

    /// `0x1002\n` → 0x1002.
    pub fn parse_hex(text: &str) -> Option<u32> {
        let text = text.trim();
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(digits, 16).ok()
    }

    /// `37\n` → 37. Знак и пустота — не число.
    pub fn parse_count(text: &str) -> Option<u64> {
        let text = text.trim();
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};

    use super::Reading;
    use super::sysfs::{find_card, read_card};
    use crate::model::PciId;

    const DRM_ROOT: &str = "/sys/class/drm";

    /// Опрос видеокарты через sysfs.
    ///
    /// Каталог карты находится однажды: видеокарта у окна одна на всю жизнь
    /// процесса, а перебирать `/sys/class/drm` каждую секунду незачем.
    /// Читается только карта, которой рисует окно, и это важно для батареи:
    /// у старых ядер чтение `gpu_busy_percent` будит заснувшую дискретную
    /// карту, но эта и так не спит — на ней рисуется Savio.
    pub struct Probe {
        device: Option<PathBuf>,
    }

    impl Probe {
        pub fn new(target: Option<PciId>) -> Self {
            Self {
                device: find_card(Path::new(DRM_ROOT), target),
            }
        }

        pub fn take(&mut self) -> Reading {
            self.device
                .as_deref()
                .map_or_else(Reading::default, read_card)
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod elsewhere {
    use super::Reading;
    use crate::model::PciId;

    /// Опрос видеокарты там, где Savio её не спрашивает, — на macOS.
    ///
    /// Не недоделка, а решение (см. заголовок модуля): IOKit здесь — это
    /// небезопасный FFI, который не на чем запустить до выпуска, и прочерк
    /// честнее числа, ни разу не виденного живым.
    pub struct Probe;

    impl Probe {
        pub fn new(_target: Option<PciId>) -> Self {
            Self
        }

        pub fn take(&mut self) -> Reading {
            Reading::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pdh::{Adapter, Busiest, Luid, Usage, parse_engine, pick_adapter};
    use super::sysfs::{find_card, is_card, parse_count, parse_hex, read_card};
    use super::*;
    use crate::model::PciId;

    const OURS: Luid = Luid {
        high: 0,
        low: 0xB869,
    };
    const WARP: Luid = Luid {
        high: 0,
        low: 0xD238,
    };

    const NVIDIA: PciId = PciId {
        vendor: 0x10DE,
        device: 0x1C31,
    };
    const INTEL: PciId = PciId {
        vendor: 0x8086,
        device: 0x9BC4,
    };

    /// Имя экземпляра разбирается ровно в том виде, в каком его отдаёт PDH.
    ///
    /// Строки сняты с живой машины (2026-09-29): регистр у PDH смешанный —
    /// префиксы строчные, цифры LUID прописные, тип движка как придётся.
    ///
    /// Проверено красным: без своей проверки цифр `from_str_radix` принимает
    /// «+0000000», и испорченное имя разбирается как движок.
    #[test]
    fn an_engine_instance_name_is_read_the_way_pdh_writes_it() {
        assert_eq!(
            parse_engine("pid_10340_luid_0x00000000_0x0000B869_phys_0_eng_0_engtype_3D"),
            Some(pdh::Engine {
                luid: OURS,
                phys: 0,
                eng: 0,
            })
        );
        assert_eq!(
            parse_engine("pid_7_luid_0x00000001_0x0000d238_phys_1_eng_12_engtype_VideoDecode"),
            Some(pdh::Engine {
                luid: Luid {
                    high: 1,
                    low: 0xD238,
                },
                phys: 1,
                eng: 12,
            })
        );
        // Движок без имени типа у части драйверов — законный экземпляр.
        assert!(parse_engine("pid_7_luid_0x00000000_0x0000B869_phys_0_eng_3_engtype_").is_some());

        for broken in [
            "",
            "_Total",
            "luid_0x00000000_0x0000B869_phys_0",
            "pid__luid_0x00000000_0x0000B869_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x_0x0000B869_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x+0000000_0x0000B869_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x00000000_0x0000B869_phys_0_eng_x_engtype_3D",
            "pid_1_luid_0x00000000_0x0000B869_phys_0_eng_0",
        ] {
            assert_eq!(parse_engine(broken), None, "«{broken}» разобрано как движок");
        }
    }

    /// LUID пишется в путь счётчика так же, как PDH пишет его в имени.
    ///
    /// Ошибка здесь не падает: PDH принимает путь с чужим LUID и молча
    /// отвечает «нет данных» — загрузки не было бы никогда.
    ///
    /// Проверено красным: с половинами наоборот проверка падает. У живой
    /// машины старшая половина — ноль, так что порядок сверен и там: DXGI
    /// назвал `HighPart = 0`, `LowPart = 0xB869`, а PDH — `0x00000000_0x0000B869`.
    #[test]
    fn a_luid_is_written_the_way_pdh_names_it() {
        assert_eq!(OURS.instance(), "luid_0x00000000_0x0000B869");
        assert_eq!(
            Luid {
                high: 0xFFFF_FFFF,
                low: 0x1A,
            }
            .instance(),
            "luid_0xFFFFFFFF_0x0000001A"
        );
    }

    fn engine(pid: u32, luid: Luid, eng: u32, kind: &str) -> String {
        format!(
            "pid_{pid}_luid_0x{:08X}_0x{:08X}_phys_0_eng_{eng}_engtype_{kind}",
            luid.high, luid.low
        )
    }

    /// Процессы внутри движка складываются, а видеокарте ставится самый
    /// занятый движок.
    ///
    /// Проверено красным: со сложением всех движков подряд (как «просто
    /// сумма») проверка падает — выходит 95, а не 45.
    #[test]
    fn the_load_is_the_busiest_engine_summed_over_processes() {
        let mut busiest = Busiest::new(OURS);
        busiest.add(&engine(1, OURS, 0, "3D"), Some(10.0));
        busiest.add(&engine(2, OURS, 0, "3D"), Some(20.0));
        busiest.add(&engine(1, OURS, 2, "VideoDecode"), Some(45.0));
        busiest.add(&engine(3, OURS, 4, "Copy"), Some(20.0));
        assert_eq!(busiest.finish(), Some(45.0));

        // Три процесса на одном движке перевешивают декодер.
        busiest.add(&engine(4, OURS, 0, "3D"), Some(25.0));
        assert_eq!(busiest.finish(), Some(55.0));
    }

    /// Доля не выходит за сотню: сумма долей процессов, снятых не в один
    /// миг, бывает на пару процентов выше.
    ///
    /// Проверено красным: без потолка выходит 103.
    #[test]
    fn the_load_never_goes_past_a_hundred() {
        let mut busiest = Busiest::new(OURS);
        busiest.add(&engine(1, OURS, 0, "3D"), Some(60.0));
        busiest.add(&engine(2, OURS, 0, "3D"), Some(43.0));
        assert_eq!(busiest.finish(), Some(100.0));
    }

    /// Чужая видеокарта в загрузку нашей не попадает.
    ///
    /// Это не теория: программный растеризатор приходит в тех же счётчиках
    /// со своими тринадцатью движками `3D` у каждого процесса.
    ///
    /// Проверено красным: без сверки LUID выходит 95 — движки обеих карт
    /// сложились в один.
    #[test]
    fn another_adapter_is_not_mixed_in() {
        let mut busiest = Busiest::new(OURS);
        busiest.add(&engine(1, OURS, 0, "3D"), Some(5.0));
        busiest.add(&engine(1, WARP, 0, "3D"), Some(80.0));
        busiest.add(&engine(2, WARP, 0, "3D"), Some(10.0));
        assert_eq!(busiest.finish(), Some(5.0));

        let mut usage = Usage::new(OURS);
        usage.add("luid_0x00000000_0x0000B869_phys_0", Some(1_000));
        usage.add("luid_0x00000000_0x0000D238_phys_0", Some(7_000));
        assert_eq!(usage.finish(), Some(1_000));
    }

    /// Экземпляр без числа — не ноль, а пропуск; ни одного числа — «нет
    /// данных», а не простой.
    ///
    /// Первое — это процесс, появившийся только что: PDH отдаёт его
    /// с `PDH_CSTATUS_INVALID_DATA`. Второе — это Правило 6: ноль здесь
    /// законен (простаивающая карта), и выдать его, когда не пришло ничего,
    /// значило бы соврать о машине.
    ///
    /// Проверено красным: с `unwrap_or(0.0)` вместо пропуска `finish` отдаёт
    /// `Some(0.0)` на пустом движке.
    #[test]
    fn a_reading_without_a_number_is_not_a_zero() {
        let mut busiest = Busiest::new(OURS);
        assert_eq!(busiest.finish(), None, "без единого экземпляра вышло число");

        busiest.add(&engine(1, OURS, 0, "3D"), None);
        assert_eq!(busiest.finish(), None, "экземпляр без числа сосчитан нулём");

        busiest.add(&engine(2, OURS, 0, "3D"), Some(0.0));
        assert_eq!(busiest.finish(), Some(0.0), "простой не отличился от пустоты");

        busiest.clear();
        assert_eq!(busiest.finish(), None, "очистка оставила прошлые числа");

        let mut usage = Usage::new(OURS);
        usage.add("luid_0x00000000_0x0000B869_phys_0", None);
        assert_eq!(usage.finish(), None);

        // Память складывается по физическим частям, а очистка перед
        // следующим замером забывает прошлый: иначе занятое росло бы
        // каждую секунду.
        usage.add("luid_0x00000000_0x0000B869_phys_0", Some(1_000));
        usage.add("luid_0x00000000_0x0000B869_phys_1", Some(500));
        assert_eq!(usage.finish(), Some(1_500));
        usage.clear();
        assert_eq!(usage.finish(), None, "очистка оставила прошлую память");
    }

    /// Видеокарта выбирается по производителю и модели — и только
    /// однозначно.
    ///
    /// Проверено красным: если из двух одинаковых брать последнюю найденную,
    /// проверка падает (`Some(1)` вместо `None`).
    #[test]
    fn the_card_is_chosen_only_when_there_is_no_doubt() {
        let nvidia = (NVIDIA.vendor, NVIDIA.device);
        let intel = (INTEL.vendor, INTEL.device);

        assert_eq!(choose(&[intel, nvidia], Some(NVIDIA)), Some(1));
        // Той, которой рисует окно, в системе нет: не подставляем другую.
        assert_eq!(choose(&[intel], Some(NVIDIA)), None);
        // Две одинаковые — какая из них наша, не сказать.
        assert_eq!(choose(&[nvidia, nvidia], Some(NVIDIA)), None);
        // Адаптер своих чисел не назвал: карта одна — берём, две — нет.
        assert_eq!(choose(&[nvidia], None), Some(0));
        assert_eq!(choose(&[intel, nvidia], None), None);
        assert_eq!(choose(&[], None), None);
    }

    fn adapter(id: PciId, luid: Luid, software: bool) -> Adapter {
        Adapter {
            vendor: id.vendor,
            device: id.device,
            luid,
            dedicated: 0,
            software,
        }
    }

    /// Программный растеризатор видеокартой для выбора не считается.
    ///
    /// Ровно так устроена машина, на которой это писалось: DXGI отдаёт
    /// Quadro и «Microsoft Basic Render Driver». Считай его — и правило
    /// «карта одна» не сработало бы нигде.
    ///
    /// Проверено красным: без отсева программных `None` вместо Quadro.
    #[test]
    fn the_software_rasterizer_is_not_a_graphics_card() {
        let warp_id = PciId {
            vendor: 0x1414,
            device: 0x008C,
        };
        let adapters = [adapter(NVIDIA, OURS, false), adapter(warp_id, WARP, true)];

        assert_eq!(pick_adapter(&adapters, None).map(|a| a.luid), Some(OURS));
        assert_eq!(pick_adapter(&adapters, Some(NVIDIA)).map(|a| a.luid), Some(OURS));
        // Окно рисует сам растеризатор: загрузки настоящей карты под его
        // именем не показываем.
        assert_eq!(pick_adapter(&adapters, Some(warp_id)).map(|a| a.luid), None);
    }

    /// Числа sysfs разбираются строго: пустота и знак — не число.
    ///
    /// Проверено красным: без своей проверки цифр `str::parse` принимает
    /// «+5» как 5.
    #[test]
    fn sysfs_numbers_are_read_as_the_kernel_writes_them() {
        assert!(is_card("card0"));
        assert!(is_card("card12"));
        for name in ["card", "card0-DP-1", "card0-HDMI-A-1", "renderD128", "version", "cardX"] {
            assert!(!is_card(name), "«{name}» принят за видеокарту");
        }

        assert_eq!(parse_hex("0x1002\n"), Some(0x1002));
        assert_eq!(parse_hex("0x73bf"), Some(0x73BF));
        for broken in ["", "0x", "\n", "0x+12", "zz"] {
            assert_eq!(parse_hex(broken), None, "«{broken}» разобрано");
        }

        assert_eq!(parse_count("37\n"), Some(37));
        assert_eq!(parse_count("0\n"), Some(0));
        for broken in ["", "\n", "-1", "+5", "12%"] {
            assert_eq!(parse_count(broken), None, "«{broken}» разобрано");
        }
    }

    /// Временное дерево вида `/sys/class/drm`.
    struct FakeDrm(std::path::PathBuf);

    impl FakeDrm {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "savio-drm-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("не завести временный каталог");
            Self(root)
        }

        fn card(&self, name: &str, files: &[(&str, &str)]) {
            let device = self.0.join(name).join("device");
            std::fs::create_dir_all(&device).expect("не завести каталог карты");
            for (file, text) in files {
                std::fs::write(device.join(file), text).expect("не записать файл карты");
            }
        }
    }

    impl Drop for FakeDrm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Две видеокарты в Linux: читается та, которой рисует окно.
    ///
    /// Разъём `card0-DP-1` и узел `renderD128` лежат рядом с картами и
    /// видеокартами не считаются. У NVIDIA `gpu_busy_percent` нет — там
    /// «нет данных», а не ноль.
    ///
    /// Проверено красным: с `is_card`, принимающим всё, что начинается
    /// на `card`, разъём с теми же числами делает выбор неоднозначным,
    /// и карта AMD не находится.
    #[test]
    fn linux_reads_the_card_that_draws_the_window() {
        let amd = PciId {
            vendor: 0x1002,
            device: 0x73BF,
        };
        let drm = FakeDrm::new("two-cards");
        drm.card(
            "card0",
            &[
                ("vendor", "0x1002\n"),
                ("device", "0x73bf\n"),
                ("gpu_busy_percent", "37\n"),
                ("mem_info_vram_used", "1073741824\n"),
                ("mem_info_vram_total", "17163091968\n"),
            ],
        );
        drm.card("card0-DP-1", &[("vendor", "0x1002\n"), ("device", "0x73bf\n")]);
        drm.card("card1", &[("vendor", "0x10de\n"), ("device", "0x1c31\n")]);
        std::fs::create_dir_all(drm.0.join("renderD128")).expect("не завести узел");

        let card = find_card(&drm.0, Some(amd)).expect("карта AMD не нашлась");
        assert_eq!(
            read_card(&card),
            Reading {
                load: Some(37.0),
                memory: Some(VideoMemory {
                    used: 1_073_741_824,
                    total: Some(17_163_091_968),
                }),
            }
        );

        let card = find_card(&drm.0, Some(NVIDIA)).expect("карта NVIDIA не нашлась");
        assert_eq!(read_card(&card), Reading::default());

        // Без чисел адаптера из двух карт не выбираем никакую.
        assert_eq!(find_card(&drm.0, None), None);
        assert_eq!(find_card(&drm.0.join("нет-такого"), Some(amd)), None);
    }
}
