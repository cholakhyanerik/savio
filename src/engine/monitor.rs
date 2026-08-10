//! Монитор производительности: что происходит с машиной прямо сейчас.
//!
//! Слой «Движок»: здесь спрашивают систему и считают скорости. Про `egui`
//! и виджеты модуль не знает ничего — наружу уезжает готовый `PerfSample`,
//! а рисует его `app.rs`.
//!
//! # Чем это отличается от `hardware`
//!
//! Соседний модуль снимает **снимок**: один раз, по кнопке, и всё, что он
//! показывает, от времени не зависит. Здесь наоборот: каждое число — разница
//! между двумя замерами, и потому опрос обязан жить в одном потоке от начала
//! до конца. `System`, созданный заново каждую секунду, показывал бы либо
//! ноль, либо среднее с момента загрузки машины — и то и другое выглядит
//! правдоподобно, что делает ошибку тихой.
//!
//! # Почему опрос умеет останавливаться
//!
//! Savio в покое не тратит ни кадра: egui рисует по вводу и по просьбе.
//! Секундный опрос эту тишину отменяет — каждый замер зовёт `notify`, то есть
//! просит кадр. Пока монитор смотрят, это ровно то, что нужно; как только
//! вкладку закрыли, а оверлей выключен, загрузчик начал бы будить видеокарту
//! раз в секунду до конца дня. Отсюда `Handle::stop`, и отсюда же `Condvar`
//! вместо `sleep`: спящий поток обязан просыпаться на остановку сразу, иначе
//! быстрое переключение вкладок оставляет за собой очередь досыпающих
//! потоков, каждый со своим `sysinfo`.
//!
//! # Правило 6 в этом модуле
//!
//! Счётчики ввода-вывода есть не везде. `IOCTL_DISK_PERFORMANCE` на Windows
//! может ответить отказом, и `sysinfo` оставит в поле ноль — от простаивающего
//! диска такой ноль по одному показанию не отличить. Поэтому смотрим на
//! **накопительный** счётчик: суммарные с загрузки байты у живой машины нулём
//! не бывают, и ноль в них означает, что счётчиков нет вовсе. Проверяем это
//! на каждом замере, а не однажды при запуске: том могли подключить на ходу.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::model::{
    Event, Metric, PROC_LIMIT, PerfSample, ProcRow, human_bytes, human_mhz, human_percent,
    human_speed, plural_ru,
};

/// Как часто снимаем показания.
///
/// Секунда — не круглое число ради красоты, а нижняя граница осмысленности:
/// загрузка процессора считается как разница двух замеров, и `sysinfo` прямо
/// требует выдержать между ними `MINIMUM_CPU_UPDATE_INTERVAL` (200 мс).
/// Чаще секунды показания начинают дёргаться, реже — монитор перестаёт
/// успевать за тем, ради чего его открыли.
pub const INTERVAL: Duration = Duration::from_secs(1);

/// Ручка опроса: пока её не попросили, поток снимает показания.
pub struct Handle {
    stop: Arc<Stop>,
}

impl Handle {
    /// Останавливает опрос. Поток просыпается и выходит сразу, не досыпая
    /// свой интервал.
    ///
    /// Звать можно сколько угодно раз: повторная остановка ничего не делает.
    pub fn stop(&self) {
        self.stop.stop();
    }
}

/// Общий с потоком выключатель.
///
/// `Condvar`, а не `AtomicBool` со `sleep`, по причине из заголовка модуля:
/// спящий поток должен уходить по первой просьбе. С `sleep` остановка стоила
/// бы целого интервала, и десяток переключений вкладки за секунду оставил бы
/// десяток живых опросчиков — каждый со своим `System`, каждый готовый
/// разбудить окно.
#[derive(Default)]
struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    fn stop(&self) {
        let mut guard = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        *guard = true;
        // Будим, не отпуская замка: `Condvar` этого не запрещает, а порядок
        // «пометить, потом разбудить» гарантирует, что проснувшийся увидит
        // уже поднятый флаг.
        self.wake.notify_all();
    }

    /// Ждёт до срока. `true` — пора остановиться.
    fn wait(&self, limit: Duration) -> bool {
        let guard = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        // `wait_timeout_while` сам переживает ложные пробуждения: без него
        // цикл считал бы каждое из них истёкшим интервалом и снимал бы
        // показания чаще, чем просили.
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, limit, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

/// Запускает опрос в отдельном потоке.
///
/// `notify` вызывается после каждого замера — UI на нём делает repaint.
/// Первый замер приезжает через `INTERVAL`, а не сразу, и это не задержка,
/// а необходимость: загрузка процессора — разница между двумя точками, и
/// показать её раньше, чем набралась вторая, нельзя ничем, кроме нуля.
pub fn start(tx: Sender<Event>, notify: impl Fn() + Send + 'static) -> Handle {
    let stop = Arc::new(Stop::default());
    let mine = Arc::clone(&stop);

    std::thread::spawn(move || {
        let mut sampler = Sampler::new();

        loop {
            if mine.wait(INTERVAL) {
                return;
            }

            let sample = sampler.take();

            // Приёмник мог умереть, пока мы снимали показания: вкладку
            // закрывают и мышью, и `Handle::stop`. Это не ошибка — просто
            // работать больше не на кого.
            if tx.send(Event::Perf(sample)).is_err() {
                return;
            }
            notify();
        }
    });

    Handle { stop }
}

/// Состояние опроса, живущее между замерами.
struct Sampler {
    sys: sysinfo::System,
    nets: sysinfo::Networks,
    disks: sysinfo::Disks,
    /// Когда сняли прошлые показания.
    ///
    /// Скорости делим на настоящий промежуток, а не на `INTERVAL`: поток спит
    /// «не меньше» заказанного, а под нагрузкой — заметно дольше. С делением
    /// на константу скорость на занятой машине оказывалась бы завышенной
    /// ровно там, где на неё и смотрят.
    last: Instant,
}

impl Sampler {
    /// Заводит опрос и снимает нулевую точку.
    ///
    /// Первый замер каждого счётчика ничего не значит: у процессора он
    /// сравнивать не с чем, у сети и дисков `sysinfo` отдаёт всё накопленное
    /// с загрузки машины. Поэтому его снимаем здесь и выбрасываем.
    fn new() -> Self {
        let mut sys = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing()
                .with_cpu(cpu_kind())
                .with_memory(sysinfo::MemoryRefreshKind::everything()),
        );
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            process_kind(),
        );

        Self {
            sys,
            nets: sysinfo::Networks::new_with_refreshed_list(),
            disks: sysinfo::Disks::new_with_refreshed_list_specifics(disk_kind()),
            last: Instant::now(),
        }
    }

    /// Снимает свежие показания.
    fn take(&mut self) -> PerfSample {
        let elapsed = self.last.elapsed();
        self.last = Instant::now();

        self.sys.refresh_cpu_specifics(cpu_kind());
        self.sys.refresh_memory();
        // `remove_dead_processes` обязателен: без него закрытые процессы
        // остаются в списке навсегда со своей последней загрузкой, и
        // верхушку рано или поздно занимают покойники.
        self.sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            process_kind(),
        );
        self.nets.refresh(false);
        self.disks.refresh_specifics(false, disk_kind());

        PerfSample {
            cpu: self.cpu(),
            mem: self.memory(),
            swap: self.swap(),
            net: self.network(elapsed),
            disk: self.disk(elapsed),
            procs: self.processes(),
        }
    }

    fn cpu(&self) -> Metric {
        let cpus = self.sys.cpus();
        let cores = cpus.len();
        // Частота приезжает нулём и когда её не спросили, и когда система не
        // ответила (`CallNtPowerInformation` на Windows заполняет вектор
        // нулями), — `human_mhz` превращает такой ноль в отсутствие значения.
        let freq = cpus.first().map(sysinfo::Cpu::frequency).and_then(human_mhz);

        let detail = match (cores, freq) {
            (0, _) => None,
            (n, Some(freq)) => Some(format!(
                "{n} {} · {freq}",
                plural_ru(n as u64, "ядро", "ядра", "ядер")
            )),
            (n, None) => Some(format!(
                "{n} {}",
                plural_ru(n as u64, "ядро", "ядра", "ядер")
            )),
        };

        Metric::new(self.sys.global_cpu_usage(), detail)
    }

    fn memory(&self) -> Metric {
        let total = self.sys.total_memory();
        let used = self.sys.used_memory();

        // Ноль здесь означал бы, что обновления памяти не было, — но оно
        // было. Значит, система действительно промолчала, и доли у нас нет:
        // делить на ноль ради `NaN` в графике незачем.
        if total == 0 {
            return Metric::missing(None);
        }

        Metric::new(
            used as f32 / total as f32 * 100.0,
            Some(format!("{} из {}", human_bytes(used), human_bytes(total))),
        )
    }

    fn swap(&self) -> Metric {
        let total = self.sys.total_swap();
        // Подкачка, выключенная пользователем, — честный ноль, а не «нет
        // данных»: так и пишем словами, как в снимке системы.
        if total == 0 {
            return Metric::missing(Some("выключена".to_owned()));
        }

        let used = self.sys.used_swap();
        Metric::new(
            used as f32 / total as f32 * 100.0,
            Some(format!("{} из {}", human_bytes(used), human_bytes(total))),
        )
    }

    /// Скорость сети за прошедший промежуток.
    fn network(&self, elapsed: Duration) -> Option<String> {
        let mut down = 0u64;
        let mut up = 0u64;
        let mut counters = 0u64;

        for data in self.nets.list().values() {
            down += data.received();
            up += data.transmitted();
            // Накопленное с загрузки: по нему и отличаем «тихо» от «нечем
            // измерить». Складываем всё в один счётчик — вопрос у нас общий,
            // а не по интерфейсам.
            counters = counters
                .saturating_add(data.total_received())
                .saturating_add(data.total_transmitted());
        }

        if counters == 0 {
            return None;
        }

        Some(format!(
            "Приём {} · Отдача {}",
            human_speed(per_second(down, elapsed)),
            human_speed(per_second(up, elapsed))
        ))
    }

    /// Скорость дисков за прошедший промежуток.
    fn disk(&self, elapsed: Duration) -> Option<String> {
        let mut read = 0u64;
        let mut written = 0u64;
        let mut counters = 0u64;

        for disk in self.disks.list() {
            let usage = disk.usage();
            read += usage.read_bytes;
            written += usage.written_bytes;
            counters = counters
                .saturating_add(usage.total_read_bytes)
                .saturating_add(usage.total_written_bytes);
        }

        if counters == 0 {
            return None;
        }

        Some(format!(
            "Чтение {} · Запись {}",
            human_speed(per_second(read, elapsed)),
            human_speed(per_second(written, elapsed))
        ))
    }

    /// Верхушка списка процессов по загрузке процессора.
    fn processes(&self) -> Vec<ProcRow> {
        // Делить обязательно: `Process::cpu_usage` отдаёт до «100% × число
        // ядер», и на восьмиядерной машине один занятой процесс показывал бы
        // 800%. Ошибка тихая — число выглядит настоящим, просто не тем.
        let cores = self.sys.cpus().len().max(1) as f32;

        let mut rows: Vec<(f32, u64, String)> = self
            .sys
            .processes()
            .values()
            .map(|process| {
                (
                    process.cpu_usage() / cores,
                    process.memory(),
                    process.name().to_string_lossy().into_owned(),
                )
            })
            .collect();

        // `total_cmp`, а не `partial_cmp().unwrap()`: сортировка по `f32`
        // с единственным `NaN` в списке — это паника посреди опроса.
        rows.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        rows.truncate(PROC_LIMIT);

        rows.into_iter()
            .map(|(cpu, mem, name)| ProcRow {
                // Доля процессора нечислом не бывает, но в `cpu_text` она
                // идёт через ту же проверку, что и остальные проценты, —
                // прочерк тут честнее подставленного нуля.
                cpu_text: human_percent(cpu).unwrap_or_else(|| "—".to_owned()),
                mem_text: human_bytes(mem),
                cpu,
                name,
            })
            .collect()
    }
}

/// Что спрашиваем у процессора.
///
/// Не `everything()`: туда входят марка и производитель, а они за секунду не
/// меняются — снимок системы спросил их один раз, и повторять это шестьдесят
/// раз в минуту незачем.
fn cpu_kind() -> sysinfo::CpuRefreshKind {
    sysinfo::CpuRefreshKind::nothing()
        .with_cpu_usage()
        .with_frequency()
}

/// Что спрашиваем у процессов.
///
/// Умолчание `refresh_processes` заодно тянет `with_tasks` и путь к
/// исполняемому файлу — на трёх сотнях процессов это заметная работа каждую
/// секунду ради того, что в списке не показывается.
fn process_kind() -> sysinfo::ProcessRefreshKind {
    sysinfo::ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
}

/// Что спрашиваем у дисков.
///
/// Только ввод-вывод: размер тома и его тип показывает снимок системы, и
/// перечитывать их раз в секунду не нужно. На Linux это заодно избавляет от
/// обхода точек монтирования.
fn disk_kind() -> sysinfo::DiskRefreshKind {
    sysinfo::DiskRefreshKind::nothing().with_io_usage()
}

/// Байты за промежуток — в байты в секунду.
///
/// Нулевой промежуток вернул бы бесконечность, а `human_speed` напечатал бы
/// её как `inf Б/с`. Случается это не только в теории: `Instant` на Windows
/// имеет разрешение около 100 нс, но два вызова подряд после пробуждения
/// потока укладываются и в него.
fn per_second(bytes: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0.0;
    }
    bytes as f64 / secs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Нулевой промежуток не превращается в бесконечную скорость.
    ///
    /// `inf Б/с` в окне — не косметика: этой строкой монитор объявляет, что
    /// сеть работает бесконечно быстро, и ни сборка, ни `clippy` такого
    /// не ловят.
    #[test]
    fn speed_survives_a_zero_interval() {
        assert_eq!(per_second(1024, Duration::ZERO), 0.0);
        assert_eq!(per_second(0, Duration::from_secs(1)), 0.0);
        assert_eq!(per_second(2048, Duration::from_secs(2)), 1024.0);
    }

    /// Остановка будит спящий поток сразу, а не через интервал.
    ///
    /// Ради этого и заведён `Condvar`. С обычным `sleep` проверка ждала бы
    /// целый `INTERVAL`, и десяток переключений вкладки за секунду оставлял
    /// бы за собой десяток досыпающих опросчиков.
    #[test]
    fn stopping_wakes_the_sleeper_at_once() {
        let stop = Arc::new(Stop::default());
        let sleeper = Arc::clone(&stop);

        let started = Instant::now();
        let thread = std::thread::spawn(move || sleeper.wait(Duration::from_secs(30)));
        stop.stop();

        assert!(thread.join().expect("поток опроса упал"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "остановка ждала пробуждения по таймеру"
        );
    }

    /// Настоящий замер на настоящей машине.
    ///
    /// Помечен `#[ignore]`, потому что занимает две секунды и спрашивает
    /// живую систему: в обычном прогоне ему не место. Запускать руками —
    /// `cargo test -- --ignored --nocapture real_sample_from_this_machine`.
    ///
    /// Проверять здесь надо не код возврата, а сами значения, и это ровно
    /// Правило 6: `IOCTL_DISK_PERFORMANCE` на Windows умеет отказать, а
    /// `sysinfo` оставит в поле ноль — от простаивающего диска такой ноль
    /// не отличается ничем. Тест печатает всё, что собрал, чтобы человек
    /// увидел глазами, есть счётчики на этой машине или нет.
    #[test]
    #[ignore = "спрашивает живую систему и занимает две секунды"]
    fn real_sample_from_this_machine() {
        let mut sampler = Sampler::new();
        std::thread::sleep(INTERVAL);
        let sample = sampler.take();

        println!("ЦП:       {:?} / {:?}", sample.cpu.percent_text, sample.cpu.detail);
        println!("Память:   {:?} / {:?}", sample.mem.percent_text, sample.mem.detail);
        println!("Подкачка: {:?}", sample.swap.detail);
        println!("Сеть:     {:?}", sample.net);
        println!("Диски:    {:?}", sample.disk);
        println!("Процессов в списке: {}", sample.procs.len());
        for row in &sample.procs {
            println!("  {:>7}  {:>10}  {}", row.cpu_text, row.mem_text, row.name);
        }

        // Загрузка процессора и память есть на любой из трёх систем: если их
        // нет, сломан сам опрос, а не отдельный счётчик.
        assert!(sample.cpu.percent.is_some(), "система не сказала загрузку ЦП");
        assert!(sample.mem.percent.is_some(), "система не сказала объём памяти");
        assert!(!sample.procs.is_empty(), "список процессов пуст");
        assert!(sample.procs.len() <= PROC_LIMIT);

        // Ради этой проверки тест и написан: `Process::cpu_usage` отдаёт
        // до «100% × число ядер», и забытое деление на ядра проявляется
        // именно здесь — числом вроде «780%» у одного процесса.
        for row in &sample.procs {
            assert!(
                (0.0..=100.0).contains(&row.cpu),
                "доля процессора {} у «{}» не поделена на число ядер",
                row.cpu,
                row.name
            );
        }

        // Список отсортирован по убыванию: верхушку читают сверху вниз.
        for pair in sample.procs.windows(2) {
            assert!(pair[0].cpu >= pair[1].cpu, "список процессов не отсортирован");
        }
    }

    /// Уже остановленный опрос не засыпает вовсе.
    ///
    /// Порядок «пометили — потом легли спать» бывает и таким: `stop` успевает
    /// сработать до того, как поток дошёл до `wait`.
    #[test]
    fn a_stopped_poll_never_sleeps() {
        let stop = Stop::default();
        stop.stop();

        let started = Instant::now();
        assert!(stop.wait(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
