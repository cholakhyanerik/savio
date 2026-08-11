//! Оформление: тёплый тёмный фон, песочный текст, оранжевый акцент.
//!
//! Здесь только цвета, шрифты, метрики и настройка `egui::Style` — слой UI,
//! как и `app.rs`. Про yt-dlp и процессы этот модуль не знает ничего.
//!
//! Стиль собирается **один раз** при старте (`apply`), а не в кадре отрисовки:
//! `Style` содержит `BTreeMap` шрифтов, и пересборка его 60 раз в секунду
//! была бы чистой потерей времени. По той же причине один раз собирается и
//! сетка фона ([`Backdrop`]): она зависит только от размера окна.

use std::sync::{Arc, LazyLock};

use eframe::egui::{
    Color32, CornerRadius, Context, FontData, FontDefinitions, FontFamily, FontId, Frame,
    InnerResponse, Margin, Mesh, Painter, Pos2, Rangef, Rect, Shadow, Shape, Stroke, Style,
    TextStyle, Theme, ThemePreference, Ui, Vec2, Visuals, pos2,
};

// ---------------------------------------------------------------------------
// Палитра
//
// Каждая пара «текст на фоне» проверена по WCAG 2.1 (формула относительной
// яркости), коэффициенты указаны в комментариях. Порог для основного текста —
// 4.5:1, для крупного текста и границ элементов управления — 3:1.
// Комбинации, не указанные здесь, использовать не следует: они не проверены.
//
// Считать контраст здесь труднее, чем в прежней плоской теме, и вот почему.
// Карточки полупрозрачные (это и есть «стекло» из макета), а фон под ними —
// не один цвет, а три тёплых пятна на почти чёрном (см. [`Backdrop`]).
// Значит, у каждого текста не один фон, а диапазон, и проверять надо **самый
// светлый** его край: там контраст наименьший. Худший случай посчитан обходом
// сетки по шести размерам окна, от 520×420 до 3840×2160, и он приходится на
// маленькое окно — пятна в нём занимают всю площадь. Числа ниже приведены
// для него:
//
//     фон               rgb(49, 41, 33)
//     карточка          rgb(64, 57, 48)   фон + 7.5% песочного
//     вложенная         rgb(74, 67, 59)   карточка + 5.5% песочного
//
// Отсюда два расхождения с макетом, оба намеренные и оба — в пользу правила,
// а не вкуса. Приглушённый текст в макете `#a19786`: на вложенной карточке
// это 3.5:1, порог 4.5 не проходит. Границы в макете — песочный с прозрачностью
// 0.14: это 1.5:1, порог 3:1 не проходит даже близко. Оба подняты до
// проходящих значений; всё остальное взято из макета как есть.
// ---------------------------------------------------------------------------

// Поверхности.
/// Основа фона: почти чёрный тёплого тона. Поверх него [`Backdrop`] кладёт
/// три пятна и общую вуаль — вместе это и есть фон окна.
pub const BG_BASE: Color32 = Color32::from_rgb(16, 14, 12);

/// Заливка карточки. Полупрозрачная намеренно: сквозь неё виден фон, и
/// карточка выглядит стеклом, а не наклейкой. Размытия под слоем egui не
/// умеет (в CSS это `backdrop-filter`), так что «стекло» здесь — только
/// прозрачность и общий градиент под ней.
pub const CARD_FILL: Color32 = Color32::from_rgba_premultiplied(19, 18, 18, 19);

/// Заливка вложенной карточки: строка очереди, строка истории, поле графика.
/// Светлее основной ровно настолько, чтобы отделиться от неё, не превращаясь
/// во вторую рамку.
pub const CARD_INNER: Color32 = Color32::from_rgba_premultiplied(14, 13, 13, 14);

/// Поле ввода и жёлоб прогресс-бара — «утоплены» глубже карточки.
/// Тёмная полупрозрачная заливка, как в макете: на любой карточке она даёт
/// одинаковое ощущение углубления.
pub const INPUT_FILL: Color32 = Color32::from_rgba_premultiplied(7, 6, 5, 140);

/// Жёлоб прогресс-бара. Тот же тон, что у поля ввода: и то и другое —
/// углубление в карточке.
pub const PROGRESS_TRACK: Color32 = INPUT_FILL;

/// Заливка модального окна. Сплошная, а не стеклянная: модалка лежит поверх
/// затемнения, и просвечивать сквозь неё нечему.
pub const MODAL_FILL: Color32 = Color32::from_rgb(42, 37, 33);

/// Затемнение под модальным окном. Полупрозрачное, а не сплошное: главное
/// окно должно просвечивать, иначе модалка выглядит отдельным приложением,
/// а не слоем поверх Savio.
pub const MODAL_BACKDROP: Color32 = Color32::from_black_alpha(190);

/// Заливка шапки и подвала. Едва заметная плёнка поверх фона: полосы должны
/// отделяться от содержимого, но не выглядеть отдельными панелями.
pub const BG_BAR: Color32 = Color32::from_rgba_premultiplied(10, 10, 9, 10);

// Границы.
/// Декоративная линия: кромка карточки, разделитель под шапкой, подсветка
/// верхнего края стекла. Намеренно почти незаметна — не годится как
/// единственный признак элемента, и порог 3:1 к ней не применяется.
pub const BORDER_SUBTLE: Color32 = Color32::from_rgba_premultiplied(30, 30, 29, 31);

/// Граница элементов управления: дорожка переключателя, вторичная кнопка,
/// поле ввода. Сплошная, а не прозрачная, и это не придирка: у прозрачной
/// границы контраст меняется вместе с фоном под ней, и на светлом краю
/// градиента она перестаёт проходить порог. 3.23:1 на вложенной карточке,
/// 3.78:1 на обычной, 4.71:1 на фоне — порог 3:1 проходит везде.
pub const BORDER_STRONG: Color32 = Color32::from_rgb(158, 147, 133);

/// Граница при наведении: заметно ярче обычной, чтобы отклик читался
/// и без изменения заливки.
pub const BORDER_HOVER: Color32 = Color32::from_rgb(186, 176, 160);

// Текст.
/// Основной текст. Не чистый белый: на тёплом тёмном фоне он «звенит».
/// Минимум 8.89:1 — порог 4.5:1 проходит с запасом.
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(249, 244, 237);

/// Подписи, значения в таблицах, строка прогресса. Минимум 6.56:1.
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(220, 211, 196);

/// Оговорки, журнал, подсказка в пустом поле, прочерк «нет данных».
/// Минимум 4.54:1 — порог 4.5:1 проходит, но без запаса, поэтому светлее
/// макетного `#a19786` (тот давал 3.54:1 на вложенной карточке).
pub const TEXT_MUTED: Color32 = Color32::from_rgb(186, 176, 160);

/// Текст на оранжевой кнопке. Светлый здесь дал бы 1.9:1 — нечитаемо.
/// Тёмно-коричневая подпись даёт 6.93:1.
pub const TEXT_ON_ACCENT: Color32 = Color32::from_rgb(64, 35, 16);

// Акцент.
/// Главный цвет: тёплый оранжевый. Минимум 4.71:1 как текст.
pub const ACCENT: Color32 = Color32::from_rgb(246, 160, 107);
/// Наведение — светлее.
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(255, 198, 165);
/// Нажатие — темнее. С `TEXT_ON_ACCENT` даёт 5.36:1.
pub const ACCENT_ACTIVE: Color32 = Color32::from_rgb(224, 139, 87);
/// Приглушённый акцент выключенной кнопки: заметно тусклее активного,
/// но не выглядит поломкой. С `TEXT_ON_ACCENT` даёт 4.94:1 — выключенная
/// кнопка обязана оставаться читаемой.
pub const ACCENT_DISABLED: Color32 = Color32::from_rgb(196, 139, 104);
/// Подсветка выделенного текста в поле ввода. Тёмная, чтобы сам текст
/// поверх неё оставался читаемым.
pub const ACCENT_SELECTION: Color32 = Color32::from_rgb(92, 55, 30);
/// Мягкая акцентная подложка: выбранный пункт списка, плашка «идёт сейчас».
pub const ACCENT_SOFT: Color32 = Color32::from_rgba_premultiplied(34, 22, 15, 36);

// Состояния.
/// Успех и «в порядке»: приглушённый шалфейный. Минимум 4.94:1.
pub const STATE_SUCCESS: Color32 = Color32::from_rgb(174, 191, 146);
/// Ошибка: тёплый красный. Минимум 4.55:1 — светлее макетного `#f0897c`,
/// который на вложенной карточке давал 3.97:1.
pub const STATE_ERROR: Color32 = Color32::from_rgb(243, 154, 143);
/// Предупреждение: медовый. Минимум 5.36:1.
pub const STATE_WARNING: Color32 = Color32::from_rgb(232, 185, 106);
/// Мягкая зелёная подложка: плашка «есть 2160p», «Опрос идёт».
pub const SUCCESS_SOFT: Color32 = Color32::from_rgba_premultiplied(26, 28, 22, 38);

// ---------------------------------------------------------------------------
// Метрики
// ---------------------------------------------------------------------------

/// Скругление «таблетки»: кнопки, поля, дорожки переключателей, плашки.
///
/// 255, а не точное значение: тесселятор epaint сам обрезает скругление до
/// половины меньшей стороны (`clamp_corner_radius`), так что любое число
/// больше половины высоты даёт ровно полукруглые торцы — при любой высоте.
pub const RADIUS_PILL: u8 = 255;
/// Скругление большой карточки.
pub const RADIUS_CARD: u8 = 28;
/// Скругление вложенной карточки и поля графика.
pub const RADIUS_INNER: u8 = 16;
/// Скругление коробки флажка. Отдельное значение нужно из-за размера: коробка
/// у флажка 16 точек в поперечнике, у чипа — 14, и общее скругление темы
/// превратило бы обе в кружок, то есть в радиокнопку — элемент с другим
/// смыслом («одно из»), хотя галочки независимы. Проверено глазами: при 5
/// коробка чипа уже читалась кружком, поэтому здесь 3, а не «на глазок
/// поменьше».
pub const RADIUS_TINY: u8 = 3;

/// Высота главной кнопки («Скачать», «Удалить»).
pub const CTA_HEIGHT: f32 = 42.0;
/// Высота вторичной кнопки и выпадающего списка.
pub const CONTROL_HEIGHT: f32 = 34.0;
/// Высота главного поля ввода — ссылки и пути к файлу. Выше обычного:
/// это первое, к чему тянется рука на экране.
pub const FIELD_HEIGHT: f32 = 42.0;
/// Высота сегмента в дорожке переключателя.
pub const SEGMENT_HEIGHT: f32 = 30.0;
/// Ширина правой колонки экрана загрузки.
pub const RAIL_WIDTH: f32 = 340.0;
/// Ниже этой ширины правая колонка не помещается и уходит под главную.
///
/// Число не с потолка: колонке нужны свои `RAIL_WIDTH`, главной карточке —
/// не меньше 360 точек (иначе переключатель качества из шести ступеней
/// перестаёт помещаться в строку), плюс поля и зазор.
pub const TWO_COLUMN_MIN: f32 = RAIL_WIDTH + 360.0 + 60.0;

// ---------------------------------------------------------------------------
// Шрифты
//
// Свои, а не те, что кладёт eframe, — и подобраны они парами, потому что
// кириллицы нет ни в Caprasimo, ни в Figtree: обе гарнитуры латинские.
// egui подбирает шрифт **на каждый знак отдельно**, идя по списку семейства
// сверху вниз, так что пара «латинский + кириллический» работает сама собой:
// «MP4 — видео» набирается Figtree и Nunito одновременно, и это ровно то же,
// что делает браузер со списком `font-family` из макета.
//
// Начертания статические, и это важно. У переменных шрифтов Google Fonts
// умолчание оси `wght` — вовсе не 400: у Figtree оно 300, у Nunito 200.
// `ab_glyph`, которым рисует egui, вариаций не применяет и берёт мастер по
// умолчанию, так что переменный файл дал бы светлое начертание вместо
// обычного — молча, без единой ошибки сборки. Файлы в `assets/fonts` уже
// приведены к 400 и 700; чем именно — записано в `assets/fonts/README.md`.
// ---------------------------------------------------------------------------

const CAPRASIMO: &[u8] = include_bytes!("../assets/fonts/Caprasimo-Regular.ttf");
const KELLY_SLAB: &[u8] = include_bytes!("../assets/fonts/KellySlab-Regular.ttf");
const FIGTREE: &[u8] = include_bytes!("../assets/fonts/Figtree-Regular.ttf");
const FIGTREE_BOLD: &[u8] = include_bytes!("../assets/fonts/Figtree-Bold.ttf");
const NUNITO: &[u8] = include_bytes!("../assets/fonts/Nunito-Regular.ttf");
const NUNITO_BOLD: &[u8] = include_bytes!("../assets/fonts/Nunito-Bold.ttf");

/// Заголовочное семейство: плитный serif. Им набраны «Savio», названия
/// карточек и крупные числа монитора.
static DISPLAY: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name("savio-display".into()));
/// Полужирное семейство. У egui нет оси насыщенности — «жирный» это отдельное
/// семейство, а `RichText::strong()` меняет только цвет.
static BOLD: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name("savio-bold".into()));

/// Заголовочный шрифт нужного размера.
///
/// Клон `FontFamily::Name` — это клон `Arc<str>`, то есть один атомарный
/// инкремент: звать в кадре отрисовки можно.
pub fn display(size: f32) -> FontId {
    FontId::new(size, DISPLAY.clone())
}

/// Полужирный шрифт нужного размера.
pub fn bold(size: f32) -> FontId {
    FontId::new(size, BOLD.clone())
}

/// Собирает набор шрифтов: свои впереди, штатные eframe — хвостом.
///
/// Хвост обязателен, и не для полноты. Знака, которого нет ни в одной нашей
/// гарнитуре, egui ищет дальше по списку, и штатный набор eframe закрывает
/// то, что мы не покрываем, — например значки и эмодзи. Уберите хвост, и
/// вместо такого знака появится пустой прямоугольник, причём молча.
fn fonts() -> FontDefinitions {
    let mut defs = FontDefinitions::default();

    for (name, bytes) in [
        ("Caprasimo", CAPRASIMO),
        ("KellySlab", KELLY_SLAB),
        ("Figtree", FIGTREE),
        ("FigtreeBold", FIGTREE_BOLD),
        ("Nunito", NUNITO),
        ("NunitoBold", NUNITO_BOLD),
    ] {
        defs.font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }

    // Штатный хвост забираем до правки: он же достаётся всем нашим семействам.
    let fallback = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let with_fallback = |head: &[&str]| {
        let mut list: Vec<String> = head.iter().map(|name| (*name).to_owned()).collect();
        list.extend(fallback.iter().cloned());
        list
    };

    defs.families.insert(
        FontFamily::Proportional,
        with_fallback(&["Figtree", "Nunito"]),
    );
    defs.families.insert(
        DISPLAY.clone(),
        // Nunito третьим: у Kelly Slab нет ни стрелок, ни части знаков
        // препинания, а заголовок бывает и с числами.
        with_fallback(&["Caprasimo", "KellySlab", "Nunito"]),
    );
    defs.families.insert(
        BOLD.clone(),
        with_fallback(&["FigtreeBold", "NunitoBold"]),
    );

    defs
}

/// Собирает стиль и ставит его в контекст.
///
/// Вызывается один раз при создании приложения. Тема задаётся жёстко и не
/// зависит от системной светлой/тёмной схемы: приложение всегда тёмное,
/// иначе часть палитры перестала бы проходить по контрасту.
pub fn apply(ctx: &Context) {
    ctx.set_fonts(fonts());

    let mut style = Style {
        visuals: visuals(),
        ..Style::default()
    };

    style.text_styles = [
        (TextStyle::Heading, display(22.0)),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();

    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(9.0, 9.0);
    // Поля кнопки широкие: у «таблетки» подпись обязана отступать от
    // полукруглых торцов, иначе она об них упирается.
    spacing.button_padding = Vec2::new(16.0, 8.0);
    spacing.interact_size = Vec2::new(0.0, CONTROL_HEIGHT);
    spacing.icon_width = 16.0;
    spacing.icon_width_inner = 9.0;
    spacing.menu_margin = Margin::same(8);

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

    // Фон рисует [`Backdrop`], а не заливка панели: сплошным цветом три
    // тёплых пятна не передать. Панели поэтому прозрачные.
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = MODAL_FILL;
    v.faint_bg_color = CARD_INNER;
    v.extreme_bg_color = INPUT_FILL;
    // Поле ввода красим напрямую, не полагаясь на `extreme_bg_color`.
    v.text_edit_bg_color = Some(INPUT_FILL);
    v.code_bg_color = CARD_INNER;
    v.window_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    v.window_corner_radius = CornerRadius::same(RADIUS_INNER);
    v.menu_corner_radius = CornerRadius::same(RADIUS_INNER);
    v.warn_fg_color = STATE_WARNING;
    v.error_fg_color = STATE_ERROR;
    // `ui.weak()` по умолчанию берёт полупрозрачный основной цвет, из-за чего
    // контраст плавает. Задаём его явно проверенным тоном.
    v.weak_text_color = Some(TEXT_SECONDARY);

    // Фокус и выделение текста — акцентные.
    v.selection.bg_fill = ACCENT_SELECTION;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.text_cursor.stroke = Stroke::new(2.0, ACCENT);

    let pill = CornerRadius::same(RADIUS_PILL);

    // Неинтерактивное: подписи, рамки, разделители.
    let w = &mut v.widgets.noninteractive;
    w.bg_fill = CARD_FILL;
    w.weak_bg_fill = CARD_FILL;
    w.bg_stroke = Stroke::new(1.0, BORDER_SUBTLE);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = pill;

    // Покой: вторичные кнопки, поле ввода. Заливки у вторичной кнопки нет —
    // в макете это контурная «таблетка», и держится она на границе.
    let w = &mut v.widgets.inactive;
    w.bg_fill = Color32::TRANSPARENT;
    w.weak_bg_fill = Color32::TRANSPARENT;
    w.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    w.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    w.corner_radius = pill;
    w.expansion = 0.0;

    // Наведение: проступает заливка, граница светлеет, подпись — тоже.
    let w = &mut v.widgets.hovered;
    w.bg_fill = CARD_INNER;
    w.weak_bg_fill = CARD_INNER;
    w.bg_stroke = Stroke::new(1.0, BORDER_HOVER);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = pill;
    w.expansion = 1.0;

    // Нажатие.
    let w = &mut v.widgets.active;
    w.bg_fill = CARD_FILL;
    w.weak_bg_fill = CARD_FILL;
    w.bg_stroke = Stroke::new(1.0, ACCENT);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = pill;
    w.expansion = 0.0;

    // Раскрытый список / развёрнутый «Журнал».
    let w = &mut v.widgets.open;
    w.bg_fill = CARD_INNER;
    w.weak_bg_fill = CARD_INNER;
    w.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    w.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.corner_radius = pill;

    v
}

// ---------------------------------------------------------------------------
// Фон
// ---------------------------------------------------------------------------

/// Одно тёплое пятно фона: цвет, плотность в середине, положение в долях
/// окна, радиусы в точках и доля радиуса, на которой оно сходит на нет.
struct Spot {
    color: Color32,
    alpha: f32,
    at: (f32, f32),
    radius: (f32, f32),
    stop: f32,
}

/// Три пятна из макета: оранжевое сверху слева, шалфейное справа, глиняное
/// снизу. Положение — в долях окна, радиусы — в точках, как в исходном CSS:
/// поэтому в маленьком окне пятна занимают всю площадь, а в развёрнутом
/// на два монитора остаются мягкими кляксами, а не растянутой заливкой.
static SPOTS: [Spot; 3] = [
    Spot {
        color: Color32::from_rgb(246, 160, 107),
        alpha: 0.20,
        at: (0.18, -0.10),
        radius: (900.0, 620.0),
        stop: 0.70,
    },
    Spot {
        color: Color32::from_rgb(174, 191, 146),
        alpha: 0.14,
        at: (0.92, 0.12),
        radius: (760.0, 560.0),
        stop: 0.72,
    },
    Spot {
        color: Color32::from_rgb(214, 127, 72),
        alpha: 0.16,
        at: (0.60, 1.08),
        radius: (700.0, 520.0),
        stop: 0.70,
    },
];

/// Вуаль поверх пятен: из тёплого серого в почти чёрный, по диагонали.
/// Она и делает фон достаточно тёмным, чтобы светлый текст на нём читался.
const VEIL_NEAR: (Color32, f32) = (Color32::from_rgb(46, 41, 34), 0.72);
const VEIL_FAR: (Color32, f32) = (Color32::from_rgb(20, 18, 15), 0.86);
/// Угол вуали в градусах, считая как в CSS: 0 — вверх, дальше по часовой.
const VEIL_ANGLE: f32 = 158.0;

/// Сторона ячейки сетки фона в точках.
///
/// Цвет между вершинами видеокарта растягивает линейно, а пятна круглые, —
/// значит, чем крупнее ячейка, тем заметнее гранёность. 64 точки подобраны
/// так, что переходы уже неотличимы от гладких, а вершин остаётся немного:
/// в окне 1920×1080 это 31×18, то есть около шестисот вершин на весь фон.
const CELL: f32 = 64.0;

/// Фон окна: три тёплых пятна и вуаль поверх, одной треугольной сеткой.
///
/// Сетка пересобирается только при изменении размера окна, а в кадре из неё
/// берётся `Arc` — то есть один атомарный инкремент вместо сотни вершин
/// (Правило 1). Держать её в поле приложения обязательно: считать цвета
/// шестьсот раз в кадре, шестьдесят кадров в секунду, было бы ровно той
/// лишней работой, которой правило и не велит.
pub struct Backdrop {
    rect: Rect,
    mesh: Arc<Mesh>,
}

impl Default for Backdrop {
    fn default() -> Self {
        Self {
            rect: Rect::ZERO,
            mesh: Arc::new(Mesh::default()),
        }
    }
}

impl Backdrop {
    /// Рисует фон в отведённом прямоугольнике, пересобрав сетку, если окно
    /// изменило размер.
    pub fn paint(&mut self, painter: &Painter, rect: Rect) {
        if self.rect != rect {
            self.rect = rect;
            self.mesh = Arc::new(build(rect));
        }
        painter.add(Shape::Mesh(Arc::clone(&self.mesh)));
    }
}

/// Считает цвет фона в точке.
fn color_at(rect: Rect, at: Pos2) -> Color32 {
    let (w, h) = (rect.width().max(1.0), rect.height().max(1.0));
    let (x, y) = (at.x - rect.left(), at.y - rect.top());

    let mut color = [
        BG_BASE.r() as f32,
        BG_BASE.g() as f32,
        BG_BASE.b() as f32,
    ];

    let mut blend = |src: Color32, alpha: f32| {
        let alpha = alpha.clamp(0.0, 1.0);
        let src = [src.r() as f32, src.g() as f32, src.b() as f32];
        for i in 0..3 {
            color[i] = src[i] * alpha + color[i] * (1.0 - alpha);
        }
    };

    for spot in &SPOTS {
        let dx = (x - spot.at.0 * w) / spot.radius.0;
        let dy = (y - spot.at.1 * h) / spot.radius.1;
        // Плотность падает от середины к краю линейно и обрывается на `stop` —
        // ровно так ведёт себя `radial-gradient(… , transparent 70%)` в CSS.
        let t = dx.hypot(dy);
        blend(spot.color, spot.alpha * (1.0 - t / spot.stop).max(0.0));
    }

    // Вуаль: проекция точки на направление градиента, нормированная длиной
    // линии градиента. Длина считается как в CSS — сумма проекций сторон.
    let (sin, cos) = VEIL_ANGLE.to_radians().sin_cos();
    let (dx, dy) = (sin, -cos);
    let length = (w * dx).abs() + (h * dy).abs();
    let t = (((x - w / 2.0) * dx + (y - h / 2.0) * dy) / length + 0.5).clamp(0.0, 1.0);
    let veil = Color32::from_rgb(
        lerp_u8(VEIL_NEAR.0.r(), VEIL_FAR.0.r(), t),
        lerp_u8(VEIL_NEAR.0.g(), VEIL_FAR.0.g(), t),
        lerp_u8(VEIL_NEAR.0.b(), VEIL_FAR.0.b(), t),
    );
    blend(veil, VEIL_NEAR.1 + (VEIL_FAR.1 - VEIL_NEAR.1) * t);

    Color32::from_rgb(color[0] as u8, color[1] as u8, color[2] as u8)
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t) as u8
}

/// Собирает сетку фона под прямоугольник окна.
fn build(rect: Rect) -> Mesh {
    let cols = (rect.width() / CELL).ceil().max(1.0) as usize;
    let rows = (rect.height() / CELL).ceil().max(1.0) as usize;

    let mut mesh = Mesh::default();
    mesh.reserve_vertices((cols + 1) * (rows + 1));
    mesh.reserve_triangles(cols * rows * 2);

    for row in 0..=rows {
        for col in 0..=cols {
            let at = pos2(
                rect.left() + rect.width() * col as f32 / cols as f32,
                rect.top() + rect.height() * row as f32 / rows as f32,
            );
            mesh.colored_vertex(at, color_at(rect, at));
        }
    }

    let stride = (cols + 1) as u32;
    for row in 0..rows as u32 {
        for col in 0..cols as u32 {
            let top_left = row * stride + col;
            mesh.add_triangle(top_left, top_left + 1, top_left + stride);
            mesh.add_triangle(top_left + 1, top_left + stride + 1, top_left + stride);
        }
    }

    mesh
}

// ---------------------------------------------------------------------------
// Готовые оболочки
// ---------------------------------------------------------------------------

/// Тень под карточкой. Мягкая и без смещения вниз: карточка не «висит над
/// столом», а лежит слоем стекла — в макете это `0 12px 30px rgba(0,0,0,.3)`.
const CARD_SHADOW: Shadow = Shadow {
    offset: [0, 6],
    blur: 24,
    spread: 0,
    color: Color32::from_black_alpha(70),
};

/// Заготовка большой карточки без блика. Нужна там, где карточку рисует не
/// [`card`], а чужой контейнер — например модальное окно.
pub fn card_frame() -> Frame {
    Frame::new()
        .fill(CARD_FILL)
        .stroke(Stroke::new(1.0, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(18))
        .shadow(CARD_SHADOW)
}

/// Большая карточка: стекло, кромка, тень и блик по верхнему краю.
///
/// Блик — то, что отличает стекло от матовой плашки: свет ложится на верхнюю
/// грань. Рисуется поверх готовой карточки одной линией, потому что своей
/// «внутренней тени» (`inset` из CSS) у `Frame` нет.
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R> {
    let result = card_frame().show(ui, |ui| {
        // Без этого карточка сжалась бы по ширине самого длинного слова
        // внутри, и у короткого содержимого вышла бы узкая полоска посреди
        // окна. Ставится здесь, а не у каждого места вызова: разъехавшись,
        // одинаковые на вид карточки выглядели бы небрежностью.
        ui.set_width(ui.available_width());
        add_contents(ui)
    });
    gloss(ui, result.response.rect);
    result
}

/// Вложенная карточка: строка очереди, строка истории, пункт списка.
pub fn inner_frame() -> Frame {
    Frame::new()
        .fill(CARD_INNER)
        .stroke(Stroke::new(1.0, BORDER_SUBTLE))
        .corner_radius(CornerRadius::same(RADIUS_INNER))
        .inner_margin(Margin::symmetric(14, 12))
}

/// Дорожка переключателя: контурная «таблетка», внутри которой сидят сегменты.
pub fn track_frame() -> Frame {
    Frame::new()
        .stroke(Stroke::new(1.0, BORDER_STRONG))
        .corner_radius(CornerRadius::same(RADIUS_PILL))
        .inner_margin(Margin::same(2))
}

/// Блик по верхней грани: одна светлая линия внутри кромки.
fn gloss(ui: &Ui, rect: Rect) {
    let inset = RADIUS_CARD as f32 * 0.6;
    if rect.width() <= inset * 2.0 {
        return;
    }
    ui.painter().hline(
        Rangef::new(rect.left() + inset, rect.right() - inset),
        rect.top() + 0.5,
        Stroke::new(1.0, Color32::from_rgba_premultiplied(41, 40, 39, 42)),
    );
}

/// Ставит стиль полосы шапки или подвала.
pub fn bar_frame() -> Frame {
    Frame::new()
        .fill(BG_BAR)
        .inner_margin(Margin::symmetric(20, 12))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Относительная яркость по WCAG 2.1.
    fn luminance(color: Color32) -> f32 {
        let channel = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    /// Коэффициент контраста между двумя непрозрачными цветами.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Кладёт полупрозрачный цвет на непрозрачный.
    ///
    /// `Color32` хранит цвет уже умноженным на альфу, поэтому смешивание —
    /// это ровно `src + dst * (1 - a)`.
    fn over(src: Color32, dst: Color32) -> Color32 {
        let keep = 1.0 - src.a() as f32 / 255.0;
        let mix = |s: u8, d: u8| (s as f32 + d as f32 * keep) as u8;
        Color32::from_rgb(
            mix(src.r(), dst.r()),
            mix(src.g(), dst.g()),
            mix(src.b(), dst.b()),
        )
    }

    /// Самое светлое место фона по всем разумным размерам окна.
    fn brightest_backdrop() -> Color32 {
        const SIZES: [(f32, f32); 6] = [
            (520.0, 420.0),
            (720.0, 560.0),
            (1100.0, 720.0),
            (1920.0, 1080.0),
            (2560.0, 1440.0),
            (3840.0, 2160.0),
        ];
        const STEPS: usize = 40;

        let mut brightest = BG_BASE;
        for (w, h) in SIZES {
            let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h));
            for row in 0..=STEPS {
                for col in 0..=STEPS {
                    let at = pos2(
                        w * col as f32 / STEPS as f32,
                        h * row as f32 / STEPS as f32,
                    );
                    let color = color_at(rect, at);
                    if luminance(color) > luminance(brightest) {
                        brightest = color;
                    }
                }
            }
        }
        brightest
    }

    /// Проверять контраст на самом светлом месте фона мало — надо ещё, чтобы
    /// это место оставалось там, где его посчитали. Пятна фона задаются
    /// числами в `SPOTS`, и сделать их ярче — правка на одну цифру, после
    /// которой все коэффициенты в комментариях палитры станут враньём. Ни
    /// сборка, ни `clippy`, ни глаза этого не поймают: разница в пару единиц
    /// яркости не видна, а порог WCAG она перейти успевает.
    #[test]
    fn the_backdrop_stays_as_dark_as_the_palette_assumes() {
        let brightest = brightest_backdrop();
        assert!(
            brightest.r() <= 51 && brightest.g() <= 43 && brightest.b() <= 35,
            "фон посветлел до rgb({}, {}, {}) — коэффициенты в палитре \
             считались для rgb(49, 41, 33) и больше не верны",
            brightest.r(),
            brightest.g(),
            brightest.b()
        );
    }

    /// Правило 4: 4.5:1 для текста, 3:1 для границ элементов управления.
    ///
    /// Худший фон — вложенная карточка на самом светлом месте: два слоя
    /// полупрозрачного песочного поверх пятна. Именно там текст ближе всего
    /// к порогу, и именно там его проверяет этот тест.
    #[test]
    fn every_colour_passes_its_threshold_on_the_worst_background() {
        let backdrop = brightest_backdrop();
        let card = over(CARD_FILL, backdrop);
        let inner = over(CARD_INNER, card);

        let text = [
            ("TEXT_PRIMARY", TEXT_PRIMARY),
            ("TEXT_SECONDARY", TEXT_SECONDARY),
            ("TEXT_MUTED", TEXT_MUTED),
            ("ACCENT", ACCENT),
            ("ACCENT_HOVER", ACCENT_HOVER),
            ("STATE_SUCCESS", STATE_SUCCESS),
            ("STATE_ERROR", STATE_ERROR),
            ("STATE_WARNING", STATE_WARNING),
        ];
        for (name, color) in text {
            for (where_, bg) in [("фон", backdrop), ("карточка", card), ("вложенная", inner)] {
                let ratio = contrast(color, bg);
                assert!(
                    ratio >= 4.5,
                    "{name} на «{where_}» даёт {ratio:.2}:1 — порог 4.5:1 не проходит"
                );
            }
        }

        for (name, color) in [("BORDER_STRONG", BORDER_STRONG), ("BORDER_HOVER", BORDER_HOVER)] {
            for (where_, bg) in [("фон", backdrop), ("карточка", card), ("вложенная", inner)] {
                let ratio = contrast(color, bg);
                assert!(
                    ratio >= 3.0,
                    "{name} на «{where_}» даёт {ratio:.2}:1 — порог 3:1 не проходит"
                );
            }
        }
    }

    /// На акценте текст только тёмный: светлый по нему нечитаем, и это та
    /// ошибка, которую делают, не задумываясь.
    #[test]
    fn the_label_on_the_accent_is_dark_enough() {
        for (name, fill) in [
            ("ACCENT", ACCENT),
            ("ACCENT_HOVER", ACCENT_HOVER),
            ("ACCENT_ACTIVE", ACCENT_ACTIVE),
            ("ACCENT_DISABLED", ACCENT_DISABLED),
        ] {
            let ratio = contrast(TEXT_ON_ACCENT, fill);
            assert!(
                ratio >= 4.5,
                "тёмная подпись на {name} даёт {ratio:.2}:1 — порог 4.5:1 не проходит"
            );
            assert!(
                contrast(TEXT_PRIMARY, fill) < 4.5,
                "светлая подпись на {name} внезапно проходит порог — \
                 проверьте, тот ли это цвет"
            );
        }
    }

    /// Сетка фона обязана накрывать окно целиком и пересобираться только на
    /// смену размера: она в кадре не считается, а берётся готовой (Правило 1).
    #[test]
    fn the_backdrop_mesh_covers_the_window_and_is_built_once() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1100.0, 720.0));
        let mesh = build(rect);

        let cols = (1100.0f32 / CELL).ceil() as usize;
        let rows = (720.0f32 / CELL).ceil() as usize;
        assert_eq!(mesh.vertices.len(), (cols + 1) * (rows + 1));
        assert_eq!(mesh.indices.len(), cols * rows * 6);

        let left = mesh.vertices.iter().map(|v| v.pos.x).fold(f32::MAX, f32::min);
        let right = mesh.vertices.iter().map(|v| v.pos.x).fold(f32::MIN, f32::max);
        let top = mesh.vertices.iter().map(|v| v.pos.y).fold(f32::MAX, f32::min);
        let bottom = mesh.vertices.iter().map(|v| v.pos.y).fold(f32::MIN, f32::max);
        assert_eq!((left, top), (rect.left(), rect.top()));
        assert_eq!((right, bottom), (rect.right(), rect.bottom()));
    }
}
