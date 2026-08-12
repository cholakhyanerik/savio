//! Питание: какая схема электропитания активна, в каком режиме работает
//! машина и как это переключить.
//!
//! Слой «Движок»: здесь спрашивают систему и просят её о переключении. Про
//! `egui` и виджеты модуль не знает ничего — наружу уезжает готовый
//! `PowerState`, а рисует его `app.rs`.
//!
//! # Почему системный вызов, а не `powercfg`
//!
//! Savio — обёртка над внешними программами, и `powercfg` напрашивался сам
//! собой. Но он не годится дважды.
//!
//! Во-первых, про режим питания (тот самый список из параметров Windows 11)
//! он не знает вовсе: `powercfg /overlaylist` отвечает «Invalid Parameters»,
//! а `/list` показывает три классические схемы — список, которого человек
//! в своих параметрах не видел.
//!
//! Во-вторых — и это Правило 6 в чистом виде — названия схем он портит молча.
//! Вывод перенаправленной консольной программы идёт не в UTF-8, а в кодовой
//! странице консоли, и всё, чего в ней нет, заменяется вопросительными
//! знаками. Проверено вживую (2026-08-12): схема, названная «Тест схемы яЁ»,
//! в выводе `powercfg /list` выглядит как `(???? ????? ??)`. Код возврата при
//! этом нулевой, строка на месте — просто имени в ней больше нет, и вернуть
//! его неоткуда. На русской Windows так пропали бы названия **всех** схем.
//!
//! `PowerReadFriendlyName` отдаёт то же имя в UTF-16, то есть без единой
//! потери, и заодно снимает разбор чужого текста, который меняется от версии
//! к версии и от языка к языку.
//!
//! # Что здесь undocumented и почему это допустимо
//!
//! Схемы (`PowerEnumerate`, `PowerReadFriendlyName`, `PowerGetActiveScheme`,
//! `PowerSetActiveScheme`) — документированный API. Режим питания
//! (`PowerGetEffectiveOverlayScheme`, `PowerGetActualOverlayScheme`,
//! `PowerSetActiveOverlayScheme`) не документирован, и другого пути к нему
//! нет: это единственное, чем ползунок «Режим питания» вообще управляется
//! снаружи. Отсюда `GetProcAddress` вместо линковки: отсутствие функции —
//! законный исход (Windows старее 10 версии 1803), и оно обязано кончаться
//! честным «система этого не умеет», а не отказом запуститься.
//!
//! # Правило 6 в этом модуле
//!
//! `PowerSetActiveOverlayScheme` возвращает успех всегда, а применяет режим
//! только при схеме «Сбалансированная». Проверено вживую (Windows 11,
//! обычные права, 2026-08-12): при активной «Высокая производительность»
//! запрошенный режим виден в `PowerGetActualOverlayScheme` и в реестре, а
//! `PowerGetEffectiveOverlayScheme` продолжает называть прежний. Поэтому
//! исход переключения определяется **перечитыванием**, а не кодом возврата,
//! и «запомнил, но не применил» — отдельный исход, а не успех.

use std::sync::mpsc::Sender;

use crate::model::{Event, NO_DOWNLOAD, PlanId, PowerMode};

/// Что просят переключить.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Схему электропитания.
    Plan(PlanId),
    /// Режим питания.
    Mode(PowerMode),
}

/// Чем кончилось переключение.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Применилось, и это подтверждено перечитыванием.
    Applied(String),
    /// Система приняла просьбу, но работает по-прежнему.
    Ignored(String),
}

/// Читает состояние питания в отдельном потоке.
///
/// В `ui()` этому места нет ни в каком виде: чтение идёт в чужую библиотеку,
/// и сколько времени займёт её ответ, Savio не решает (Правило 1).
pub fn start(tx: Sender<Event>, notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let _ = tx.send(Event::Power(read()));
        notify();
    });
}

/// Переключает и тут же перечитывает состояние.
///
/// Событий отправляется два: сначала исход словами, следом свежее состояние.
/// Порядок важен — состояние приезжает уже проверенным, и переключатель
/// в окне встаёт туда, где система его действительно оставила.
pub fn start_change(change: Change, tx: Sender<Event>, notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let _ = tx.send(match apply(change) {
            Ok(Outcome::Applied(text)) => Event::Notice(text),
            // Не `Failed`: система ответила успехом, ничего не сломалось и
            // повторять нечего — просто её ответ значит не то, чего ждали.
            Ok(Outcome::Ignored(text)) => Event::Warning(text),
            Err(message) => Event::Failed {
                id: NO_DOWNLOAD,
                message,
            },
        });
        let _ = tx.send(Event::Power(read()));
        notify();
    });
}

/// Почему на этой системе переключать нечего.
#[cfg(not(windows))]
const FOREIGN_SYSTEM: &str = "Питанием Savio управляет только в Windows: схемы \
     электропитания и «Режим питания» — её понятия. У Linux и macOS \
     соответствия им нет: cpufreq, TLP и pmset устроены иначе и меняют \
     другое, так что показать здесь то же самое не выйдет.";

#[cfg(not(windows))]
pub fn read() -> crate::model::PowerState {
    crate::model::PowerState {
        trouble: Some(FOREIGN_SYSTEM.to_owned()),
        ..Default::default()
    }
}

#[cfg(not(windows))]
pub fn apply(_change: Change) -> Result<Outcome, String> {
    Err(FOREIGN_SYSTEM.to_owned())
}

#[cfg(windows)]
pub use windows::{apply, read};

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;

    use super::{Change, Outcome};
    use crate::model::{PlanId, PowerMode, PowerModes, PowerPlan, PowerState};

    /// GUID в том виде, в каком его ждут функции Windows.
    ///
    /// Полями, а не шестнадцатью байтами подряд: буфер, в который система
    /// пишет `GUID`, обязан быть выровнен как `u32`, а у `[u8; 16]`
    /// выравнивание единица.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        d1: u32,
        d2: u16,
        d3: u16,
        d4: [u8; 8],
    }

    impl Guid {
        const ZERO: Self = Self {
            d1: 0,
            d2: 0,
            d3: 0,
            d4: [0; 8],
        };

        fn new(id: PlanId) -> Self {
            let b = id.bytes();
            Self {
                d1: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                d2: u16::from_le_bytes([b[4], b[5]]),
                d3: u16::from_le_bytes([b[6], b[7]]),
                d4: [b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
            }
        }

        fn id(self) -> PlanId {
            let (a, b, c) = (
                self.d1.to_le_bytes(),
                self.d2.to_le_bytes(),
                self.d3.to_le_bytes(),
            );
            let t = self.d4;
            PlanId::from_parts(
                u32::from_le_bytes(a),
                u16::from_le_bytes(b),
                u16::from_le_bytes(c),
                t,
            )
        }
    }

    // Коды возврата Windows. Свои имена, потому что тянуть ради трёх чисел
    // крейт `windows` незачем: фича с ними лежит в другом модуле, а сами
    // числа не менялись со времён NT.
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_MORE_DATA: u32 = 234;

    /// Что перечисляем в `PowerEnumerate` — схемы, а не их настройки.
    const ACCESS_SCHEME: u32 = 16;

    /// Сколько схем готовы прочитать.
    ///
    /// Потолок обязателен по Правилу 1, как `LOG_LIMIT` и `USB_LIMIT`: цикл
    /// крутится по ответам чужой библиотеки, и его конец задаём не только мы.
    /// Тридцать две — заведомо больше, чем бывает на машине с вендорскими
    /// схемами, и при этом конечное число.
    const PLAN_LIMIT: u32 = 32;

    /// Сколько байт готовы отдать под название схемы.
    ///
    /// Имя приходит в UTF-16, то есть это 512 знаков. Размер спрашиваем
    /// у самой системы, и ограничение здесь — только защита от ответа,
    /// который выделит гигабайт.
    const NAME_LIMIT: u32 = 1024;

    type EnumerateFn = unsafe extern "system" fn(
        *mut c_void,
        *const Guid,
        *const Guid,
        u32,
        u32,
        *mut u8,
        *mut u32,
    ) -> u32;
    type FriendlyNameFn = unsafe extern "system" fn(
        *mut c_void,
        *const Guid,
        *const Guid,
        *const Guid,
        *mut u8,
        *mut u32,
    ) -> u32;
    type GetActiveFn = unsafe extern "system" fn(*mut c_void, *mut *mut Guid) -> u32;
    type SetActiveFn = unsafe extern "system" fn(*mut c_void, *const Guid) -> u32;
    type GetOverlayFn = unsafe extern "system" fn(*mut Guid) -> u32;
    type SetOverlayFn = unsafe extern "system" fn(Guid) -> u32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(name: *const u16, reserved: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *const c_void;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    /// Функции `powrprof.dll`, которые нашлись в этой системе.
    struct Api {
        enumerate: EnumerateFn,
        friendly_name: FriendlyNameFn,
        get_active: GetActiveFn,
        set_active: SetActiveFn,
        /// Режимы питания. `None` — Windows старее 10 версии 1803.
        overlay: Option<Overlay>,
    }

    /// Три функции режима питания. Вместе, а не порознь: без любой из них
    /// показывать переключатель нельзя — он либо соврёт о состоянии, либо
    /// молча ничего не переключит.
    struct Overlay {
        effective: GetOverlayFn,
        set: SetOverlayFn,
        /// Запомненный режим. Единственная необязательная: без неё Savio не
        /// сможет сказать «система помнит другой», но всё остальное работает.
        actual: Option<GetOverlayFn>,
    }

    /// Загружает библиотеку один раз на весь запуск.
    ///
    /// `OnceLock`, а не загрузка на каждое чтение: `LoadLibraryEx` при
    /// повторном вызове только увеличивает счётчик ссылок, но `GetProcAddress`
    /// по семи именам на каждое открытие вкладки — работа впустую.
    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(load).as_ref()
    }

    fn load() -> Option<Api> {
        // `LOAD_LIBRARY_SEARCH_SYSTEM32`: искать только в системном каталоге.
        // Без него порядок поиска начинается с каталога самой программы, то
        // есть чужой `powrprof.dll`, положенный рядом с портативной поставкой
        // Savio, был бы загружен вместо системного.
        const SEARCH_SYSTEM32: u32 = 0x0000_0800;

        let name: Vec<u16> = "powrprof.dll"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), SEARCH_SYSTEM32) };
        if module.is_null() {
            return None;
        }

        // Каждый `transmute` здесь превращает адрес в указатель на функцию,
        // и `Option` в типе — не украшение: у указателя на функцию нулевое
        // значение запрещено, поэтому нулём система и сообщает «такой
        // функции у меня нет».
        let enumerate = unsafe {
            std::mem::transmute::<*const c_void, Option<EnumerateFn>>(symbol(
                module,
                b"PowerEnumerate\0",
            ))
        };
        let friendly_name = unsafe {
            std::mem::transmute::<*const c_void, Option<FriendlyNameFn>>(symbol(
                module,
                b"PowerReadFriendlyName\0",
            ))
        };
        let get_active = unsafe {
            std::mem::transmute::<*const c_void, Option<GetActiveFn>>(symbol(
                module,
                b"PowerGetActiveScheme\0",
            ))
        };
        let set_active = unsafe {
            std::mem::transmute::<*const c_void, Option<SetActiveFn>>(symbol(
                module,
                b"PowerSetActiveScheme\0",
            ))
        };
        let effective = unsafe {
            std::mem::transmute::<*const c_void, Option<GetOverlayFn>>(symbol(
                module,
                b"PowerGetEffectiveOverlayScheme\0",
            ))
        };
        let actual = unsafe {
            std::mem::transmute::<*const c_void, Option<GetOverlayFn>>(symbol(
                module,
                b"PowerGetActualOverlayScheme\0",
            ))
        };
        let set_overlay = unsafe {
            std::mem::transmute::<*const c_void, Option<SetOverlayFn>>(symbol(
                module,
                b"PowerSetActiveOverlayScheme\0",
            ))
        };

        Some(Api {
            enumerate: enumerate?,
            friendly_name: friendly_name?,
            get_active: get_active?,
            set_active: set_active?,
            overlay: effective.zip(set_overlay).map(|(effective, set)| Overlay {
                effective,
                set,
                actual,
            }),
        })
    }

    /// Адрес функции в загруженном модуле. Нулевой — такой функции нет.
    ///
    /// # Safety
    ///
    /// `module` — живой результат `LoadLibraryEx`, `name` кончается нулём.
    unsafe fn symbol(module: *mut c_void, name: &[u8]) -> *const c_void {
        debug_assert_eq!(name.last(), Some(&0), "имя функции без нуля на конце");
        unsafe { GetProcAddress(module, name.as_ptr()) }
    }

    pub fn read() -> PowerState {
        let Some(api) = api() else {
            return PowerState {
                trouble: Some(
                    "Windows не отдала powrprof.dll — библиотеку, которая \
                     заведует питанием. Переключать отсюда нечего."
                        .to_owned(),
                ),
                ..PowerState::default()
            };
        };

        let plans = plans(api);
        let active = active_plan(api);
        let modes = modes(api);

        // Оговорки собираем здесь, а не в кадре отрисовки: строка неизменна,
        // пока не перечитали состояние (Правило 1).
        let mut trouble = String::new();
        if plans.is_empty() {
            trouble.push_str(
                "Список схем электропитания система не отдала — переключать нечего. ",
            );
        }
        if modes == PowerModes::Unsupported {
            trouble.push_str(
                "Режим питания эта Windows не поддерживает: он появился \
                 в Windows 10 версии 1803.",
            );
        }

        PowerState {
            plans,
            active,
            modes,
            trouble: (!trouble.is_empty()).then(|| trouble.trim_end().to_owned()),
        }
    }

    /// Все схемы электропитания с их названиями.
    fn plans(api: &Api) -> Vec<PowerPlan> {
        let mut plans = Vec::new();

        for index in 0..PLAN_LIMIT {
            let mut guid = Guid::ZERO;
            let mut size = size_of::<Guid>() as u32;
            let rc = unsafe {
                (api.enumerate)(
                    null_mut(),
                    null(),
                    null(),
                    ACCESS_SCHEME,
                    index,
                    (&raw mut guid).cast::<u8>(),
                    &raw mut size,
                )
            };
            // Конец списка — это `ERROR_NO_MORE_ITEMS`, но разбирать код
            // незачем: любой не-успех значит, что дальше читать нечего.
            if rc != ERROR_SUCCESS {
                break;
            }

            let id = guid.id();
            plans.push(PowerPlan {
                // Имени может не быть — схему могли удалить между двумя
                // вызовами. Показать её без названия честнее, чем потерять:
                // именно она может оказаться активной.
                name: friendly_name(api, &guid).unwrap_or_else(|| id.to_string()),
                id,
            });
        }

        plans
    }

    /// Название схемы, как его хранит система, — сразу в UTF-16.
    fn friendly_name(api: &Api, guid: &Guid) -> Option<String> {
        let mut size: u32 = 0;
        let rc = unsafe {
            (api.friendly_name)(null_mut(), guid, null(), null(), null_mut(), &raw mut size)
        };
        // Запрос размера отвечает по-разному в разных сборках Windows:
        // где-то успехом, где-то `ERROR_MORE_DATA`. Важен здесь размер.
        if (rc != ERROR_SUCCESS && rc != ERROR_MORE_DATA) || size == 0 || size > NAME_LIMIT {
            return None;
        }

        // Буфер из `u16`, а не из байт: система пишет туда UTF-16, и
        // выравнивание должно быть под него. Запас в одно слово — на случай
        // нечётного размера в ответе.
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        let rc = unsafe {
            (api.friendly_name)(
                null_mut(),
                guid,
                null(),
                null(),
                buf.as_mut_ptr().cast::<u8>(),
                &raw mut size,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }

        let end = buf.iter().position(|&unit| unit == 0).unwrap_or(buf.len());
        let name = String::from_utf16_lossy(&buf[..end]);
        let name = name.trim();
        // Пустое имя — это «нет данных», а не название. Пустая подпись на
        // кнопке выглядела бы недорисованной кнопкой.
        (!name.is_empty()).then(|| name.to_owned())
    }

    /// Какая схема активна.
    fn active_plan(api: &Api) -> Option<PlanId> {
        let mut ptr: *mut Guid = null_mut();
        let rc = unsafe { (api.get_active)(null_mut(), &raw mut ptr) };
        if rc != ERROR_SUCCESS || ptr.is_null() {
            return None;
        }

        let id = unsafe { *ptr }.id();
        // Память под ответ выделила система, освобождать её нам — иначе
        // каждое открытие вкладки оставляло бы по шестнадцать байт.
        unsafe { LocalFree(ptr.cast::<c_void>()) };
        Some(id)
    }

    /// Действующий и запомненный режимы питания.
    fn modes(api: &Api) -> PowerModes {
        let Some(overlay) = &api.overlay else {
            return PowerModes::Unsupported;
        };

        let Some(effective) = current_overlay(overlay.effective) else {
            // Функция есть, а ответить не смогла: показывать переключатель
            // нельзя — его положение было бы выдумкой.
            return PowerModes::Unsupported;
        };

        let effective_mode = PowerMode::from_id(effective);
        let ignored = overlay
            .actual
            .and_then(current_overlay)
            .filter(|actual| *actual != effective)
            .and_then(PowerMode::from_id);

        PowerModes::Known {
            effective: effective_mode,
            ignored,
        }
    }

    /// Один вызов «какой сейчас режим».
    fn current_overlay(read: GetOverlayFn) -> Option<PlanId> {
        let mut guid = Guid::ZERO;
        let rc = unsafe { read(&raw mut guid) };
        (rc == ERROR_SUCCESS).then(|| guid.id())
    }

    pub fn apply(change: Change) -> Result<Outcome, String> {
        let Some(api) = api() else {
            return Err(
                "Windows не отдала powrprof.dll — переключать питание нечем.".to_owned(),
            );
        };

        match change {
            Change::Plan(id) => apply_plan(api, id),
            Change::Mode(mode) => apply_mode(api, mode),
        }
    }

    fn apply_plan(api: &Api, id: PlanId) -> Result<Outcome, String> {
        let guid = Guid::new(id);
        let rc = unsafe { (api.set_active)(null_mut(), &guid) };
        if rc != ERROR_SUCCESS {
            return Err(format!(
                "Windows не переключила схему электропитания (код {rc}). \
                 Обычно так отвечают на схему, которой больше нет: \
                 нажмите «Обновить»."
            ));
        }

        // Правило 6: код успеха — не доказательство. Спрашиваем систему
        // заново и верим только её ответу.
        let name = friendly_name(api, &guid).unwrap_or_else(|| id.to_string());
        match active_plan(api) {
            Some(now) if now == id => Ok(Outcome::Applied(format!(
                "Схема электропитания переключена: «{name}»."
            ))),
            _ => Err(format!(
                "Windows ответила успехом, но активной осталась не «{name}». \
                 Схему могла вернуть назад политика организации."
            )),
        }
    }

    fn apply_mode(api: &Api, mode: PowerMode) -> Result<Outcome, String> {
        let Some(overlay) = &api.overlay else {
            return Err(
                "Режим питания эта Windows не поддерживает: он появился \
                 в Windows 10 версии 1803."
                    .to_owned(),
            );
        };

        let rc = unsafe { (overlay.set)(Guid::new(mode.id())) };
        if rc != ERROR_SUCCESS {
            return Err(format!(
                "Windows не переключила режим питания (код {rc})."
            ));
        }

        let Some(now) = current_overlay(overlay.effective) else {
            return Err(
                "Windows приняла режим питания, но назвать действующий \
                 отказалась — что стало с машиной, Savio не знает."
                    .to_owned(),
            );
        };

        if now == mode.id() {
            return Ok(Outcome::Applied(format!(
                "Режим питания переключён: {}.",
                mode.label()
            )));
        }

        // Тот самый молчаливый отказ из заголовка модуля. Ошибкой это
        // называть нельзя (система сделала ровно то, о чём её просили,
        // и запомнила выбор), но и успехом тоже: машина работает иначе.
        //
        // Сказано коротко и только про само нажатие. Почему не применилось и
        // что теперь делать, видно из состояния, которое уедет следующим
        // событием (`PowerModes::ignored`), — и пересказывать это здесь
        // значило бы поставить в окно два жёлтых абзаца об одном и том же.
        Ok(Outcome::Ignored(format!(
            "Режим «{}» Windows запомнила, но не применила.",
            mode.label()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Чтение состояния не должно ни падать, ни выдумывать.
    ///
    /// Живой тест: он спрашивает настоящую систему и потому проверяет не
    /// числа, а согласованность ответа — на машине без Windows, на Windows
    /// без режимов питания и на обычной Windows 11 он одинаково осмыслен.
    /// Ничего не переключает: у переключения нет исхода, который можно
    /// проверить, не изменив состояние машины того, кто гоняет тесты.
    #[test]
    fn reading_power_state_is_self_consistent() {
        let state = read();

        // Активная схема обязана быть в списке: показать в окне выбранным
        // то, чего в списке нет, нельзя — переключатель окажется пустым.
        if let Some(active) = state.active {
            assert!(
                state.plans.iter().any(|plan| plan.id == active),
                "активная схема {active} не попала в список из {} схем",
                state.plans.len()
            );
        }

        // Пустое название — это пустая кнопка в окне.
        for plan in &state.plans {
            assert!(!plan.name.trim().is_empty(), "схема {} без названия", plan.id);
        }

        // Молчание — не успех: если показывать нечего, Savio обязан
        // объяснить почему.
        assert!(
            !state.is_blank() || state.trouble.is_some(),
            "состояние пустое и без объяснения"
        );

        // Запомненный режим отдельно от действующего называется только
        // тогда, когда они разошлись.
        if let crate::model::PowerModes::Known { effective, ignored } = state.modes {
            assert!(
                ignored.is_none() || ignored != effective,
                "«запомнен, но не применён» назван тем же режимом, что и действующий"
            );
        }
    }
}
