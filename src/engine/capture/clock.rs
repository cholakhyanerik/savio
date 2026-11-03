//! Местное время — для имени файла записи.
//!
//! Стандартная библиотека знает только время от эпохи, без часового пояса,
//! а человек ищет запись по тому времени, которое видел на часах. Поэтому
//! две ветки: `GetLocalTime` на Windows и `localtime_r` из libc на Unix —
//! libc и так слинкована в любой Rust-программе под Unix.

/// Дата и время по часам машины.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Который час. `None` — система не ответила; имя тогда без даты.
#[cfg(windows)]
pub(super) fn now() -> Option<Stamp> {
    /// `SYSTEMTIME`: восемь `WORD` подряд.
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(time: *mut SystemTime);
    }

    let mut time = SystemTime::default();
    // Отказать функция не умеет: у неё нет кода возврата.
    unsafe { GetLocalTime(&mut time) };
    Some(Stamp {
        year: i32::from(time.year),
        month: u32::from(time.month),
        day: u32::from(time.day),
        hour: u32::from(time.hour),
        minute: u32::from(time.minute),
        second: u32::from(time.second),
    })
}

/// Который час. `None` — система не ответила; имя тогда без даты.
#[cfg(unix)]
pub(super) fn now() -> Option<Stamp> {
    use std::ffi::{c_char, c_int, c_long};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `struct tm` у glibc, musl и macOS: девять `int`, затем смещение пояса
    /// и его имя. Хвост объявлен целиком, хотя не читается: `localtime_r`
    /// пишет и его, и укороченная структура дала бы запись мимо памяти.
    #[repr(C)]
    struct Tm {
        sec: c_int,
        min: c_int,
        hour: c_int,
        mday: c_int,
        mon: c_int,
        year: c_int,
        wday: c_int,
        yday: c_int,
        isdst: c_int,
        gmtoff: c_long,
        zone: *const c_char,
    }

    unsafe extern "C" {
        // `time_t` — это `long` на всех трёх системах сборки (Linux x86_64,
        // macOS arm64 и x86_64).
        fn localtime_r(time: *const c_long, out: *mut Tm) -> *mut Tm;
    }

    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let secs = c_long::try_from(secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    if unsafe { localtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    // Нули в структуре допустимы (указатель пояса — пустой), а функция,
    // ответившая не нулём, заполнила её целиком.
    let tm = unsafe { tm.assume_init() };
    Some(Stamp {
        year: tm.year + 1900,
        month: u32::try_from(tm.mon + 1).ok()?,
        day: u32::try_from(tm.mday).ok()?,
        hour: u32::try_from(tm.hour).ok()?,
        minute: u32::try_from(tm.min).ok()?,
        second: u32::try_from(tm.sec).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Часы отвечают правдоподобно: год не из прошлого века, месяц и день
    /// в своих пределах. Сверять с чем-то точнее нечем — стандартная
    /// библиотека местного времени не знает, ради этого модуль и заведён.
    #[test]
    fn the_local_clock_answers_with_a_plausible_date() {
        let now = now().expect("часы не ответили");
        assert!(now.year >= 2026, "{now:?}");
        assert!((1..=12).contains(&now.month), "{now:?}");
        assert!((1..=31).contains(&now.day), "{now:?}");
        assert!(now.hour < 24 && now.minute < 60 && now.second < 61, "{now:?}");
    }
}
