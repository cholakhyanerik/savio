//! Движение интерфейса: длительности, кривые и лерпы.
//!
//! Кладётся рядом с `theme.rs` и подключается в `main.rs` как `mod motion;`.
//! Здесь только числа и функции — про yt-dlp, процессы и виджеты этот модуль
//! не знает ничего, ровно как `theme.rs`.
//!
//! Правило то же, что у темы: значения берутся отсюда, а не пишутся по месту.
//! Две одинаковые по смыслу анимации, разъехавшиеся по длительности, читаются
//! как небрежность — и заметить это можно только глазами, ни сборка, ни тесты
//! разницы в 60 мс не видят.

use eframe::egui::{Color32, Context, Id, Pos2, Rect, Rgba, Vec2, pos2};

// ---------------------------------------------------------------------------
// Длительности
//
// Три штуки на весь интерфейс, и четвёртой заводить не нужно: как только их
// становится пять, движение перестаёт читаться как одно целое.
// ---------------------------------------------------------------------------

/// Отклик под курсором и нажатие: граница, заливка, подпись.
pub const TOUCH: f32 = 0.12;
/// Состояние: перетекающая таблетка, раскрытие группы, смена цвета, строка
/// очереди, доезжающее число.
pub const MOVE: f32 = 0.24;
/// Слой: смена раздела, появление карточки, модальное окно.
pub const LAYER: f32 = 0.30;

/// Блик по кромке стекла — один раз при появлении карточки.
pub const GLOSS: f32 = 1.10;
/// Дыхание точки «идёт сейчас».
pub const BREATH: f32 = 1.60;
/// Такт замера монитора: график едет ровно на один шаг за это время.
pub const SAMPLE: f32 = 1.00;

/// Насколько таблетка выбора вытягивается в пути. Больше 1.12 — и она
/// начинает выглядеть резиновой, а не жидкой.
pub const STRETCH: f32 = 1.10;

/// Задержка между соседними всплывающими карточками — те самые 40 мс из
/// спецификации, но долей, а не миллисекундами: она накладывается на готовый
/// коэффициент `animate_bool_with_time`, а тот идёт от нуля к единице за
/// `LAYER`. Отсюда и число: 0.04 / 0.30.
///
/// Тип указан явно, и это не украшение. Без него `1.0 - STAGGER` остаётся
/// «каким-то числом с точкой», и вызов `.max` по нему — ошибка компиляции
/// на ровном месте (E0689), ровно на которой черновик и не собрался.
pub const STAGGER: f32 = 0.13;

/// Насколько ниже своего места начинает всплывающая карточка, в точках.
///
/// Десять — это заметно глазу и незаметно раскладке: соседи сдвигаются на
/// то же время, за которое карточка доезжает, и перестановки на экране не
/// происходит (её спецификация запрещает отдельным пунктом).
pub const RISE: f32 = 10.0;

// ---------------------------------------------------------------------------
// Кривые
//
// Три штуки, по одной на роль. Приходящее тормозит у цели (`glide`),
// выбираемое слегка перелетает (`liquid`), уходящее идёт ровно (`fade`) —
// провожать взглядом там нечего.
//
// Сигнатура у всех трёх — `fn(f32) -> f32`, ровно та, которую ждёт
// `Context::animate_bool_with_time_and_easing` в egui 0.36. То есть кривую
// можно и наложить на готовый коэффициент, и отдать egui целиком.
// ---------------------------------------------------------------------------

/// Приход: ease-out. Соответствует cubic-bezier(.22, 1, .36, 1).
pub fn glide(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(5)
}

/// Выбор: ease-out с перелётом. Соответствует cubic-bezier(.34, 1.32, .5, 1).
pub fn liquid(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    const C1: f32 = 1.32;
    const C3: f32 = C1 + 1.0;
    1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
}

/// Уход и пульсация: ease-in-out. Соответствует cubic-bezier(.4, 0, .6, 1).
pub fn fade(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
    }
}

// ---------------------------------------------------------------------------
// Выключатель
// ---------------------------------------------------------------------------

/// Множитель длительностей. Ноль означает «без движения»: все `animate_*`
/// с нулевым временем встают на конечное значение тем же кадром, поэтому
/// отдельной ветки «если анимации выключены» нигде писать не нужно.
///
/// Системного «уменьшить движение» ни eframe, ни winit не отдают, так что
/// это настройка Savio: галочка в подвале, которая запоминается вместе
/// с остальными в `settings.json`.
///
/// Оговорка по egui 0.36, проверенная по исходникам `animation_manager.rs`:
/// «тем же кадром» верно для `animate_bool_with_time` (деление на ноль даёт
/// не-число, и egui подставляет цель), а `animate_value_with_time` на нулевом
/// времени отдаёт цель **следующим** кадром — кадр он при этом просит сам,
/// так что задержка равна одному кадру и глазом не видна. Городить ради неё
/// обход не нужно; знать о ней стоит, чтобы не искать несуществующий баг.
pub fn scale(enabled: bool) -> f32 {
    if enabled { 1.0 } else { 0.0 }
}

// ---------------------------------------------------------------------------
// Лерпы
// ---------------------------------------------------------------------------

/// Смешивает два цвета. Через `Rgba`, а не покомпонентно по `Color32`:
/// последний хранит цвет в гамме, и линейное смешивание в ней даёт грязный
/// провал в середине — заметнее всего на паре «оранжевый → шалфейный»
/// у точки статуса.
pub fn mix(from: Color32, to: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let (a, b) = (Rgba::from(from), Rgba::from(to));
    Color32::from(a * (1.0 - t) + b * t)
}

/// Перелёт прямоугольника: откуда тронулся, куда едет и когда начал.
///
/// Живёт в памяти egui рядом с её собственными `animate_*`, а не в поле
/// приложения: дорожек в окне пять, и знать про каждую ни `SavioApp`, ни этот
/// модуль не должны. Пишется только на смену цели, читается раз в кадр.
#[derive(Clone, Copy)]
struct Flight {
    from: Rect,
    to: Rect,
    since: f64,
}

/// Где перелёт находится сейчас.
///
/// Доехавший отдаёт `to` **тем же значением**, а не досчитанным до него:
/// иначе `rect` не смог бы отличить «доехали» от «почти доехали» и просил бы
/// кадры вечно. Это и есть та самая дисциплина кадров, ради которой всё.
fn flown(flight: &Flight, now: f64, time: f32, ease: fn(f32) -> f32) -> Rect {
    let step = (now - flight.since) as f32;
    if time <= 0.0 || step >= time {
        return flight.to;
    }
    let t = ease((step / time).max(0.0));
    let at =
        |from: Pos2, to: Pos2| pos2(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
    Rect::from_min_max(
        at(flight.from.min, flight.to.min),
        at(flight.from.max, flight.to.max),
    )
}

/// Прямоугольник, доезжающий до цели по заданной кривой.
///
/// Своя память, а не `Context::animate_value_with_time`, и это не
/// изобретательство. У egui значения едут **строго линейно**: `animate_value`
/// раскладывает время по `remap_clamp`, и наложить кривую снаружи не на что —
/// доля пути наружу не выдаётся. Таблетке же выбора нужен перелёт (`liquid`),
/// без него она не «капля», а ползунок; ровно поэтому в наборе три кривые,
/// а не одна.
///
/// Именно `Rect`, а не отдельные x и ширина: у таблетки едут оба, и
/// разъехавшись на кадр они дают заметный перекос на широких сегментах.
///
/// Смена цели на полпути трогает с того места, где таблетка сейчас, а не с
/// начала: иначе второй щелчок подряд дёргал бы её назад.
pub fn rect(ctx: &Context, id: Id, target: Rect, time: f32, ease: fn(f32) -> f32) -> Rect {
    let now = ctx.input(|i| i.time);

    let Some(mut flight) = ctx.data(|d| d.get_temp::<Flight>(id)) else {
        // Впервые увидели дорожку — таблетка обязана уже стоять на месте.
        // Прилёт из левого верхнего угла при открытии окна выглядел бы
        // не приёмом, а недоделкой.
        ctx.data_mut(|d| {
            d.insert_temp(
                id,
                Flight {
                    from: target,
                    to: target,
                    since: now,
                },
            );
        });
        return target;
    };

    if flight.to != target {
        flight = Flight {
            from: flown(&flight, now, time, ease),
            to: target,
            since: now,
        };
        ctx.data_mut(|d| d.insert_temp(id, flight));
    }

    let at = flown(&flight, now, time, ease);
    if at != flight.to {
        ctx.request_repaint();
    }
    at
}

/// Насколько ужимается нажимаемое своими руками (приём 02).
///
/// Полтора процента ширины — предел: больше, и «таблетка» начинает прыгать,
/// а не приминаться. Считается от ширины, а не от высоты: у широкой кнопки
/// сдвиг кромки должен быть тем же на глаз, а не тем же в точках.
pub const PRESS: f32 = 0.0075;

/// Прямоугольник, примятый нажатием.
///
/// `k` — коэффициент от `Response::is_pointer_button_down_on`, доведённый
/// `glide` за `TOUCH * 0.75`: нажатие короче отклика на приближение, иначе
/// кнопка «залипает» под пальцем.
pub fn pressed(rect: Rect, k: f32) -> Rect {
    rect.shrink(rect.width() * PRESS * k.clamp(0.0, 1.0))
}

/// Доля пути, пройденная от заданного мгновения.
///
/// Пока путь не пройден, кадры просятся сами; как только пройден — перестают.
/// Это и есть дисциплина кадров в одной строчке.
pub fn elapsed(ctx: &Context, start: f64, time: f32) -> f32 {
    if time <= 0.0 {
        return 1.0;
    }
    let t = ((ctx.input(|i| i.time) - start) as f32 / time).clamp(0.0, 1.0);
    if t < 1.0 {
        ctx.request_repaint();
    }
    t
}

/// Доля пути с того мгновения, когда элемент увидели впервые.
///
/// `animate_bool_with_time` для этого не годится, и промах тут молчаливый:
/// незнакомый идентификатор он отдаёт **сразу конечным** значением, то есть
/// строка, только что вставленная в список, появилась бы уже целиком — а весь
/// смысл приёма в том, чтобы она раздвинула соседей. Здесь засекается сам
/// первый показ.
pub fn since_first_seen(ctx: &Context, id: Id, time: f32) -> f32 {
    if let Some(start) = ctx.data(|d| d.get_temp::<f64>(id)) {
        return elapsed(ctx, start, time);
    }
    let now = ctx.input(|i| i.time);
    ctx.data_mut(|d| d.insert_temp(id, now));
    if time <= 0.0 {
        return 1.0;
    }
    ctx.request_repaint();
    0.0
}

/// Переливание цвета: откуда, куда и когда началось.
struct Tint {
    from: Color32,
    to: Color32,
    since: f64,
}

/// Цвет, переливающийся в новый за `time`.
///
/// Устроен как [`rect`] и по той же причине: у egui есть только линейная
/// анимация одного числа, а цвет — это четыре числа, и разъехавшись на кадр
/// они дают грязный оттенок в середине перехода. Кривая здесь `fade`: смена
/// состояния — это уход старого, а провожать взглядом там нечего.
pub fn tint(ctx: &Context, id: Id, target: Color32, time: f32) -> Color32 {
    let now = ctx.input(|i| i.time);

    let Some(mut flight) = ctx.data(|d| {
        d.get_temp::<(Color32, Color32, f64)>(id)
            .map(|(from, to, since)| Tint { from, to, since })
    }) else {
        ctx.data_mut(|d| d.insert_temp(id, (target, target, now)));
        return target;
    };

    if flight.to != target {
        let at = tinted(&flight, now, time);
        flight = Tint {
            from: at,
            to: target,
            since: now,
        };
        ctx.data_mut(|d| d.insert_temp(id, (flight.from, flight.to, flight.since)));
    }

    let at = tinted(&flight, now, time);
    if at != flight.to {
        ctx.request_repaint();
    }
    at
}

fn tinted(flight: &Tint, now: f64, time: f32) -> Color32 {
    let step = (now - flight.since) as f32;
    if time <= 0.0 || step >= time {
        return flight.to;
    }
    mix(flight.from, flight.to, fade((step / time).max(0.0)))
}

/// Таблетка выбора: доезжает до сегмента и вытягивается по ходу.
///
/// Растяжение считается из того, насколько она сейчас далека от цели, а не
/// из времени: так один и тот же сегмент, выбранный соседним и дальним
/// нажатием, тянется по-разному — ровно как капля.
pub fn pill(ctx: &Context, id: Id, target: Rect, time: f32) -> Rect {
    let shown = rect(ctx, id, target, time, liquid);
    let away = (shown.center().x - target.center().x).abs() / target.width().max(1.0);
    let stretch = 1.0 + (STRETCH - 1.0) * away.min(1.0);
    Rect::from_center_size(
        shown.center(),
        Vec2::new(shown.width() * stretch, shown.height()),
    )
}

/// Дыхание: 0.55 → 1.0 и обратно, `BREATH` на полный цикл.
///
/// Кадры к сроку приходится просить самому: без ввода egui окно не
/// перерисовывает, и точка замерла бы до первого движения мыши.
///
/// Выключатель здесь не для полноты, а ради того же счёта кадров: это одна
/// из трёх бесконечных анимаций набора, и с выключенными переходами она
/// обязана не будить окно вовсе, а не дышать втихую.
pub fn breath(ctx: &Context, speed: f32) -> f32 {
    if speed <= 0.0 {
        return 1.0;
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(33));
    let t = ctx.input(|i| i.time) as f32;
    0.55 + 0.45 * fade((t / BREATH % 1.0 * 2.0 - 1.0).abs())
}

/// Насколько далеко продвинулась карточка номер `delay_steps`, когда весь
/// приход прошёл долю `t`.
///
/// Соседние карточки приезжают не разом, а сверху вниз: это и отличает смену
/// раздела от простого проявления. Задержка съедает начало общего хода, а
/// остаток растягивается на полный путь — иначе последняя карточка не успела
/// бы доехать вовсе.
pub fn stagger(t: f32, delay_steps: u32) -> f32 {
    let shifted = (t - delay_steps as f32 * STAGGER).clamp(0.0, 1.0) / (1.0 - STAGGER).max(0.01);
    glide(shifted.min(1.0))
}

/// Точка на графике с дробным сдвигом: ломаная едет влево ровно на шаг за
/// такт замера, поэтому новая точка вползает, а не появляется скачком.
pub fn trace_x(right: f32, index: usize, newest: usize, step: f32, phase: f32) -> f32 {
    right - (newest - index) as f32 * step - phase.clamp(0.0, 1.0) * step
}

/// Точка ломаной целиком — чтобы не считать y в двух местах.
pub fn trace_point(
    rect: Rect,
    value: f32,
    index: usize,
    newest: usize,
    step: f32,
    phase: f32,
) -> Pos2 {
    pos2(
        trace_x(rect.right(), index, newest, step, phase),
        rect.bottom() - (value / 100.0).clamp(0.0, 1.0) * rect.height(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Все три кривые обязаны выходить из нуля и приходить ровно в единицу.
    /// Промах здесь не видно глазами — движение просто не доезжает или
    /// начинается с рывка, — а стоит он одной опечатки в показателе степени.
    #[test]
    fn every_curve_runs_from_zero_to_one() {
        for curve in [glide as fn(f32) -> f32, liquid, fade] {
            assert!(curve(0.0).abs() < 1e-5, "начало не в нуле: {}", curve(0.0));
            assert!(
                (curve(1.0) - 1.0).abs() < 1e-5,
                "конец не в единице: {}",
                curve(1.0)
            );
        }
    }

    /// Значение за пределами [0, 1] кривая обязана зажать, а не продолжить.
    /// Без этого `liquid` за единицей уходит вверх кубически, и один
    /// подзатянувшийся кадр вышвырнул бы таблетку за пределы дорожки.
    #[test]
    fn every_curve_clamps_its_input() {
        for curve in [glide as fn(f32) -> f32, liquid, fade] {
            assert_eq!(curve(-3.0), curve(0.0));
            assert_eq!(curve(7.0), curve(1.0));
        }
    }

    /// `glide` и `fade` не отступают назад, а `fade` вдобавок симметрична:
    /// на середине пути ровно половина. Это то, чем она отличается от `glide`
    /// и ради чего заведена отдельно.
    #[test]
    fn glide_and_fade_only_move_forward() {
        let mut previous = (0.0_f32, 0.0_f32);
        for step in 0..=100 {
            let t = step as f32 / 100.0;
            let (g, f) = (glide(t), fade(t));
            assert!(g >= previous.0 - 1e-6, "glide отступила на {t}");
            assert!(f >= previous.1 - 1e-6, "fade отступила на {t}");
            previous = (g, f);
        }
        assert!((fade(0.5) - 0.5).abs() < 1e-5);
    }

    /// А `liquid` обязана перелететь: без перелёта она неотличима от `glide`,
    /// и весь смысл третьей кривой пропадает. Перелёт при этом умеренный —
    /// таблетка выезжает за сегмент на считаные проценты, а не подпрыгивает.
    #[test]
    fn liquid_overshoots_but_not_wildly() {
        let peak = (0..=100)
            .map(|step| liquid(step as f32 / 100.0))
            .fold(0.0_f32, f32::max);
        assert!(peak > 1.0, "перелёта нет вовсе: {peak}");
        assert!(peak < 1.15, "перелёт слишком велик: {peak}");
    }

    /// Выключатель обязан давать ровно ноль: на нём держится обещание
    /// «отдельной ветки „если анимации выключены“ писать не нужно нигде».
    #[test]
    fn the_switch_gives_exactly_zero() {
        assert_eq!(scale(true), 1.0);
        assert_eq!(scale(false), 0.0);
    }

    /// Концы смешивания цветов должны быть точными: на нуле — исходный цвет,
    /// на единице — целевой, без потери байта на округлении гаммы.
    #[test]
    fn mixing_hits_both_ends_exactly() {
        let from = Color32::from_rgb(224, 122, 63);
        let to = Color32::from_rgb(174, 191, 146);
        assert_eq!(mix(from, to, 0.0), from);
        assert_eq!(mix(from, to, 1.0), to);
        assert_eq!(mix(from, to, -1.0), from);
        assert_eq!(mix(from, to, 2.0), to);
        assert_eq!(mix(from, from, 0.5), from);
    }

    /// Смешивание идёт по линейному свету, а не по гамме. Проверяется тем,
    /// что середина светлее наивного покомпонентного среднего: ровно этот
    /// провал в середине и был причиной считать через `Rgba`.
    #[test]
    fn mixing_goes_through_linear_light() {
        let from = Color32::from_rgb(224, 122, 63);
        let to = Color32::from_rgb(174, 191, 146);
        let middle = mix(from, to, 0.5);
        let naive = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
        assert!(
            middle.r() > naive(from.r(), to.r())
                && middle.g() > naive(from.g(), to.g())
                && middle.b() > naive(from.b(), to.b()),
            "середина не светлее гамма-среднего: {middle:?}"
        );
    }

    /// Свежая точка графика на нулевой фазе стоит ровно у правого края, а за
    /// полный такт ломаная уезжает ровно на один шаг — не на полтора и не на
    /// половину. Ошибка здесь выглядит как ускоряющийся или дёргающийся
    /// график, то есть враньём о том, чего на машине не происходило.
    #[test]
    fn the_trace_slides_exactly_one_step_per_sample() {
        let step = 12.0;
        assert_eq!(trace_x(300.0, 9, 9, step, 0.0), 300.0);
        assert_eq!(trace_x(300.0, 9, 9, step, 1.0), 300.0 - step);
        assert_eq!(trace_x(300.0, 8, 9, step, 0.0), 300.0 - step);
        // Фаза за пределами такта не должна уносить ломаную дальше шага.
        assert_eq!(trace_x(300.0, 9, 9, step, 3.0), 300.0 - step);
    }

    /// Ноль процентов ложится на нижнюю грань, сотня — на верхнюю, а замер
    /// сверх сотни (WMI такое отдаёт) не уходит выше графика.
    #[test]
    fn the_trace_keeps_its_values_inside_the_plot() {
        let plot = Rect::from_min_max(pos2(0.0, 40.0), pos2(300.0, 100.0));
        assert_eq!(trace_point(plot, 0.0, 9, 9, 12.0, 0.0).y, plot.bottom());
        assert_eq!(trace_point(plot, 100.0, 9, 9, 12.0, 0.0).y, plot.top());
        assert_eq!(trace_point(plot, 140.0, 9, 9, 12.0, 0.0).y, plot.top());
        assert_eq!(trace_point(plot, -5.0, 9, 9, 12.0, 0.0).y, plot.bottom());
    }

    // -----------------------------------------------------------------------
    // Перелёт прямоугольника — тот единственный кусок модуля, который держит
    // своё состояние и сам просит кадры. Отсюда и тесты с настоящим
    // `Context`: и «доехала», и «перестала будить окно» невидимы ни сборке,
    // ни глазам — в окне доехавшая таблетка выглядит точно так же, как
    // таблетка, которая продолжает пересчитываться шестьдесят раз в секунду.
    // -----------------------------------------------------------------------

    use eframe::egui::{RawInput, ViewportId};

    /// Прогоняет один кадр в заданный момент времени и говорит, попросил ли
    /// контекст следующий кадр немедленно.
    ///
    /// `drop_without_applying_deltas` обязателен: `TexturesDelta` роняет
    /// процесс, если её выбросили с неприменёнными правками, а применять их
    /// в тесте нечем — рисовать некуда.
    fn frame(ctx: &Context, at: f64, mut run: impl FnMut(&Context)) -> (bool, std::time::Duration) {
        let input = RawInput {
            time: Some(at),
            ..Default::default()
        };
        let out = ctx.run_ui(input, |ui| run(ui.ctx()));
        let delay = out
            .viewport_output
            .get(&ViewportId::ROOT)
            .map_or(std::time::Duration::MAX, |v| v.repaint_delay);
        out.drop_without_applying_deltas();
        (delay == std::time::Duration::ZERO, delay)
    }

    /// Гоняет кадры, пока контекст не перестанет просить следующий, и отдаёт
    /// момент, на котором это случилось.
    ///
    /// Свежий `Context` просит несколько проходов сам по себе — раскладка,
    /// текстуры, первый кадр, — так что судить по самому первому ответу
    /// нельзя: тест мерил бы egui, а не нас.
    fn settle(ctx: &Context, from: f64, mut run: impl FnMut(&Context)) -> f64 {
        let mut at = from;
        for _ in 0..40 {
            let (asked, _) = frame(ctx, at, &mut run);
            if !asked {
                return at;
            }
            at += 0.016;
        }
        panic!("контекст не успокоился за сорок кадров");
    }

    /// Дорожка, увиденная впервые, обязана отдать цель как есть: таблетка,
    /// прилетающая из угла окна при открытии, — не приём, а недоделка.
    #[test]
    fn the_pill_starts_where_it_belongs() {
        let ctx = Context::default();
        let target = Rect::from_min_max(pos2(10.0, 4.0), pos2(90.0, 34.0));
        let mut seen = Rect::NOTHING;
        frame(&ctx, 0.0, |ctx| {
            seen = rect(ctx, Id::new("track"), target, MOVE, liquid);
        });
        assert_eq!(seen, target);
    }

    /// Доехав, перелёт обязан перестать просить кадры. Забытый `request_repaint`
    /// не виден в окне вовсе, а стоит он шестидесяти кадров в секунду —
    /// в покое, вечно, ровно та беда, о которой Правило 1.
    #[test]
    fn a_finished_flight_stops_asking_for_frames() {
        let ctx = Context::default();
        let id = Id::new("track");
        let first = Rect::from_min_max(pos2(10.0, 4.0), pos2(90.0, 34.0));
        let second = Rect::from_min_max(pos2(100.0, 4.0), pos2(180.0, 34.0));

        let quiet = settle(&ctx, 0.0, |ctx| {
            rect(ctx, id, first, MOVE, liquid);
        });

        // Сменили цель — таблетка в пути, и кадры нужны.
        let (asked, _) = frame(&ctx, quiet + 0.016, |ctx| {
            rect(ctx, id, second, MOVE, liquid);
        });
        assert!(asked, "таблетка в пути, а кадров не просит");

        // Дали ей доехать и снова успокоиться.
        let quiet = settle(&ctx, quiet + 0.032, |ctx| {
            rect(ctx, id, second, MOVE, liquid);
        });

        let mut seen = Rect::NOTHING;
        let (asked, delay) = frame(&ctx, quiet + 0.016, |ctx| {
            seen = rect(ctx, id, second, MOVE, liquid);
        });
        assert_eq!(seen, second, "не доехала до цели");
        assert!(
            !asked,
            "доехала, но продолжает будить окно (задержка {delay:?})"
        );
    }

    /// С выключенными «Плавными переходами» время равно нулю, и перелёт обязан
    /// встать на цель тем же кадром, ни разу не попросив следующий.
    #[test]
    fn the_switch_puts_the_pill_on_target_at_once() {
        let ctx = Context::default();
        let id = Id::new("track");
        let first = Rect::from_min_max(pos2(10.0, 4.0), pos2(90.0, 34.0));
        let second = Rect::from_min_max(pos2(100.0, 4.0), pos2(180.0, 34.0));

        let quiet = settle(&ctx, 0.0, |ctx| {
            rect(ctx, id, first, MOVE * scale(false), liquid);
        });
        let mut seen = Rect::NOTHING;
        let (asked, _) = frame(&ctx, quiet + 0.016, |ctx| {
            seen = rect(ctx, id, second, MOVE * scale(false), liquid);
        });
        assert_eq!(seen, second, "не встала на цель тем же кадром");
        assert!(!asked, "движение выключено, а кадры всё равно просятся");
    }

    /// Второй щелчок на полпути трогает таблетку с того места, где она сейчас,
    /// а не с начала: иначе она заметно дёргается назад.
    #[test]
    fn a_second_click_does_not_throw_the_pill_back() {
        let ctx = Context::default();
        let id = Id::new("track");
        let left = Rect::from_min_max(pos2(0.0, 4.0), pos2(80.0, 34.0));
        let middle = Rect::from_min_max(pos2(90.0, 4.0), pos2(170.0, 34.0));
        let right = Rect::from_min_max(pos2(180.0, 4.0), pos2(260.0, 34.0));

        frame(&ctx, 0.0, |ctx| {
            rect(ctx, id, left, MOVE, liquid);
        });
        frame(&ctx, 0.01, |ctx| {
            rect(ctx, id, middle, MOVE, liquid);
        });
        // На середине пути к среднему сегменту просим дальний.
        let mut halfway = Rect::NOTHING;
        frame(&ctx, 0.01 + (MOVE / 2.0) as f64, |ctx| {
            halfway = rect(ctx, id, middle, MOVE, liquid);
        });
        let mut after = Rect::NOTHING;
        frame(&ctx, 0.01 + (MOVE / 2.0) as f64 + 0.001, |ctx| {
            after = rect(ctx, id, right, MOVE, liquid);
        });
        assert!(
            after.left() >= halfway.left() - 1.0,
            "таблетку отбросило назад: было {}, стало {}",
            halfway.left(),
            after.left()
        );
    }
}
