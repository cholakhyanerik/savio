//! Файлы шрифтов Savio, вшитые в бинарник.
//!
//! Модуль-лист, как `i18n`: шрифты нужны двум слоям сразу. Окно набирает ими
//! интерфейс (`theme::install_fonts`), а движок отдаёт их странице для
//! телефона (`engine::share`), чтобы та выглядела так же, как окно. Лежи байты
//! в `theme.rs`, движку пришлось бы тянуть за ними слой оформления вместе
//! с `egui` — и он перестал бы собираться для CLI и тестов. Лежи по копии
//! в каждом слое — `include_bytes!` одного файла в двух местах, и без LTO
//! компоновщик не обязан их сливать: шестьсот килобайт шрифтов в бинарнике
//! дважды.
//!
//! Здесь только байты: ни `egui`, ни сети, ни разбора. Какой файл для какого
//! алфавита и почему начертания статические — в `assets/fonts/README.md`.

/// Заголовки, латиница.
pub const CAPRASIMO: &[u8] = include_bytes!("../assets/fonts/Caprasimo-Regular.ttf");
/// Заголовки, кириллица.
pub const KELLY_SLAB: &[u8] = include_bytes!("../assets/fonts/KellySlab-Regular.ttf");
/// Основной текст, латиница.
pub const FIGTREE: &[u8] = include_bytes!("../assets/fonts/Figtree-Regular.ttf");
/// Полужирный, латиница.
pub const FIGTREE_BOLD: &[u8] = include_bytes!("../assets/fonts/Figtree-Bold.ttf");
/// Основной текст, кириллица.
pub const NUNITO: &[u8] = include_bytes!("../assets/fonts/Nunito-Regular.ttf");
/// Полужирный, кириллица.
pub const NUNITO_BOLD: &[u8] = include_bytes!("../assets/fonts/Nunito-Bold.ttf");
/// Основной текст, армянский.
pub const ARMENIAN: &[u8] = include_bytes!("../assets/fonts/NotoSansArmenian-Regular.ttf");
/// Полужирный, армянский.
pub const ARMENIAN_BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSansArmenian-Bold.ttf");
/// Заголовки, армянский. Serif, а не sans, ровно затем, чтобы заголовок на
/// армянском оставался заголовком: латиницу и кириллицу там набирают плитные
/// Caprasimo и Kelly Slab, и сансерифная вставка посреди них читалась бы
/// обычным текстом, набранным крупнее.
pub const ARMENIAN_DISPLAY: &[u8] = include_bytes!("../assets/fonts/NotoSerifArmenian-Regular.ttf");
