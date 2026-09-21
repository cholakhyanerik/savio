//! Оформление: две темы на холодной нейтральной основе, бирюзовый акцент.
//!
//! Здесь только цвета, шрифты, метрики и настройка `egui::Style` — слой UI,
//! как и `app.rs`. Про yt-dlp и процессы этот модуль не знает ничего.
//!
//! Главное отличие от прежней версии — **тема здесь значение, а не набор
//! `const`**. Цвет берётся из [`Palette`], которую окно держит в состоянии и
//! передаёт вниз параметром, ровно как `Lang`. Глобаль была бы короче на
//! четыре сотни правок, но спрятала бы зависимость и сделала бы невозможным
//! то, ради чего палитра и стала значением: тест, которому нужны обе темы
//! в одном процессе, и переключение на ходу без пересборки окна.
//!
//! Стиль собирается **один раз** при старте и ещё раз при смене темы
//! ([`apply`]), а не в кадре отрисовки: `Style` содержит `BTreeMap` шрифтов,
//! и пересборка его 60 раз в секунду была бы чистой потерей времени. Сами
//! шрифты ставятся отдельно ([`install_fonts`]) и ровно однажды: `set_fonts`
//! пересобирает атлас глифов, и звать его на каждое нажатие переключателя
//! темы — это подвисание на ровном месте.

use std::sync::{Arc, LazyLock};

use eframe::egui::{
    Color32, CornerRadius, Context, FontData, FontDefinitions, FontFamily, FontId, Frame,
    InnerResponse, Margin, Mesh, Painter, Rangef, Rect, Shadow, Shape, Stroke, Style, TextStyle,
    Theme, ThemePreference, Ui, Vec2, Visuals, pos2,
};

use crate::model::Appearance;

// ---------------------------------------------------------------------------
// Палитра
//
// Каждая пара «текст на фоне» проверена по WCAG 2.1 (формула относительной
// яркости), коэффициенты указаны в комментариях к полям. Порог для основного
// текста — 4.5:1, для крупного текста и границ элементов управления — 3:1.
// Комбинации, не указанные здесь, использовать не следует: они не проверены.
//
// Считать контраст стало проще, чем в прежней тёплой теме, и это единственная
// приятная новость от смены макета. Раньше карточки были полупрозрачным
// стеклом, а под ними лежали три тёплых пятна, так что у одного и того же
// текста фон был разным в разных углах окна, и проверять приходилось самый
// светлый край диапазона. Теперь поверхности сплошные, и «худший фон» — не
// диапазон, а конечный список из семи значений: фон окна, рельс, карточка,
// вложенный блок на карточке, вложенный блок на рельсе, поле ввода и модалка.
// Их и перебирает `every_colour_passes_its_threshold_in_both_themes`.
//
// Два расхождения с макетом, оба намеренные и оба — в пользу Правила 4,
// а не вкуса.
//
// Первое: кромка элементов управления. В макете это `rgba(255,255,255,.16)`
// в тёмной теме и `rgba(26,31,31,.22)` в светлой, то есть 1.66:1 и 1.58:1
// на карточке — порог 3:1 не проходит даже близко. Ровно та же правка уже
// делалась однажды, в первой версии темы, и по той же причине; здесь граница
// снова сплошная и снова поднята до проходящего значения.
//
// Второе: `TEXT_FAINT`. Макет даёт ему 4.23:1 на вложенной карточке, и потому
// этот цвет разрешён только на четырёх поверхностях из семи (см. его поле).
// Поднимать его было нельзя: между `text_muted` и порогом остаётся меньше
// десятка единиц яркости, и «ещё более тусклый» цвет, проходящий везде, от
// `text_muted` уже неотличим — то есть роль исчезла бы, а не стала безопаснее.
// ---------------------------------------------------------------------------

/// Все цвета окна одним значением.
///
/// `Copy` намеренно: 35 полей по четыре байта — это 140 байт, дешевле
/// указателя с разыменованием, и копия в начале функции снимает все споры
/// с заимствованием `&mut self` у методов `SavioApp`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Palette {
    // Поверхности.
    /// Фон окна. Сплошной: пятен и вуали, как в прежней теме, больше нет.
    pub bg: Color32,
    /// Заливка рельса слева и полос шапки, пока они есть.
    pub rail_fill: Color32,
    /// Заливка карточки. Сплошная, а не стеклянная, — так в макете v2.
    pub card_fill: Color32,
    /// Заливка вложенного блока: строка очереди, строка истории, поле графика,
    /// нижний блок рельса. Полупрозрачная, поэтому её цвет зависит от того,
    /// на чём она лежит, — отсюда в проверке две подложки, а не одна.
    pub card_inner: Color32,
    /// Поле ввода. В светлой теме совпадает с карточкой намеренно: поле там
    /// держится на кромке, а не на заливке.
    pub input_fill: Color32,
    /// Жёлоб прогресс-бара. Отдельно от поля ввода, и это не придирка:
    /// в светлой теме поле белое, и белый жёлоб на белой карточке был бы
    /// невидим — полосу не стало бы видно, пока она не заполнится.
    pub progress_track: Color32,
    /// Заливка модального окна. Сплошная: модалка лежит поверх затемнения,
    /// и просвечивать сквозь неё нечему.
    pub modal_fill: Color32,
    /// Затемнение под модальным окном. Тёмное в обеих темах: оно гасит окно,
    /// а не подкрашивает его.
    pub modal_backdrop: Color32,

    // Границы.
    /// Декоративная линия: кромка карточки, разделитель. Намеренно почти
    /// незаметна — не годится как единственный признак элемента, и порог
    /// 3:1 к ней не применяется.
    pub border_subtle: Color32,
    /// Граница элементов управления: дорожка переключателя, вторичная кнопка,
    /// поле ввода. Сплошная, а не прозрачная, и это не придирка: у прозрачной
    /// границы контраст меняется вместе с фоном под ней. Минимум 3.15:1.
    pub border_strong: Color32,
    /// Граница при наведении: заметно ярче обычной, чтобы отклик читался
    /// и без изменения заливки. Минимум 4.5:1.
    pub border_hover: Color32,

    // Текст.
    /// Заголовки и значения. Минимум 8.9:1.
    pub text_primary: Color32,
    /// Подписи, значения в таблицах, строка прогресса. Минимум 6.1:1.
    pub text_secondary: Color32,
    /// Оговорки, журнал, подсказка в пустом поле, прочерк «нет данных».
    /// Минимум 4.78:1.
    pub text_muted: Color32,
    /// Заголовки групп в рельсе и номер версии.
    ///
    /// Разрешён **не везде**: на фоне окна, на рельсе, на карточке и в
    /// модалке (минимум 4.65:1), но не во вложенном блоке — там он даёт
    /// 4.23:1. Это ограничение держит проверка, у которой для каждого цвета
    /// свой список поверхностей, а не общий.
    pub text_faint: Color32,
    /// Подпись на акцентной заливке главной кнопки. Минимум 6.1:1.
    pub text_on_accent: Color32,
    /// Подпись на заливке выключенной главной кнопки. Отдельно от предыдущей,
    /// потому что в светлой теме они разные: включённая кнопка тёмная и
    /// подписана белым, выключенная — пастельная и подписана тёмным.
    /// Минимум 5.39:1.
    pub text_on_accent_disabled: Color32,

    // Акцент.
    /// Главный цвет. Минимум 4.51:1 как текст.
    pub accent: Color32,
    /// Наведение.
    pub accent_hover: Color32,
    /// Нажатие.
    pub accent_active: Color32,
    /// Приглушённый акцент выключенной кнопки: заметно тусклее активного,
    /// но не выглядит поломкой. Выключенная кнопка обязана оставаться
    /// читаемой (`disabled_alpha` там выставлен в единицу).
    pub accent_disabled: Color32,
    /// Подсветка выделенного текста в поле ввода.
    pub accent_selection: Color32,
    /// Мягкая акцентная подложка: выбранный пункт рельса, подсвеченная строка
    /// очереди, плашка «идёт сейчас».
    ///
    /// На ней `text_muted` брать нельзя: заливка поднимает фон, и приглушённый
    /// текст падает до 4.03:1. Разрешены `accent_text`, `text_secondary`
    /// и `text_primary` — минимум 5.03:1.
    pub accent_soft: Color32,
    /// Текст на акцентной подложке. Минимум 5.03:1.
    pub accent_text: Color32,

    // Состояния.
    /// Успех и «в порядке». Минимум 6.16:1.
    pub state_success: Color32,
    /// Ошибка. Минимум 4.6:1.
    pub state_error: Color32,
    /// Предупреждение. Минимум 5.76:1.
    pub state_warning: Color32,
    /// Мягкая зелёная подложка: плашка «есть 2160p», «Опрос идёт».
    pub success_soft: Color32,

    // Небо: значки погоды.
    //
    // Значок — графика, и порог для него 3:1. Почти все цвета неба берутся
    // из уже проверенной части палитры и проходят и текстовый порог; свой
    // здесь один — голубой воды, и он заодно подписывает вероятность осадков
    // («40%»), поэтому проверяется по порогу текста вместе с остальными.
    /// Дождь, морось и вероятность осадков. Единственный холодный цвет
    /// прежней палитры остался холодным и в новой. Минимум 4.53:1.
    pub sky_water: Color32,

    // Стекло.
    /// Постоянный свет на верхней грани карточки.
    pub gloss_line: Color32,
    /// Яркость бегущего пятна на той же грани (приём 06).
    ///
    /// В светлой теме это не белый, а акцент: белая полоса по белой карточке
    /// не видна вовсе, и приём исчез бы молча — ровно тот случай, которого
    /// не ловят ни сборка, ни тесты.
    pub gloss_spark: Color32,
    /// Цвет тени под карточкой.
    pub shadow: Color32,
}

impl Palette {
    /// Палитра выбранной темы.
    pub fn of(appearance: Appearance) -> Palette {
        match appearance {
            Appearance::Dark => Palette::dark(),
            Appearance::Light => Palette::light(),
        }
    }

    /// Тёмная тема.
    pub fn dark() -> Palette {
        Palette {
            bg: Color32::from_rgb(0x12, 0x15, 0x14),
            rail_fill: Color32::from_rgb(0x17, 0x1B, 0x1B),
            card_fill: Color32::from_rgb(0x1A, 0x1F, 0x1F),
            card_inner: Color32::from_rgba_unmultiplied(255, 255, 255, 10),
            input_fill: Color32::from_rgb(0x0E, 0x12, 0x12),
            progress_track: Color32::from_rgb(0x0E, 0x12, 0x12),
            // Чуть темнее макетного `#1E2322`: на том приглушённая подпись
            // давала 4.45:1 — на пять сотых ниже порога, и ровно это поймала
            // проверка палитры. Глазу разница в три единицы яркости не видна,
            // порог WCAG она переходит.
            modal_fill: Color32::from_rgb(0x1B, 0x20, 0x20),
            modal_backdrop: Color32::from_black_alpha(190),

            border_subtle: Color32::from_rgba_unmultiplied(255, 255, 255, 18),
            border_strong: Color32::from_rgb(0x6F, 0x76, 0x76),
            border_hover: Color32::from_rgb(0x8D, 0x93, 0x93),

            text_primary: Color32::from_rgb(0xF2, 0xF5, 0xF4),
            text_secondary: Color32::from_rgb(0xC2, 0xCC, 0xCC),
            text_muted: Color32::from_rgb(0x8B, 0x98, 0x99),
            text_faint: Color32::from_rgb(0x7C, 0x8A, 0x8B),
            text_on_accent: Color32::from_rgb(0x0E, 0x2A, 0x2C),
            text_on_accent_disabled: Color32::from_rgb(0x0E, 0x2A, 0x2C),

            accent: Color32::from_rgb(0x7F, 0xC3, 0xC9),
            accent_hover: Color32::from_rgb(0x9B, 0xD2, 0xD8),
            accent_active: Color32::from_rgb(0x6B, 0xB0, 0xB6),
            accent_disabled: Color32::from_rgb(0x6F, 0xA3, 0xA7),
            accent_selection: Color32::from_rgb(0x1E, 0x4A, 0x4E),
            accent_soft: Color32::from_rgba_unmultiplied(0x7F, 0xC3, 0xC9, 41),
            accent_text: Color32::from_rgb(0x9F, 0xD6, 0xDB),

            state_success: Color32::from_rgb(0xAE, 0xBF, 0x92),
            state_error: Color32::from_rgb(0xD0, 0x76, 0x6C),
            state_warning: Color32::from_rgb(0xE8, 0xB9, 0x6A),
            success_soft: Color32::from_rgba_unmultiplied(0xAE, 0xBF, 0x92, 36),

            sky_water: Color32::from_rgb(0x92, 0xBA, 0xE0),

            gloss_line: Color32::from_rgba_unmultiplied(255, 255, 255, 13),
            gloss_spark: Color32::from_rgba_unmultiplied(255, 255, 255, 150),
            shadow: Color32::from_black_alpha(70),
        }
    }

    /// Светлая тема.
    ///
    /// Акцент здесь заметно темнее, чем в тёмной теме, и это намеренно:
    /// бирюзовый `#7FC3C9` на светлом фоне даёт около 1.9:1 и не читается
    /// ни подписью, ни кромкой.
    pub fn light() -> Palette {
        Palette {
            bg: Color32::from_rgb(0xF4, 0xF2, 0xEC),
            rail_fill: Color32::from_rgb(0xEA, 0xE7, 0xDF),
            card_fill: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            card_inner: Color32::from_rgba_unmultiplied(0x1A, 0x1F, 0x1F, 13),
            input_fill: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            progress_track: Color32::from_rgb(0xE3, 0xE0, 0xD8),
            modal_fill: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            modal_backdrop: Color32::from_black_alpha(110),

            border_subtle: Color32::from_rgba_unmultiplied(0x1A, 0x1F, 0x1F, 26),
            border_strong: Color32::from_rgb(0x7A, 0x7A, 0x76),
            border_hover: Color32::from_rgb(0x61, 0x62, 0x5E),

            text_primary: Color32::from_rgb(0x17, 0x1B, 0x1B),
            text_secondary: Color32::from_rgb(0x43, 0x50, 0x4F),
            text_muted: Color32::from_rgb(0x56, 0x60, 0x5F),
            text_faint: Color32::from_rgb(0x5E, 0x68, 0x67),
            text_on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            text_on_accent_disabled: Color32::from_rgb(0x1B, 0x3A, 0x3C),

            accent: Color32::from_rgb(0x26, 0x6B, 0x72),
            accent_hover: Color32::from_rgb(0x1F, 0x5A, 0x60),
            accent_active: Color32::from_rgb(0x17, 0x47, 0x4C),
            accent_disabled: Color32::from_rgb(0x9D, 0xBF, 0xC2),
            accent_selection: Color32::from_rgb(0xBB, 0xD9, 0xDC),
            accent_soft: Color32::from_rgba_unmultiplied(0x26, 0x6B, 0x72, 31),
            accent_text: Color32::from_rgb(0x1F, 0x5F, 0x66),

            state_success: Color32::from_rgb(0x46, 0x52, 0x31),
            state_error: Color32::from_rgb(0xA1, 0x42, 0x37),
            state_warning: Color32::from_rgb(0x6F, 0x4B, 0x08),
            success_soft: Color32::from_rgba_unmultiplied(0x46, 0x52, 0x31, 31),

            sky_water: Color32::from_rgb(0x2C, 0x66, 0x90),

            gloss_line: Color32::from_rgba_unmultiplied(0x1A, 0x1F, 0x1F, 15),
            gloss_spark: Color32::from_rgba_unmultiplied(0x26, 0x6B, 0x72, 90),
            shadow: Color32::from_black_alpha(28),
        }
    }

    /// Солнце: ясное небо на значке погоды.
    pub fn sky_sun(self) -> Color32 {
        self.accent
    }

    /// Луна. Ночь на значке обязана отличаться от дня не только формой:
    /// на мелком столбике почасового прогноза форму не разглядеть.
    pub fn sky_moon(self) -> Color32 {
        self.accent_hover
    }

    /// Ближнее облако.
    pub fn sky_cloud(self) -> Color32 {
        self.text_secondary
    }

    /// Дальнее облако пасмурного неба — тусклее ближнего, иначе два облака
    /// сливаются в одно пятно.
    pub fn sky_cloud_far(self) -> Color32 {
        self.text_muted
    }

    /// Снег — самый контрастный цвет текста.
    pub fn sky_snow(self) -> Color32 {
        self.text_primary
    }

    /// Молния — медовый предупреждения.
    pub fn sky_bolt(self) -> Color32 {
        self.state_warning
    }
}

// QR-код на экране «Телефон».
//
// От темы не зависит, и это не недосмотр. Тёмные модули на светлом поле, а не
// наоборот: камеры телефонов код в обратных цветах читают не все — по
// стандарту QR тёмное это модуль, и часть сканеров светлые модули на тёмном
// просто не находит. Светлое поле вокруг кода (тихая зона) обязательно по той
// же причине. Возьми эти два цвета из палитры — и в светлой теме код
// перевернулся бы: `bg` там светлее `text_primary`. Пара даёт 17.60:1:
// сканеру нужен не порог WCAG, а как можно больший перепад.
/// Модуль кода.
pub const QR_DARK: Color32 = Color32::from_rgb(16, 14, 12);
/// Поле кода и тихая зона вокруг.
pub const QR_LIGHT: Color32 = Color32::from_rgb(249, 244, 237);

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
/// Поля по бокам подписи сегмента — против штатных 16 у кнопки.
///
/// Урезаны намеренно: в дорожке качества шесть ступеней, и в окне шириной
/// 520 со штатными полями подписи не влезают в свою долю, раздвигают кнопки
/// и уносят дорожку за кромку. Живёт здесь, а не в `segment_button`, чтобы
/// проверка ширины (`the_segment_tracks_fit_the_smallest_window`) считала
/// по тому же числу: своя копия в тесте разъехалась бы с раскладкой при
/// первой же правке и молча перестала бы что-либо ловить.
pub const SEGMENT_PADDING: f32 = 10.0;
/// Промежуток между сегментами дорожки.
pub const SEGMENT_GAP: f32 = 2.0;
/// Ширина рельса разделов слева.
///
/// Не путать с [`RAIL_WIDTH`]: тот — правая колонка экрана загрузки, и это
/// два разных рельса. Имена разошлись не от небрежности, а от истории: правая
/// колонка называлась рельсом задолго до того, как слева появился настоящий.
pub const NAV_WIDTH: f32 = 216.0;
/// Ширина свёрнутого рельса: акцентная точка, значки разделов и одна кнопка.
pub const NAV_NARROW: f32 = 56.0;
/// Высота пункта рельса.
pub const NAV_ITEM_HEIGHT: f32 = 38.0;
/// Скругление пункта рельса и его нижнего блока.
pub const RADIUS_NAV: u8 = 10;
/// Отступ подписи пункта от левого края: значок и промежуток за ним.
pub const NAV_ICON_COLUMN: f32 = 40.0;
/// Поля рельса по бокам — столько же, сколько у `rail_frame`.
pub const NAV_PADDING: f32 = 12.0;
/// Поля нижнего блока рельса изнутри — у `rail_block_frame`.
pub const NAV_BLOCK_MARGIN: f32 = 12.0;
/// Зазор между последним пунктом рельса и его нижним блоком.
pub const NAV_BLOCK_GAP: f32 = 12.0;
/// Сколько места остаётся подписи пункта рельса.
///
/// Считается, а не выбирается, и живёт рядом с раскладкой: проверка ширины
/// на трёх языках считает по этому же числу, а своя копия в тесте разъехалась
/// бы при первой же правке и молча перестала бы что-либо ловить.
pub const NAV_LABEL_WIDTH: f32 = NAV_WIDTH - NAV_PADDING * 2.0 - NAV_ICON_COLUMN;
/// Сколько места остаётся содержимому нижнего блока рельса: ширина рельса
/// минус его поля и минус поля самого блока.
pub const NAV_BLOCK_WIDTH: f32 = NAV_WIDTH - NAV_PADDING * 2.0 - NAV_BLOCK_MARGIN * 2.0;
/// Поля вокруг содержимого окна, по горизонтали.
///
/// Живёт здесь, а не у `CentralPanel`, потому что по нему считается порог
/// разворота рельса: своя копия числа разъехалась бы с раскладкой при первой
/// же правке (та же беда, что у `SEGMENT_PADDING`).
pub const CONTENT_MARGIN: f32 = 20.0;

/// Ширина правой колонки экрана загрузки.
pub const RAIL_WIDTH: f32 = 340.0;
/// Ниже этой ширины правая колонка не помещается и уходит под главную.
///
/// Число не с потолка: колонке нужны свои `RAIL_WIDTH`, главной карточке —
/// не меньше 360 точек (иначе переключатель качества из шести ступеней
/// перестаёт помещаться в строку), плюс поля и зазор.
pub const TWO_COLUMN_MIN: f32 = RAIL_WIDTH + 360.0 + 60.0;

/// Ниже этой ширины **окна** рельс разделов сворачивается в полосу значков.
///
/// Число считается, а не выбирается: разворот рельса отнимает у содержимого
/// `NAV_WIDTH - NAV_NARROW`, то есть 160 точек, и если позволить ему
/// разворачиваться раньше, содержимое ровно на этом теряет вторую колонку.
/// Расширение окна на одну точку схлопывало бы раскладку — беда тем более
/// обидная, что выглядит она как случайность.
///
/// В макете здесь стояло 900, и при нём разрыв как раз и получался: окно 899
/// давало содержимому 803 точки и две колонки, окно 900 — 644 и одну.
/// Поэтому порог тут свой и выведен из `TWO_COLUMN_MIN`, а не переписан
/// из документа. Держит это `the_content_never_loses_room_as_the_window_grows`.
pub const NAV_WIDE_MIN: f32 = NAV_WIDTH + TWO_COLUMN_MIN + CONTENT_MARGIN * 2.0;

/// Ниже этой высоты **окна** нижний блок рельса уходит во всплывающее меню,
/// а подписи пунктов остаются на месте.
///
/// Развёрнутый рельс не прокручивается: пункты идут сверху, нижний блок
/// прижат к низу, и в низком окне блок ложился прямо на пункты — подписи
/// «Погода» и «Обновить движок» рисовались одна поверх другой.
///
/// Число снято замером рельса на трёх языках, а не сложено из метрик:
/// высота блока зависит от перевода — по-армянски кнопки обновления
/// переносятся на вторую строку. Замер 2026-09-21: по-русски и по-английски
/// нужно 771 точка, по-армянски 807. Правка блока или перевода сдвигает
/// его, и держит это `the_wide_rail_does_not_cover_its_own_items`:
/// проверка рисует рельс ровно такой высоты и ищет наложение.
pub const NAV_WIDE_MIN_HEIGHT: f32 = 810.0;

/// Ниже этой высоты **окна** рельс сворачивается в полосу значков, какой
/// бы ни была ширина: подписанным пунктам с кнопкой «Настройки» под ними
/// уже не хватает места. Замер того же дня — 458 точек на всех трёх
/// языках; проверяется так же, как [`NAV_WIDE_MIN_HEIGHT`].
pub const NAV_LABELS_MIN_HEIGHT: f32 = 460.0;

// ---------------------------------------------------------------------------
// Шрифты
//
// Свои, а не те, что кладёт eframe, — и подобраны они тройками, потому что
// кириллицы нет ни в Caprasimo, ни в Figtree, а армянского нет ни в одной из
// четырёх латино-кириллических гарнитур. egui подбирает шрифт **на каждый знак
// отдельно**, идя по списку семейства сверху вниз, так что тройка
// «латинский + кириллический + армянский» работает сама собой: «MP4 — видео»
// набирается Figtree и Nunito одновременно, и это ровно то же, что делает
// браузер со списком `font-family` из макета.
//
// Армянский без своего файла — это ряды пустых прямоугольников, и ничем, кроме
// глаз, этого не увидеть. Штатный хвост eframe здесь не спасает: у Ubuntu-Light
// армянских знаков ноль, а Hack (их там 86) стоит только в `Monospace`, а
// запасной глиф egui подбирает **строго внутри своего семейства**. Числа сняты
// разбором таблицы `cmap` у самих файлов: из девяти вшитых гарнитур армянский
// есть ровно у трёх, добавленных ниже, — по 91 знаку у каждой.
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
const ARMENIAN: &[u8] = include_bytes!("../assets/fonts/NotoSansArmenian-Regular.ttf");
const ARMENIAN_BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSansArmenian-Bold.ttf");
/// Заголовочный армянский. Serif, а не sans, ровно затем, чтобы заголовок на
/// армянском оставался заголовком: латиницу и кириллицу там набирают плитные
/// Caprasimo и Kelly Slab, и сансерифная вставка посреди них читалась бы
/// обычным текстом, набранным крупнее.
const ARMENIAN_DISPLAY: &[u8] = include_bytes!("../assets/fonts/NotoSerifArmenian-Regular.ttf");

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
        ("Armenian", ARMENIAN),
        ("ArmenianBold", ARMENIAN_BOLD),
        ("ArmenianDisplay", ARMENIAN_DISPLAY),
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
        with_fallback(&["Figtree", "Nunito", "Armenian"]),
    );
    defs.families.insert(
        DISPLAY.clone(),
        // Nunito третьим: у Kelly Slab нет ни стрелок, ни части знаков
        // препинания, а заголовок бывает и с числами.
        with_fallback(&["Caprasimo", "KellySlab", "Nunito", "ArmenianDisplay"]),
    );
    defs.families.insert(
        BOLD.clone(),
        with_fallback(&["FigtreeBold", "NunitoBold", "ArmenianBold"]),
    );

    defs
}

/// Ставит шрифты в контекст. Зовётся **один раз**, при создании приложения.
///
/// Отдельно от [`apply`] намеренно: `set_fonts` пересобирает атлас глифов, и
/// если звать его при каждой смене темы, переключатель будет подвешивать окно
/// на десятки миллисекунд там, где меняются одни только цвета.
pub fn install_fonts(ctx: &Context) {
    ctx.set_fonts(fonts());
}

/// Собирает стиль выбранной темы и ставит его в контекст.
///
/// Зовётся при старте и при каждой смене темы — но не в кадре: `Style`
/// содержит `BTreeMap` шрифтов, и пересобирать его 60 раз в секунду незачем.
pub fn apply(ctx: &Context, palette: Palette) {
    let mut style = Style {
        visuals: visuals(palette),
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
    // и на светлой ОС тёмная тема Savio открылась бы со светлым стилем. Выбор
    // темы принадлежит человеку и лежит в настройках, а не в системе.
    ctx.set_theme(if palette.is_dark() {
        ThemePreference::Dark
    } else {
        ThemePreference::Light
    });

    // Стиль кладём в оба слота: если egui всё же переключит тему (например,
    // при смене системной схемы на ходу), внешний вид не поедет.
    let style = Arc::new(style);
    ctx.set_style_of(Theme::Dark, Arc::clone(&style));
    ctx.set_style_of(Theme::Light, style);
}

impl Palette {
    /// Тёмная ли это тема. Спрашивается по яркости фона, а не по полю
    /// «какую выбрали»: палитра обязана оставаться значением, из которого
    /// всё выводится, иначе подправленный вручную набор цветов и его признак
    /// разъедутся.
    fn is_dark(self) -> bool {
        self.text_primary.r() > self.bg.r()
    }
}

fn visuals(p: Palette) -> Visuals {
    let mut v = if p.is_dark() {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.panel_fill = p.bg;
    v.window_fill = p.modal_fill;
    v.faint_bg_color = p.card_inner;
    v.extreme_bg_color = p.input_fill;
    // Поле ввода красим напрямую, не полагаясь на `extreme_bg_color`.
    v.text_edit_bg_color = Some(p.input_fill);
    v.code_bg_color = p.card_inner;
    v.window_stroke = Stroke::new(1.0, p.border_subtle);
    v.window_corner_radius = CornerRadius::same(RADIUS_INNER);
    v.menu_corner_radius = CornerRadius::same(RADIUS_INNER);
    v.warn_fg_color = p.state_warning;
    v.error_fg_color = p.state_error;
    // `ui.weak()` по умолчанию берёт полупрозрачный основной цвет, из-за чего
    // контраст плавает. Задаём его явно проверенным тоном.
    v.weak_text_color = Some(p.text_secondary);

    // Фокус и выделение текста — акцентные.
    v.selection.bg_fill = p.accent_selection;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);

    let pill = CornerRadius::same(RADIUS_PILL);

    // Неинтерактивное: подписи, рамки, разделители.
    let w = &mut v.widgets.noninteractive;
    w.bg_fill = p.card_fill;
    w.weak_bg_fill = p.card_fill;
    w.bg_stroke = Stroke::new(1.0, p.border_subtle);
    w.fg_stroke = Stroke::new(1.0, p.text_primary);
    w.corner_radius = pill;

    // Покой: вторичные кнопки, поле ввода. Заливки у вторичной кнопки нет —
    // в макете это контурная «таблетка», и держится она на границе.
    let w = &mut v.widgets.inactive;
    w.bg_fill = Color32::TRANSPARENT;
    w.weak_bg_fill = Color32::TRANSPARENT;
    w.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.fg_stroke = Stroke::new(1.0, p.text_secondary);
    w.corner_radius = pill;
    w.expansion = 0.0;

    // Наведение: проступает заливка, граница светлеет, подпись — тоже.
    let w = &mut v.widgets.hovered;
    w.bg_fill = p.card_inner;
    w.weak_bg_fill = p.card_inner;
    w.bg_stroke = Stroke::new(1.0, p.border_hover);
    w.fg_stroke = Stroke::new(1.0, p.text_primary);
    w.corner_radius = pill;
    w.expansion = 1.0;

    // Нажатие.
    let w = &mut v.widgets.active;
    w.bg_fill = p.card_fill;
    w.weak_bg_fill = p.card_fill;
    w.bg_stroke = Stroke::new(1.0, p.accent);
    w.fg_stroke = Stroke::new(1.0, p.text_primary);
    w.corner_radius = pill;
    w.expansion = 0.0;

    // Раскрытый список / развёрнутый «Журнал».
    let w = &mut v.widgets.open;
    w.bg_fill = p.card_inner;
    w.weak_bg_fill = p.card_inner;
    w.bg_stroke = Stroke::new(1.0, p.border_strong);
    w.fg_stroke = Stroke::new(1.0, p.text_primary);
    w.corner_radius = pill;

    v
}

// ---------------------------------------------------------------------------
// Готовые оболочки
// ---------------------------------------------------------------------------

/// Тень под карточкой. Мягкая и без смещения вниз: карточка не «висит над
/// столом», а лежит слоем — в макете это `0 12px 30px rgba(0,0,0,.3)`.
fn card_shadow(p: Palette) -> Shadow {
    Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: p.shadow,
    }
}

/// Заготовка большой карточки без блика. Нужна там, где карточку рисует не
/// [`card`], а чужой контейнер — например модальное окно.
pub fn card_frame(p: Palette) -> Frame {
    Frame::new()
        .fill(p.card_fill)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(18))
        .shadow(card_shadow(p))
}

/// Как появляющаяся карточка выглядит на этом кадре.
///
/// Значение, а не расчёт: длительности и кривые живут в `motion`, а этот
/// слой знает только про оболочки карточек и обязан оставаться не знающим
/// ни про загрузку, ни про `Event`.
#[derive(Clone, Copy)]
pub struct Appear {
    /// Насколько карточка ещё ниже своего места, в точках.
    pub offset: f32,
    /// Насколько она уже проявилась: 0 — не видно, 1 — как обычно.
    pub opacity: f32,
    /// Где сейчас бегущий по кромке блик, долей пути (приём 06).
    /// Единица и больше — блика нет.
    pub sweep: f32,
}

/// Оболочка появляющегося содержимого: подъезжает снизу и проступает.
///
/// `None` означает «уже на месте» и не стоит ничего — ни лишнего `Ui`,
/// ни лишнего отступа.
///
/// Прозрачность **умножается**, а не выставляется: содержимое вкладки может
/// появляться внутри уже приглушённого слоя (модалка, выключенная группа),
/// и `set_opacity` там вернул бы ему полную яркость.
pub fn rising<R>(ui: &mut Ui, appear: Option<Appear>, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let Some(appear) = appear else {
        return add_contents(ui);
    };
    ui.add_space(appear.offset);
    ui.scope(|ui| {
        ui.multiply_opacity(appear.opacity);
        add_contents(ui)
    })
    .inner
}

/// Большая карточка: заливка, кромка, тень и блик по верхнему краю.
///
/// Блик — одна светлая линия на верхней грани, поверх готовой карточки:
/// своей «внутренней тени» (`inset` из CSS) у `Frame` нет.
pub fn card<R>(
    ui: &mut Ui,
    p: Palette,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    card_with_sweep(ui, p, 1.0, add_contents)
}

/// Та же карточка, но с блеском, бегущим по кромке.
fn card_with_sweep<R>(
    ui: &mut Ui,
    p: Palette,
    sweep: f32,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let result = card_frame(p).show(ui, |ui| {
        // Без этого карточка сжалась бы по ширине самого длинного слова
        // внутри, и у короткого содержимого вышла бы узкая полоска посреди
        // окна. Ставится здесь, а не у каждого места вызова: разъехавшись,
        // одинаковые на вид карточки выглядели бы небрежностью.
        ui.set_width(ui.available_width());
        add_contents(ui)
    });
    gloss(ui, p, result.response.rect, sweep);
    result
}

/// Та же карточка, но всплывающая при появлении.
///
/// `None` в `appear` означает «уже на месте», и тогда это ровно [`card`] —
/// ни лишнего `Ui`, ни лишнего отступа. Такова и договорённость: карточка,
/// которой не сказали, как появляться, ведёт себя как раньше.
pub fn card_rising<R>(
    ui: &mut Ui,
    p: Palette,
    appear: Option<Appear>,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    let sweep = appear.map_or(1.0, |appear| appear.sweep);
    rising(ui, appear, |ui| card_with_sweep(ui, p, sweep, add_contents))
}

/// Вложенная карточка: строка очереди, строка истории, пункт списка.
pub fn inner_frame(p: Palette) -> Frame {
    Frame::new()
        .fill(p.card_inner)
        .stroke(Stroke::new(1.0, p.border_subtle))
        .corner_radius(CornerRadius::same(RADIUS_INNER))
        .inner_margin(Margin::symmetric(14, 12))
}

/// Дорожка переключателя: контурная «таблетка», внутри которой сидят сегменты.
pub fn track_frame(p: Palette) -> Frame {
    Frame::new()
        .stroke(Stroke::new(1.0, p.border_strong))
        .corner_radius(CornerRadius::same(RADIUS_PILL))
        .inner_margin(Margin::same(2))
}

/// Блик по верхней грани: одна светлая линия внутри кромки.
///
/// `sweep` — где сейчас бегущее пятно, долей пути слева направо. Единица и
/// больше означает «пятна нет»: остаётся ровно та линия, что была до приёма 06.
///
/// Пятно рисуется `Mesh`-полоской с вершинными цветами, а не отрезком: у
/// `Stroke` цвет один на всю линию, и мягкого края у пятна не вышло бы —
/// получилась бы светлая чёрточка, ползущая по кромке.
fn gloss(ui: &Ui, p: Palette, rect: Rect, sweep: f32) {
    let inset = RADIUS_CARD as f32 * 0.6;
    if rect.width() <= inset * 2.0 {
        return;
    }
    let (left, right) = (rect.left() + inset, rect.right() - inset);
    let y = rect.top() + 0.5;
    let painter = ui.painter();
    painter.hline(Rangef::new(left, right), y, Stroke::new(1.0, p.gloss_line));

    if !(0.0..1.0).contains(&sweep) {
        return;
    }

    // Ширина светового пятна — два радиуса карточки, как в макете.
    let half = RADIUS_CARD as f32;
    // Пятно выезжает из-за левого края и уезжает за правый: иначе оно
    // возникало бы и пропадало на самой грани.
    let at = left - half + (right - left + half * 2.0) * sweep;

    let mut mesh = Mesh::default();
    for (offset, color) in [
        (-half, Color32::TRANSPARENT),
        (0.0, p.gloss_spark),
        (half, Color32::TRANSPARENT),
    ] {
        let x = (at + offset).clamp(left, right);
        mesh.colored_vertex(pos2(x, y - 0.5), color);
        mesh.colored_vertex(pos2(x, y + 0.5), color);
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    mesh.add_triangle(2, 3, 4);
    mesh.add_triangle(3, 5, 4);
    painter.add(Shape::Mesh(Arc::new(mesh)));
}

/// Оболочка рельса разделов: заливка и кромка по правому краю.
///
/// Кромку рисует `Frame::stroke`? Нет — она обвела бы рельс со всех сторон,
/// а нужна одна линия справа. Поэтому её кладёт `paint_rail_edge` кистью,
/// уже после содержимого.
pub fn rail_frame(p: Palette) -> Frame {
    Frame::new()
        .fill(p.rail_fill)
        .inner_margin(Margin::symmetric(NAV_PADDING as i8, 18))
}

/// Кромка по правому краю рельса.
pub fn paint_rail_edge(painter: &Painter, p: Palette, rect: Rect) {
    painter.vline(
        rect.right() - 0.5,
        rect.y_range(),
        Stroke::new(1.0, p.border_subtle),
    );
}

/// Оболочка нижнего блока рельса: обслуживание и настройки окна.
pub fn rail_block_frame(p: Palette) -> Frame {
    Frame::new()
        .fill(p.card_inner)
        .corner_radius(CornerRadius::same(RADIUS_NAV))
        .inner_margin(Margin::same(NAV_BLOCK_MARGIN as i8))
}

/// Закрашивает окно фоном темы.
///
/// Заливки у панелей хватило бы, если бы панель была одна; их несколько,
/// и между ними остаются щели, в которых иначе просвечивал бы чёрный.
pub fn paint_background(painter: &Painter, p: Palette, rect: Rect) {
    painter.rect_filled(rect, 0.0, p.bg);
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

    /// Все непрозрачные поверхности темы, на которых вообще бывает текст.
    ///
    /// Вложенный блок встречается дважды, и оба раза считать обязательно:
    /// его заливка полупрозрачная, так что на карточке и на рельсе это два
    /// разных цвета. Именно на вложенном блоке текст ближе всего к порогу.
    fn surfaces(p: Palette) -> Vec<(&'static str, Color32)> {
        vec![
            ("фон окна", p.bg),
            ("рельс", p.rail_fill),
            ("карточка", p.card_fill),
            ("блок на карточке", over(p.card_inner, p.card_fill)),
            ("блок на рельсе", over(p.card_inner, p.rail_fill)),
            ("поле ввода", p.input_fill),
            ("модалка", p.modal_fill),
        ]
    }

    /// Правило 4: 4.5:1 для текста, 3:1 для границ элементов управления.
    ///
    /// Гоняется по **обеим** палитрам, и это не формальность: светлая тема
    /// переворачивает половину пар, и проверка одной только тёмной пропустила
    /// бы её целиком. Проверено красным: с макетным `border_strong`
    /// (`rgba(255,255,255,.16)`) проверка падает на первой же поверхности.
    #[test]
    fn every_colour_passes_its_threshold_in_both_themes() {
        for (theme, p) in [("тёмная", Palette::dark()), ("светлая", Palette::light())] {
            let all = surfaces(p);

            // Третье поле — разрешён ли цвет на вложенном блоке. У приглушённой
            // подписи там 4.23:1, поэтому ей нельзя, и это записано у самого
            // поля, а не только здесь. Последняя пара — не только капли на
            // значке погоды, но и подпись «40%» под ним.
            let text = [
                ("text_primary", p.text_primary, true),
                ("text_secondary", p.text_secondary, true),
                ("text_muted", p.text_muted, true),
                ("text_faint", p.text_faint, false),
                ("accent", p.accent, true),
                ("accent_text", p.accent_text, true),
                ("state_success", p.state_success, true),
                ("state_error", p.state_error, true),
                ("state_warning", p.state_warning, true),
                ("sky_water", p.sky_water, true),
            ];

            for (name, color, on_inner) in text {
                for (where_, bg) in &all {
                    if !on_inner && where_.starts_with("блок") {
                        continue;
                    }
                    let ratio = contrast(color, *bg);
                    assert!(
                        ratio >= 4.5,
                        "{theme}: {name} на «{where_}» даёт {ratio:.2}:1 — \
                         порог 4.5:1 не проходит"
                    );
                }
            }

            for (name, color) in [
                ("border_strong", p.border_strong),
                ("border_hover", p.border_hover),
            ] {
                for (where_, bg) in &all {
                    let ratio = contrast(color, *bg);
                    assert!(
                        ratio >= 3.0,
                        "{theme}: {name} на «{where_}» даёт {ratio:.2}:1 — \
                         порог 3:1 не проходит"
                    );
                }
            }
        }
    }

    /// На акцентной подложке читаются три цвета — и приглушённый серый
    /// в их число не входит.
    ///
    /// Заливка поднимает фон, и `text_muted` падает там до 4.03:1, оставаясь
    /// на вид тем же самым серым, что и на карточке. Отсюда и проверка: три
    /// разрешённых цвета обязаны проходить на **обеих** подложках, а про
    /// приглушённый утверждается только то, что правило не выдумано, —
    /// в тёмной теме он вправду ниже порога.
    ///
    /// Шире это требовать нельзя, и это выяснилось на красном прогоне: в
    /// светлой теме тот же серый на карточке даёт 5.48:1, то есть проходит,
    /// и падает только на рельсе (4.49:1). Совет «берите `accent_text`»
    /// остаётся общим для обеих тем, но проверкой он держится там, где
    /// вправду ломается.
    #[test]
    fn the_accent_underlay_carries_only_the_colours_it_can() {
        for (theme, p) in [("тёмная", Palette::dark()), ("светлая", Palette::light())] {
            for (where_, base) in [("карточка", p.card_fill), ("рельс", p.rail_fill)] {
                let soft = over(p.accent_soft, base);
                for (name, color) in [
                    ("accent_text", p.accent_text),
                    ("text_secondary", p.text_secondary),
                    ("text_primary", p.text_primary),
                ] {
                    let ratio = contrast(color, soft);
                    assert!(
                        ratio >= 4.5,
                        "{theme}: {name} на акцентной подложке ({where_}) даёт \
                         {ratio:.2}:1 — порог 4.5:1 не проходит"
                    );
                }
            }
        }

        let p = Palette::dark();
        let ratio = contrast(p.text_muted, over(p.accent_soft, p.card_fill));
        assert!(
            ratio < 4.5,
            "приглушённый серый на акцентной подложке вдруг проходит порог \
             ({ratio:.2}:1) — оговорка у `accent_soft` больше не про эту заливку"
        );
    }

    /// На акцентной заливке читается её собственная подпись — и не читается
    /// обычный текст окна.
    ///
    /// Прежнее правило звучало проще — «на акценте текст только тёмный», — но
    /// оно было про одну тему. В светлой акцент сам тёмный, и подпись на нём
    /// белая, так что цвет подписи стал полем палитры. Неизменной осталась
    /// вторая половина: обычным текстом окна акцентную кнопку подписывать
    /// нельзя. Это и есть та ошибка, которую делают не задумываясь, — светлым
    /// по светлому акценту выходит 1.81:1.
    ///
    /// Выключенная заливка во вторую половину не входит, и намеренно. Она
    /// бледная, и в светлой теме на ней читаются оба цвета сразу (обычный
    /// текст даёт там 8.82:1); требовать обратного значило бы выдумать
    /// правило ради симметрии проверки. А в тёмной теме подпись у неё та же,
    /// что у включённой, — «ровно один из двух» там не проверить в принципе.
    #[test]
    fn an_accent_fill_carries_its_own_label_and_not_the_body_text() {
        for (theme, p) in [("тёмная", Palette::dark()), ("светлая", Palette::light())] {
            let fills = [
                ("accent", p.accent, p.text_on_accent),
                ("accent_hover", p.accent_hover, p.text_on_accent),
                ("accent_active", p.accent_active, p.text_on_accent),
                (
                    "accent_disabled",
                    p.accent_disabled,
                    p.text_on_accent_disabled,
                ),
            ];
            for (name, fill, label) in fills {
                let ratio = contrast(label, fill);
                assert!(
                    ratio >= 4.5,
                    "{theme}: своя подпись на {name} даёт {ratio:.2}:1 — \
                     порог 4.5:1 не проходит"
                );
            }

            for (name, fill) in [
                ("accent", p.accent),
                ("accent_hover", p.accent_hover),
                ("accent_active", p.accent_active),
            ] {
                let ratio = contrast(p.text_primary, fill);
                assert!(
                    ratio < 4.5,
                    "{theme}: обычный текст окна на {name} внезапно проходит \
                     порог ({ratio:.2}:1) — проверьте, тот ли это цвет"
                );
            }
        }
    }

    /// Темы обязаны отличаться по светлоте, а не только по набору чисел:
    /// `is_dark` выводится из палитры, и на этом держится выбор `Visuals`.
    #[test]
    fn the_two_themes_know_which_of_them_is_dark() {
        assert!(Palette::dark().is_dark(), "тёмная тема считает себя светлой");
        assert!(
            !Palette::light().is_dark(),
            "светлая тема считает себя тёмной"
        );
        assert!(
            luminance(Palette::light().bg) > luminance(Palette::dark().bg),
            "фон светлой темы не светлее тёмной"
        );
    }

    /// Каждое семейство обязано уметь нарисовать все три алфавита.
    ///
    /// Отсутствующий знак рисуется пустым прямоугольником, и не видят этого
    /// ни сборка, ни `clippy`, ни `cargo test` — только глаза, и только у того,
    /// кто выбрал третий язык. Ровно из-за этого армянский интерфейс без
    /// своего шрифта был бы рядами квадратов: запасной глиф egui подбирает
    /// **строго внутри списка своего семейства**, а Hack с его армянскими
    /// знаками стоит лишь в `Monospace`.
    ///
    /// Проверяется тут и то, ради чего семейства вообще собраны тройками:
    /// у каждой гарнитуры свой алфавит, и выпади любая из списка — тест
    /// покраснеет на том алфавите, который она и закрывала.
    #[test]
    fn every_family_can_draw_all_three_alphabets() {
        // По одному знаку с каждого конца алфавита плюс точка-разделитель,
        // которой Savio делит части строк.
        const SAMPLES: [(&str, &str); 4] = [
            ("латиница", "MP4 Quality"),
            ("кириллица", "Качество Ёё"),
            ("армянский", "Որակ ևֆ"),
            ("разделитель", "·"),
        ];

        let ctx = Context::default();
        install_fonts(&ctx);
        apply(&ctx, Palette::dark());
        // Шрифтов нет до первого кадра — `Context::fonts_mut` на этом прямо
        // паникует, так что кадр обязателен.
        let mut output = ctx.run_ui(Default::default(), |_| {});
        output.textures_delta.clear();

        let families = [
            ("Proportional", FontId::new(15.0, FontFamily::Proportional)),
            ("savio-display", display(22.0)),
            ("savio-bold", bold(15.0)),
        ];
        for (family, font) in families {
            for (alphabet, sample) in SAMPLES {
                assert!(
                    ctx.fonts_mut(|fonts| fonts.has_glyphs(&font, sample)),
                    "{family}: {alphabet} рисуется пустыми прямоугольниками"
                );
            }
        }
    }

    /// QR-код читается камерой, только если модуль заметно темнее поля.
    ///
    /// Цвета кода намеренно не из палитры: возьми их оттуда — и в светлой
    /// теме код вышел бы в обратных цветах, которые часть сканеров не читает.
    #[test]
    fn the_qr_code_is_dark_on_light_in_both_themes() {
        assert!(
            luminance(QR_DARK) < luminance(QR_LIGHT),
            "код в обратных цветах"
        );
        let ratio = contrast(QR_DARK, QR_LIGHT);
        assert!(ratio >= 15.0, "перепад кода всего {ratio:.2}:1");
    }
}
