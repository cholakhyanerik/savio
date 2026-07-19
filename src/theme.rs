//! Оформление: тёмный фон и жёлтый акцент.
//!
//! Здесь только цвета и настройка `egui::Style` — слой UI, как и `app.rs`.
//! Про yt-dlp и процессы этот модуль не знает ничего.
//!
//! Стиль собирается **один раз** при старте (`apply`), а не в кадре отрисовки:
//! `Style` содержит `BTreeMap` шрифтов, и пересборка его 60 раз в секунду
//! была бы чистой потерей времени.

use std::sync::Arc;

use eframe::egui::{
    Color32, CornerRadius, Context, FontFamily, FontId, Stroke, Style, TextStyle, Theme,
    ThemePreference, Vec2, Visuals,
};

// ---------------------------------------------------------------------------
// Палитра
//
// Каждая пара «текст на фоне» проверена по WCAG 2.1 (формула относительной
// яркости), коэффициенты указаны в комментариях. Порог для основного текста —
// 4.5:1, для крупного текста и границ элементов управления — 3:1.
// Комбинации, не указанные здесь, использовать не следует: они не проверены.
// ---------------------------------------------------------------------------

// Поверхности.
/// Фон окна и панелей.
pub const BG_ROOT: Color32 = Color32::from_rgb(18, 18, 18);
/// Карточка с основными элементами управления, шапка.
pub const BG_SURFACE: Color32 = Color32::from_rgb(26, 26, 26);
/// Приподнятые мелочи: плашка статуса, тело журнала, наведение на кнопку.
pub const BG_ELEVATED: Color32 = Color32::from_rgb(36, 36, 36);
/// Нажатое состояние вторичной кнопки.
pub const BG_PRESSED: Color32 = Color32::from_rgb(46, 46, 46);
/// Поле ввода и дорожка переключателя формата — «утоплены» глубже фона.
pub const BG_INPUT: Color32 = Color32::from_rgb(15, 15, 15);
/// Жёлоб прогресс-бара.
pub const PROGRESS_TRACK: Color32 = Color32::from_rgb(48, 48, 48);

// Границы.
/// Декоративная линия: край карточки, разделитель под шапкой.
/// Намеренно почти незаметна — не годится как единственный признак элемента.
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(43, 43, 43);
/// Граница элементов управления. 3.78:1 на `BG_ROOT` — проходит порог 3:1.
pub const BORDER_STRONG: Color32 = Color32::from_rgb(112, 112, 112);
/// Граница при наведении. Отдельный тон нужен потому, что `BORDER_STRONG`
/// на `BG_PRESSED` даёт лишь 2.74:1 и порог 3:1 не проходит.
pub const BORDER_HOVER: Color32 = Color32::from_rgb(119, 119, 119);

// Текст.
/// Основной текст. Не чистый белый: на почти чёрном фоне он «звенит».
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(242, 242, 242);
/// Подписи, метаданные, строка прогресса. Минимум 6.85:1 — проходит AA.
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(184, 184, 184);
/// Только журнал и подсказка в пустом поле. На `BG_PRESSED` уже не проходит
/// порог, поэтому на нажатых поверхностях не применяется.
pub const TEXT_MUTED: Color32 = Color32::from_rgb(148, 148, 148);
/// Текст на жёлтой кнопке. Белый здесь дал бы 1.4:1 — нечитаемо.
/// Тёмная подпись на акценте даёт 12.4:1.
pub const TEXT_ON_ACCENT: Color32 = Color32::from_rgb(26, 20, 0);

// Акцент.
pub const ACCENT: Color32 = Color32::from_rgb(255, 215, 0);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(255, 224, 77);
pub const ACCENT_ACTIVE: Color32 = Color32::from_rgb(230, 194, 0);
/// Приглушённый акцент выключенной кнопки: заметно тусклее активного,
/// но не выглядит поломкой.
pub const ACCENT_DISABLED: Color32 = Color32::from_rgb(133, 117, 40);
/// Подсветка выделенного текста в поле ввода. Тёмная, чтобы сам текст
/// поверх неё оставался читаемым.
pub const ACCENT_SELECTION: Color32 = Color32::from_rgb(74, 63, 0);

// Состояния.
pub const STATE_SUCCESS: Color32 = Color32::from_rgb(74, 222, 128);
pub const STATE_ERROR: Color32 = Color32::from_rgb(248, 113, 113);
pub const STATE_WARNING: Color32 = Color32::from_rgb(255, 149, 80);

// ---------------------------------------------------------------------------
// Метрики
// ---------------------------------------------------------------------------

/// Скругление элементов управления.
pub const RADIUS: u8 = 8;
/// Скругление мелких элементов: плашка статуса, сегменты переключателя.
pub const RADIUS_SMALL: u8 = 6;
/// Высота главной кнопки «Скачать».
pub const CTA_HEIGHT: f32 = 40.0;
/// Высота поля ввода и вторичных кнопок.
pub const CONTROL_HEIGHT: f32 = 32.0;

/// Собирает стиль и ставит его в контекст.
///
/// Вызывается один раз при создании приложения. Тема задаётся жёстко и не
/// зависит от системной светлой/тёмной схемы: приложение всегда тёмное,
/// иначе часть палитры перестала бы проходить по контрасту.
pub fn apply(ctx: &Context) {
    let mut style = Style {
        visuals: visuals(),
        ..Style::default()
    };

    style.text_styles = [
        (TextStyle::Heading, FontId::new(22.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();

    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(8.0, 8.0);
    spacing.button_padding = Vec2::new(14.0, 8.0);
    spacing.interact_size = Vec2::new(0.0, CONTROL_HEIGHT);
    spacing.icon_width = 16.0;
    spacing.icon_width_inner = 9.0;

    // По умолчанию egui следует за системной схемой (`ThemePreference::System`),
    // и на светлой ОС приложение открылось бы со светлым стилем. Тема Savio
    // тёмная всегда: часть палитры на светлом фоне не прошла бы по контрасту.
    ctx.set_theme(ThemePreference::Dark);

    // Стиль кладём в оба слота: если egui всё же переключит тему (например,
    // при смене системной схемы на ходу), внешний вид не поедет.
    let style = Arc::new(style);
    ctx.set_style_of(Theme::Dark, Arc::clone(&style));
    ctx.set_style_of(Theme::Light, style);
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();

    v.panel_fill = BG_ROOT;
    v.window_fill = BG_ROOT;
    v.faint_bg_color = BG_SURFACE;
    v.extreme_bg_color = BG_INPUT;
    // Поле ввода красим напрямую, не полагаясь на `extreme_bg_color`.
    v.text_edit_bg_color = Some(BG_INPUT);
    v.code_bg_color = BG_ELEVATED;
    v.window_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    v.window_corner_radius = CornerRadius::same(RADIUS + 4);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.warn_fg_color = STATE_WARNING;
    v.error_fg_color = STATE_ERROR;
    // `ui.weak()` по умолчанию берёт полупрозрачный основной цвет, из-за чего
    // контраст плавает. Задаём его явно проверенным тоном.
    v.weak_text_color = Some(TEXT_SECONDARY);

    // Фокус и выделение текста — акцентные.
    v.selection.bg_fill = ACCENT_SELECTION;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.text_cursor.stroke = Stroke::new(2.0, ACCENT);

    let radius = CornerRadius::same(RADIUS);

    // Неинтерактивное: подписи, рамки, разделители.
    let w = &mut v.widgets.noninteractive;
    w.bg_fill = BG_SURFACE;
    w.weak_bg_fill = BG_SURFACE;
    w.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = radius;

    // Покой: вторичные кнопки, поле ввода.
    let w = &mut v.widgets.inactive;
    w.bg_fill = BG_SURFACE;
    w.weak_bg_fill = BG_SURFACE;
    w.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = radius;
    w.expansion = 0.0;

    // Наведение.
    let w = &mut v.widgets.hovered;
    w.bg_fill = BG_ELEVATED;
    w.weak_bg_fill = BG_ELEVATED;
    w.bg_stroke = Stroke::new(1.0, BORDER_HOVER);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = radius;
    w.expansion = 1.0;

    // Нажатие.
    let w = &mut v.widgets.active;
    w.bg_fill = BG_PRESSED;
    w.weak_bg_fill = BG_PRESSED;
    w.bg_stroke = Stroke::new(1.0, ACCENT);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = radius;
    w.expansion = 0.0;

    // Раскрытый список / развёрнутый «Журнал».
    let w = &mut v.widgets.open;
    w.bg_fill = BG_ELEVATED;
    w.weak_bg_fill = BG_ELEVATED;
    w.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = radius;

    v
}
