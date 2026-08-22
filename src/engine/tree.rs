//! Убийство дерева процессов, а не одного процесса.
//!
//! `Child::kill()` бьёт ровно по тому процессу, который запустил сам Savio,
//! а дерево у загрузки глубже:
//!
//! ```text
//! savio → yt-dlp (обёртка PyInstaller) → yt-dlp (рабочий) → ffmpeg
//! ```
//!
//! При обрезке ключом `--download-sections` качает именно ffmpeg, и после
//! «Отмены» он вместе с рабочим yt-dlp оставался жив: проверено вживую
//! (2026-07-27) — через тридцать секунд оба процесса на месте и работают.
//! У загрузки без обрезки та же беда, только тише: события `Progress` после
//! «Отмены» шли ещё восемнадцать секунд и дошли до 126 МБ (2026-07-31).
//! Рабочий yt-dlp там не замечает закрытого канала и заметить не может —
//! читающий конец трубы держит открытым сам движок, поток которого до конца
//! сидит в `BufReader::lines()`.
//!
//! Отсюда же способ проверки: **признак живой загрузки — события `Progress`,
//! а не размер файла**. Пока `.part` открыт, каталожная запись Windows
//! показывает 0 байт, и настоящий размер появляется только при закрытии.
//!
//! Единого способа убить поддерево у трёх систем нет, поэтому внутри две
//! ветки `#[cfg]`. Наружу они торчат одним типом [`Group`], и звать его надо
//! в трёх точках: [`Group::arm`] — до запуска, [`Group::adopt`] — сразу после,
//! [`Group::kill`] — при отмене. Пропуск любой из трёх ничего не ломает
//! в сборке, в `clippy` и в тестах: просто вернётся переживший «Отмену»
//! процесс, которого в окне не видно.

use std::process::{Child, Command};

/// Группа, в которую попадает запущенный процесс и всё, что он запустит сам.
///
/// Живёт в `Control` рядом со слотом процесса и флагом отмены: убийство дерева
/// нужно ровно там же, где убийство одного процесса, и второй общей структуры
/// ради него заводить незачем.
#[derive(Default)]
pub(super) struct Group(imp::Group);

impl Group {
    /// Готовит команду к запуску в этой группе. Звать **до** `spawn`.
    ///
    /// На Unix иначе нельзя: своя группа процессов заказывается при запуске,
    /// снаружи её потом не назначить без гонки с `exec`.
    pub(super) fn arm(&self, cmd: &mut Command) {
        self.0.arm(cmd);
    }

    /// Принимает запущенный процесс в группу. Звать сразу **после** `spawn`.
    ///
    /// Между `spawn` и этой строкой лежит окно, в которое запущенный процесс
    /// теоретически успел бы наплодить потомков мимо группы. Закрыть его
    /// нечем: запуск приостановленным (`CREATE_SUSPENDED`) требует ручки
    /// главного потока, а стандартная библиотека её не отдаёт. На практике
    /// окно безопасно — обёртка PyInstaller распаковывает себя доли секунды,
    /// прежде чем запустить хоть что-нибудь.
    pub(super) fn adopt(&self, child: &Child) {
        self.0.adopt(child);
    }

    /// Убивает всё дерево разом.
    pub(super) fn kill(&self) {
        self.0.kill();
    }

    /// Забывает дерево после того, как процесс дождались.
    ///
    /// На Unix это обязательно, и не ради порядка: группа зовётся по номеру
    /// процесса-родоначальника, а номер после `wait` система вправе выдать
    /// кому угодно. Забытый номер превратил бы «Отмену» в выстрел по чужой
    /// группе процессов — по той самой причине, по которой `wait_and_release`
    /// освобождает и слот.
    pub(super) fn release(&self) {
        self.0.release();
    }
}

/// Windows: Job Object.
///
/// Ручка Job Object — это `OwnedHandle`, а не сырой указатель, и взят он не
/// для красоты: `Control` уезжает в поток движка, а `OwnedHandle` — `Send`
/// и `Sync` и закрывает себя сам. Ради сырого указателя пришлось бы писать
/// и `unsafe impl Send`, и свой `Drop`.
#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::{Child, Command};
    use std::ptr::{null, null_mut};

    // Числа Windows своими именами: тянуть крейт `windows` ради четырёх
    // функций и двух констант незачем, и в `power.rs` сделано так же.

    /// `JobObjectExtendedLimitInformation`.
    const EXTENDED_LIMIT: u32 = 9;

    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: закрытие последней ручки убивает
    /// всё, что в группе осталось.
    ///
    /// Ради этого флага группа и заводится ручкой, а не списком номеров:
    /// ручки закрывает система при смерти процесса, то есть поддерево не
    /// переживёт ни выхода Savio, ни его падения. Без флага пережившие
    /// «Отмену» ffmpeg и yt-dlp продолжали качать и после закрытия окна —
    /// ровно то, с чего дефект начинался.
    const KILL_ON_CLOSE: u32 = 0x0000_2000;

    /// `JOBOBJECT_BASIC_LIMIT_INFORMATION`.
    ///
    /// Поля нужны все до единого, хотя заполняем мы одно: структура уходит
    /// в систему целиком, и порядок с размерами в ней — часть договора.
    /// Ошибка в раскладке не даст ни ошибки сборки, ни ругани `clippy` —
    /// `SetInformationJobObject` просто вернёт ноль либо, что хуже, поймёт
    /// не то поле.
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimit {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    /// `IO_COUNTERS`.
    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operations: u64,
        write_operations: u64,
        other_operations: u64,
        read_transfer: u64,
        write_transfer: u64,
        other_transfer: u64,
    }

    /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetInformationJobObject(
            job: *mut c_void,
            class: u32,
            info: *mut c_void,
            len: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
    }

    pub(super) struct Group {
        /// `None` — система группы не дала. Тогда работаем по-старому: убитым
        /// окажется один процесс, но загрузка всё равно остановится, а падать
        /// из-за этого — хуже, чем недоубить.
        job: Option<OwnedHandle>,
    }

    /// Группа заводится сразу, а не при первом запуске процесса: она уезжает
    /// в поток движка внутри `Control`, то есть по общей ссылке, а из-под неё
    /// поле уже не заполнить.
    impl Default for Group {
        fn default() -> Self {
            Self { job: create() }
        }
    }

    impl Group {
        pub(super) fn arm(&self, _cmd: &mut Command) {
            // Windows зачисляет в группу уже запущенный процесс — готовить
            // к этому команду не нужно.
        }

        pub(super) fn adopt(&self, child: &Child) {
            let Some(job) = &self.job else {
                return;
            };
            // Ручка процесса у `Child` уже с правами `PROCESS_ALL_ACCESS`:
            // её выдал `CreateProcess`, а не `OpenProcess`.
            let _ = unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) };
        }

        pub(super) fn kill(&self) {
            if let Some(job) = &self.job {
                let _ = unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
            }
        }

        pub(super) fn release(&self) {
            // Группа переживает свой процесс намеренно: после yt-dlp в неё
            // ложится наш ffmpeg (`engine::cut`), и заводить под него вторую
            // было бы работой впустую. Номерами Windows группу не зовёт,
            // так что старая ручка ни по кому чужому не выстрелит.
        }
    }

    /// Создаёт группу. `None` — система отказала.
    fn create() -> Option<OwnedHandle> {
        // Безымянная и ненаследуемая: имя сделало бы группу общей для двух
        // запущенных разом Savio, а наследуемая ручка уехала бы в сам yt-dlp
        // и не дала бы флагу `KILL_ON_CLOSE` сработать при выходе.
        let job = unsafe { CreateJobObjectW(null_mut(), null()) };
        if job.is_null() {
            return None;
        }
        let job = unsafe { OwnedHandle::from_raw_handle(job) };

        let mut limits = ExtendedLimit {
            basic: BasicLimit {
                limit_flags: KILL_ON_CLOSE,
                ..BasicLimit::default()
            },
            ..ExtendedLimit::default()
        };
        // Отказ не повод бросать группу: без флага не будет только уборки при
        // выходе, а «Отмена» убьёт дерево и так — она зовёт `TerminateJobObject`
        // сама, не полагаясь на закрытие ручки.
        let _ = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                EXTENDED_LIMIT,
                (&raw mut limits).cast(),
                size_of::<ExtendedLimit>() as u32,
            )
        };

        Some(job)
    }
}

/// Unix: своя группа процессов.
///
/// `kill(-pgid)` бьёт по всей группе разом — это и есть здешний Job Object.
/// Саму функцию объявляем сами: `libc` стандартная библиотека и так линкует,
/// а заводить ради одного вызова зависимость нельзя (см. «Общее» в CLAUDE.md).
#[cfg(unix)]
mod imp {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicI32, Ordering};

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    /// `SIGKILL` — девятка и на Linux, и на macOS.
    ///
    /// Не `SIGTERM`: `Child::kill()` шлёт именно `SIGKILL`, и «Отмена» после
    /// этой правки обязана остаться такой же немедленной, какой была.
    const SIGKILL: i32 = 9;

    #[derive(Default)]
    pub(super) struct Group {
        /// Номер группы (он же номер процесса-родоначальника). Ноль — процесса
        /// ещё нет либо его уже дождались.
        pgid: AtomicI32,
    }

    impl Group {
        pub(super) fn arm(&self, cmd: &mut Command) {
            // Нуль значит «сделать процесс родоначальником своей группы»:
            // номером она получит его собственный. Стабильно с 1.64, свой
            // `setsid` через FFI для этого не нужен.
            cmd.process_group(0);
        }

        pub(super) fn adopt(&self, child: &Child) {
            self.pgid.store(child.id() as i32, Ordering::Relaxed);
        }

        pub(super) fn kill(&self) {
            // Забываем номер сразу: убивать по нему второй раз нечего, а вот
            // выстрелить в чужую группу после того, как система выдаст номер
            // заново, — вполне.
            let pgid = self.pgid.swap(0, Ordering::Relaxed);
            if pgid > 0 {
                // Минус перед номером и превращает выстрел в залп по группе.
                let _ = unsafe { kill(-pgid, SIGKILL) };
            }
        }

        pub(super) fn release(&self) {
            self.pgid.store(0, Ordering::Relaxed);
        }
    }

    /// Долг перед Windows, где то же самое делает `KILL_ON_JOB_CLOSE`.
    ///
    /// Выхода из Savio эта половина не покрывает: eframe после `App::on_exit`
    /// зовёт `std::process::exit(0)`, и деструкторы не выполняются ни у кого
    /// (Правило 3). Поэтому идущую загрузку останавливает `on_exit`, а `Drop`
    /// остаётся сторожем для всех остальных случаев.
    impl Drop for Group {
        fn drop(&mut self) {
            self.kill();
        }
    }
}
