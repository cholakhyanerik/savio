//! Языки интерфейса и таблицы строк.
//!
//! Модуль-лист: он не знает ни про `egui`, ни про процессы, ни про сеть —
//! потому что пользуются им все три слоя сразу. Домен спрашивает у него
//! подписи формата и качества, движок — тексты предупреждений и объяснений
//! отказа, UI — вообще всё, что видно в окне. Появись здесь хоть одна
//! зависимость от `egui`, и движок перестал бы собираться для CLI и тестов.
//!
//! Язык **ездит параметром**, а не лежит процессной глобалью. Глобаль была бы
//! короче на один аргумент в десятке сигнатур и прятала бы зависимость:
//! чистая функция, читающая скрытое состояние, перестаёт быть проверяемой,
//! а весь остальной проект обходится без глобалей.
//!
//! # Правило 1 и эти таблицы
//!
//! `t` отдаёт `&'static str` и ничего не собирает. Подписи читаются в кадре
//! отрисовки шестьдесят раз в секунду, и `format!` внутри таблицы переводов
//! стоил бы сотен аллокаций в секунду на ровном месте. Там, где в строку надо
//! подставить число или имя, в таблице лежит **шаблон** с `{}`, а подстановку
//! делает [`fill`] — и зовут её из обработчиков событий, а не из кадра.
//!
//! # Чего здесь нет
//!
//! Приметы, по которым узнаётся чужая ошибка (`not a bot`, `in your country`,
//! `maximum supported texture size`), сюда не попадают и попасть не могут: они
//! принадлежат yt-dlp и wgpu, а не Savio, и переводить их — значит перестать
//! узнавать беду. Переводятся только **объяснения** рядом с ними.

/// Язык интерфейса.
///
/// Русский — значение по умолчанию: он был у Savio единственным, и
/// запомненные настройки откатываются именно к нему.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Lang {
    #[default]
    Ru,
    En,
    Am,
}

impl Lang {
    /// Все языки в том порядке, в каком их рисует переключатель шапки.
    pub const ALL: [Lang; 3] = [Lang::Ru, Lang::En, Lang::Am];

    /// Подпись в переключателе — на самом языке, а не на текущем.
    ///
    /// «Рус · Eng · Հայ», а не «Русский · Английский · Армянский»: человек,
    /// открывший чужой язык по ошибке, ищет свой глазами, и найти он должен
    /// знакомое слово, а не перевод его названия на язык, которого не знает.
    /// Заодно это экономит ширину: три сегмента стоят в шапке рядом с номером
    /// версии, и в окне 520 места там нет вовсе.
    pub fn label(self) -> &'static str {
        match self {
            Lang::Ru => "Рус",
            Lang::En => "Eng",
            Lang::Am => "Հայ",
        }
    }

    /// Код для файла настроек. Двухбуквенный и устойчивый: имя варианта
    /// перечисления менять можно, а это — нет, иначе у всех разом слетит
    /// запомненный выбор.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
            Lang::Am => "hy",
        }
    }

    /// Разбирает код из файла настроек. `None` — код чужой.
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|lang| lang.code() == code)
    }
}

/// Объявляет ключ и три его перевода разом.
///
/// Макрос, а не три таблицы рядом, ровно затем, чтобы переводы **нельзя**
/// было разъехать: ключ без одного языка не соберётся, лишний ключ в одной
/// из таблиц не соберётся тоже. Раздельные `HashMap` дали бы ту же таблицу
/// ценой поиска в кадре отрисовки и молчаливого «ключ потерялся».
macro_rules! strings {
    ($( $(#[$meta:meta])* $key:ident = $ru:literal, $en:literal, $am:literal; )*) => {
        /// Ключ строки интерфейса.
        ///
        /// Перечисление, а не сама строка: по ключу компилятор ловит опечатку,
        /// а по русскому тексту — нет, и промахнувшийся поиск обернулся бы
        /// пустой подписью в окне.
        ///
        /// `dead_code` здесь выключен, и это не небрежность: часть ключей
        /// живёт только в ветках `#[cfg]` под чужую ОС (`PowerForeignSystem`
        /// на не-Windows, `SetupChmodFailed` на Unix). Собираясь под Windows,
        /// компилятор их не видит и объявил бы неиспользуемыми — то есть
        /// `-D warnings` ломал бы сборку из-за строк, нужных на соседней
        /// системе. Полноту таблицы держит не эта проверка, а
        /// `every_key_says_something_in_every_language`.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[allow(dead_code)]
        pub enum Key {
            $( $(#[$meta])* $key, )*
        }

        /// Строка на выбранном языке.
        ///
        /// Чистая функция и `&'static str`: звать её из кадра отрисовки можно
        /// (Правило 1).
        pub const fn t(lang: Lang, key: Key) -> &'static str {
            match key {
                $(
                    Key::$key => match lang {
                        Lang::Ru => $ru,
                        Lang::En => $en,
                        Lang::Am => $am,
                    },
                )*
            }
        }

        /// Все ключи вместе с их именами — для тестов полноты таблицы.
        #[cfg(test)]
        pub const ALL_KEYS: &[(Key, &str)] = &[ $( (Key::$key, stringify!($key)), )* ];
    };
}

strings! {
    // -----------------------------------------------------------------------
    // Домен: формат, качество, вход на сайт, субтитры, фрагмент
    // -----------------------------------------------------------------------

    FormatMp4 = "MP4 — видео", "MP4 — video", "MP4 — տեսանյութ";
    FormatMp3 = "MP3 — аудио", "MP3 — audio", "MP3 — ձայն";
    /// Подпись поля над переключателем качества у видео.
    QualityFieldVideo = "Качество", "Quality", "Որակ";
    /// То же у звука: единицу называет подпись, на сегменте для неё места нет.
    ///
    /// По-армянски единица сокращена до «կբ/վ»: полное «կբիթ/վ» не влезает
    /// в колонку подписей и молча обрезается (проверка
    /// `the_field_labels_fit_their_column`). Спутать её тут не с чем —
    /// слово «Բիթրեյթ» стоит рядом, а на дорожке числа 320…96.
    QualityFieldAudio = "Битрейт, кбит/с", "Bitrate, kbps", "Բիթրեյթ, կբ/վ";
    /// Верх шкалы. Короткое намеренно: шесть сегментов делят ширину окна.
    QualityMax = "Макс.", "Max", "Առավ.";
    Kbps320 = "320 кбит/с", "320 kbps", "320 կբիթ/վ";
    Kbps256 = "256 кбит/с", "256 kbps", "256 կբիթ/վ";
    Kbps192 = "192 кбит/с", "192 kbps", "192 կբիթ/վ";
    Kbps128 = "128 кбит/с", "128 kbps", "128 կբիթ/վ";
    Kbps96 = "96 кбит/с", "96 kbps", "96 կբիթ/վ";

    CookieNone = "Не использовать", "Do not use", "Չօգտագործել";
    /// С многоточием, как принято у пунктов, открывающих диалог.
    CookieFile = "Из файла…", "From a file…", "Ֆայլից…";

    SubLangOriginal = "Язык ролика", "Video language", "Տեսանյութի լեզուն";
    SubsNoneAtAll =
        "Субтитров у этого ролика нет вовсе — ни своих, ни автоматических. Вшивать нечего.",
        "This video has no subtitles at all — neither authored nor automatic. There is \
         nothing to embed.",
        "Այս տեսանյութը ենթագրեր ընդհանրապես չունի՝ ո՛չ հեղինակային, ո՛չ ավտոմատ։ \
         Ներկարելու բան չկա։";
    SubsOnlyAuto =
        "Своих субтитров у этого ролика нет, зато есть автоматические. Поставьте «Можно \
         автоматические» — только учтите, что их пишет робот и ошибки в них обычное дело.",
        "This video has no authored subtitles, but it does have automatic ones. Tick \
         “Automatic will do” — but keep in mind that a robot writes them and mistakes \
         there are commonplace.",
        "Այս տեսանյութը հեղինակային ենթագրեր չունի, բայց ունի ավտոմատ։ Նշեք «Ավտոմատը \
         նույնպես կլինի»՝ հաշվի առնելով, որ դրանք գրում է ռոբոտը և սխալները սովորական բան են։";
    /// `{}` — имя языка так, как его назвал источник.
    SubsNoSuchLang =
        "Субтитров на этом языке ({}) у ролика нет — файл сохранится без них. Выберите \
         другой язык из списка.",
        "The video has no subtitles in this language ({}) — the file will be saved without \
         them. Pick another language from the list.",
        "Տեսանյութն այս լեզվով ({}) ենթագրեր չունի — ֆայլը կպահվի առանց դրանց։ Ցանկից \
         ընտրեք այլ լեզու։";

    SectionErrStart =
        "Начало не похоже на время. Нужно «1:30», «1:02:03» или число секунд.",
        "The start does not look like a time. Use “1:30”, “1:02:03” or a number of seconds.",
        "Սկիզբը ժամանակի նման չէ։ Պետք է «1:30», «1:02:03» կամ վայրկյանների թիվ։";
    SectionErrEnd =
        "Конец не похож на время. Нужно «4:00», «1:02:03» или число секунд.",
        "The end does not look like a time. Use “4:00”, “1:02:03” or a number of seconds.",
        "Վերջը ժամանակի նման չէ։ Պետք է «4:00», «1:02:03» կամ վայրկյանների թիվ։";
    SectionErrOrder =
        "Конец должен быть позже начала.",
        "The end must be later than the start.",
        "Վերջը պետք է լինի սկզբից ուշ։";

    // -----------------------------------------------------------------------
    // Единицы
    //
    // Шкала объёма одна на размер и на скорость: разъехавшись, они положили бы
    // рядом «5.0 ГБ из 10.0 ГБ» и «5120.0 МБ/с» — числа одного порядка,
    // выглядящие как разные.
    // -----------------------------------------------------------------------

    UnitByte = "Б", "B", "Բ";
    UnitKilobyte = "КБ", "KB", "ԿԲ";
    UnitMegabyte = "МБ", "MB", "ՄԲ";
    UnitGigabyte = "ГБ", "GB", "ԳԲ";
    UnitTerabyte = "ТБ", "TB", "ՏԲ";
    /// Хвост скорости: приписывается к единице объёма — «МБ/с».
    UnitPerSecond = "/с", "/s", "/վ";
    UnitGigahertz = "ГГц", "GHz", "ԳՀց";
    UnitMegahertz = "МГц", "MHz", "ՄՀց";
    /// `{}` — сколько прошло, `{}` — сколько всего.
    AmountOfTotal = "{} из {}", "{} of {}", "{}՝ {}-ից";

    UptimeDayOne = "день", "day", "օր";
    UptimeDayFew = "дня", "days", "օր";
    UptimeDayMany = "дней", "days", "օր";
    UptimeHourOne = "час", "hour", "ժամ";
    UptimeHourFew = "часа", "hours", "ժամ";
    UptimeHourMany = "часов", "hours", "ժամ";
    UptimeMinuteOne = "минута", "minute", "րոպե";
    UptimeMinuteFew = "минуты", "minutes", "րոպե";
    UptimeMinuteMany = "минут", "minutes", "րոպե";

    // -----------------------------------------------------------------------
    // Снимок системы: состояние пункта и общий итог
    // -----------------------------------------------------------------------

    CheckOk = "В порядке", "All right", "Կարգին է";
    CheckWarning = "Внимание", "Attention", "Ուշադրություն";
    CheckFailed = "Не удалось", "Failed", "Չհաջողվեց";
    /// «Спросили, а значения нет» — это не «хорошо» и не «плохо».
    CheckUnknown = "Нет данных", "No data", "Տվյալներ չկան";

    ReportEmpty = "Отчёт пуст.", "The report is empty.", "Հաշվետվությունը դատարկ է։";
    /// `{}` — сколько пунктов всего, `{}` — сколько из них в порядке.
    ReportCounted =
        "Пунктов: {} — {} в порядке",
        "Items: {} — {} all right",
        "Կետեր՝ {} — {}-ը կարգին է";
    ReportWarned = "{} с замечанием", "{} with a remark", "{}-ը դիտողությամբ";
    ReportFailed = "{} не удалось", "{} failed", "{}-ը չհաջողվեց";
    ReportUnknown = "{} без данных", "{} without data", "{}-ը առանց տվյալների";
    ReportFileTitle =
        "Отчёт Savio о системе",
        "Savio system report",
        "Savio-ի հաշվետվություն համակարգի մասին";
    ReportAdvice = "Совет", "Advice", "Խորհուրդ";

    // -----------------------------------------------------------------------
    // Питание
    // -----------------------------------------------------------------------

    PowerModeSaver =
        "Наилучшая энергоэффективность",
        "Best power efficiency",
        "Լավագույն էներգաարդյունավետություն";
    PowerModeBalanced = "Сбалансированный", "Balanced", "Հավասարակշռված";
    PowerModeHigh =
        "Высокая производительность",
        "High performance",
        "Բարձր արտադրողականություն";
    PowerModeMax =
        "Максимальная производительность",
        "Maximum performance",
        "Առավելագույն արտադրողականություն";

    PowerForeignSystem =
        "Питанием Savio управляет только в Windows: схемы электропитания и «Режим питания» — \
         её понятия. У Linux и macOS соответствия им нет: cpufreq, TLP и pmset устроены \
         иначе и меняют другое, так что показать здесь то же самое не выйдет.",
        "Savio manages power only on Windows: power plans and the “Power mode” are its own \
         notions. Linux and macOS have no counterparts to them: cpufreq, TLP and pmset are \
         built differently and change other things, so showing the same here will not work.",
        "Savio-ն սնուցումը կառավարում է միայն Windows-ում. սնուցման սխեմաներն ու «Սնուցման \
         ռեժիմը» նրա հասկացություններն են։ Linux-ն ու macOS-ը դրանց համարժեքներ չունեն. \
         cpufreq-ը, TLP-ն և pmset-ը կառուցված են այլ կերպ և փոխում են այլ բան, այնպես որ \
         նույնը ցույց տալն այստեղ չի ստացվի։";
    PowerNoLibraryRead =
        "Windows не отдала powrprof.dll — библиотеку, которая заведует питанием. \
         Переключать отсюда нечего.",
        "Windows did not hand over powrprof.dll — the library that runs power management. \
         There is nothing to switch from here.",
        "Windows-ը չտվեց powrprof.dll-ը՝ այն գրադարանը, որը տնօրինում է սնուցումը։ Այստեղից \
         փոխարկելու բան չկա։";
    PowerNoPlans =
        "Список схем электропитания система не отдала — переключать нечего.",
        "The system did not hand over the list of power plans — there is nothing to switch.",
        "Համակարգը սնուցման սխեմաների ցանկը չտվեց — փոխարկելու բան չկա։";
    PowerModesUnsupported =
        "Режим питания эта Windows не поддерживает: он появился в Windows 10 версии 1803.",
        "This Windows does not support the power mode: it appeared in Windows 10 version 1803.",
        "Այս Windows-ը սնուցման ռեժիմ չի աջակցում. այն հայտնվել է Windows 10-ի 1803 \
         տարբերակում։";
    PowerNoLibraryApply =
        "Windows не отдала powrprof.dll — переключать питание нечем.",
        "Windows did not hand over powrprof.dll — there is nothing to switch the power with.",
        "Windows-ը չտվեց powrprof.dll-ը — սնուցումը փոխարկելու բան չկա։";
    /// `{}` — код возврата Windows.
    PowerPlanSetFailed =
        "Windows не переключила схему электропитания (код {}). Обычно так отвечают на схему, \
         которой больше нет: нажмите «Обновить».",
        "Windows did not switch the power plan (code {}). That is the usual answer for a plan \
         that no longer exists: press “Refresh”.",
        "Windows-ը սնուցման սխեման չփոխարկեց (կոդ {})։ Սովորաբար այդպես են պատասխանում այն \
         սխեմային, որն այլևս չկա. սեղմեք «Թարմացնել»։";
    /// `{}` — название схемы, как его дала система.
    PowerPlanApplied =
        "Схема электропитания переключена: «{}».", "The power plan has been switched: “{}”.",
        "Սնուցման սխեման փոխարկվեց՝ «{}»։";
    PowerPlanNotActive =
        "Windows ответила успехом, но активной осталась не «{}». Схему могла вернуть назад \
         политика организации.",
        "Windows answered with success, but the active plan is still not “{}”. An \
         organisation policy may have put the plan back.",
        "Windows-ը պատասխանեց հաջողությամբ, բայց ակտիվը մնաց ոչ «{}»-ը։ Սխեման կարող էր \
         հետ վերադարձնել կազմակերպության քաղաքականությունը։";
    /// `{}` — код возврата Windows.
    PowerModeSetFailed =
        "Windows не переключила режим питания (код {}).",
        "Windows did not switch the power mode (code {}).",
        "Windows-ը սնուցման ռեժիմը չփոխարկեց (կոդ {})։";
    PowerModeUnreadable =
        "Windows приняла режим питания, но назвать действующий отказалась — что стало \
         с машиной, Savio не знает.",
        "Windows accepted the power mode but refused to name the effective one — Savio does \
         not know what became of the machine.",
        "Windows-ն ընդունեց սնուցման ռեժիմը, բայց գործողը անվանելուց հրաժարվեց — ինչ եղավ \
         մեքենայի հետ, Savio-ն չգիտի։";
    /// `{}` — название режима.
    PowerModeApplied =
        "Режим питания переключён: {}.", "The power mode has been switched: {}.",
        "Սնուցման ռեժիմը փոխարկվեց՝ {}։";
    PowerModeIgnored =
        "Режим «{}» Windows запомнила, но не применила.",
        "Windows remembered the “{}” mode but did not apply it.",
        "«{}» ռեժիմը Windows-ը հիշեց, բայց չկիրառեց։";

    // -----------------------------------------------------------------------
    // Погода: единицы, шкалы, ветер, календарь
    // -----------------------------------------------------------------------

    WindMetersPerSecond = "м/с", "m/s", "մ/վ";
    WindKilometersPerHour = "км/ч", "km/h", "կմ/ժ";
    PressureMmHg = "мм рт. ст.", "mmHg", "մմ ս.ս.";
    PressureHectopascal = "гПа", "hPa", "հՊա";
    UnitMillimetre = "мм", "mm", "մմ";
    UnitMicrogram = "мкг/м³", "µg/m³", "մկգ/մ³";

    WindCalm = "штиль", "calm", "անհողմ";
    /// `{}` — скорость порыва, `{}` — единица.
    WindGusts = "порывы до {} {}", "gusts up to {} {}", "պոռթկումները մինչև {} {}";
    WindNorth = "северный", "northerly", "հյուսիսային";
    WindNorthEast = "северо-восточный", "north-easterly", "հյուսիս-արևելյան";
    WindEast = "восточный", "easterly", "արևելյան";
    WindSouthEast = "юго-восточный", "south-easterly", "հարավ-արևելյան";
    WindSouth = "южный", "southerly", "հարավային";
    WindSouthWest = "юго-западный", "south-westerly", "հարավ-արևմտյան";
    WindWest = "западный", "westerly", "արևմտյան";
    WindNorthWest = "северо-западный", "north-westerly", "հյուսիս-արևմտյան";

    UvLow = "низкий", "low", "ցածր";
    UvModerate = "умеренный", "moderate", "չափավոր";
    UvHigh = "высокий", "high", "բարձր";
    UvVeryHigh = "очень высокий", "very high", "շատ բարձր";
    UvExtreme = "экстремальный", "extreme", "ծայրահեղ";

    AqiGood = "хорошее", "good", "լավ";
    AqiFair = "удовлетворительное", "fair", "բավարար";
    AqiModerate = "умеренное", "moderate", "չափավոր";
    AqiPoor = "плохое", "poor", "վատ";
    AqiVeryPoor = "очень плохое", "very poor", "շատ վատ";
    AqiExtreme = "крайне плохое", "extremely poor", "ծայրահեղ վատ";

    /// Число и словесная оценка рядом: «7 — высокий», «33 — удовлетворительное».
    ValueWithLevel = "{} — {}", "{} — {}", "{} — {}";

    WeekdayMon = "пн", "Mon", "երկ";
    WeekdayTue = "вт", "Tue", "երք";
    WeekdayWed = "ср", "Wed", "չրք";
    WeekdayThu = "чт", "Thu", "հնգ";
    WeekdayFri = "пт", "Fri", "ուրբ";
    WeekdaySat = "сб", "Sat", "շբթ";
    WeekdaySun = "вс", "Sun", "կիր";

    MonthJan = "янв", "Jan", "հնվ";
    MonthFeb = "фев", "Feb", "փտվ";
    MonthMar = "мар", "Mar", "մրտ";
    MonthApr = "апр", "Apr", "ապր";
    MonthMay = "мая", "May", "մյս";
    MonthJun = "июн", "Jun", "հնս";
    MonthJul = "июл", "Jul", "հլս";
    MonthAug = "авг", "Aug", "օգս";
    MonthSep = "сен", "Sep", "սեպ";
    MonthOct = "окт", "Oct", "հոկ";
    MonthNov = "ноя", "Nov", "նոյ";
    MonthDec = "дек", "Dec", "դեկ";

    /// «16 сен»: `{}` — число, `{}` — месяц. Порядок у языков разный, поэтому
    /// подстановка, а не склейка.
    DateDayMonth = "{} {}", "{} {}", "{} {}";
    /// «ср, 16 сен»: `{}` — день недели, `{}` — дата.
    DateWeekdayAndDay = "{}, {}", "{}, {}", "{}, {}";
    DayToday = "Сегодня", "Today", "Այսօր";
    DayTomorrow = "Завтра", "Tomorrow", "Վաղը";

    WeatherUpdated = "Обновлено", "Updated", "Թարմացվել է";
    WeatherSavedReport = "Сохранённый отчёт", "Saved report", "Պահպանված հաշվետվություն";
    /// `{}` — «Обновлено» или «Сохранённый отчёт», `{}` — часы и минуты.
    WeatherUpdatedAt = "{} в {}", "{} at {}", "{} {}-ին";
    /// То же, но не сегодня: `{}` — подпись, `{}` — дата, `{}` — время.
    WeatherUpdatedOn = "{} {} в {}", "{} {} at {}", "{} {} {}-ին";
    /// Часовой пояс машины система не назвала — говорим время по UTC.
    WeatherUpdatedUtc = "{} в {} UTC", "{} at {} UTC", "{} {}-ին UTC";
    WeatherClockBroken =
        "Когда получен прогноз, неизвестно: часы компьютера показывают дату раньше 1970 года.",
        "There is no telling when the forecast arrived: the computer clock shows a date \
         earlier than 1970.",
        "Երբ է ստացվել կանխատեսումը, հայտնի չէ. համակարգչի ժամացույցը ցույց է տալիս \
         1970-ից շուտ ամսաթիվ։";
    WeatherNow = "Сейчас", "Now", "Հիմա";
    /// `{}` — температура вместе со знаком градуса.
    WeatherFeelsLike = "ощущается как {}", "feels like {}", "զգացվում է {}";
    WeatherHumidity = "Влажность", "Humidity", "Խոնավություն";
    WeatherWind = "Ветер", "Wind", "Քամի";
    WeatherPressure = "Давление", "Pressure", "Ճնշում";
    WeatherCloudCover = "Облачность", "Cloud cover", "Ամպամածություն";
    WeatherPrecipitation = "Осадки", "Precipitation", "Տեղումներ";
    WeatherUvIndex = "УФ-индекс", "UV index", "ՈՒՄ ինդեքս";
    WeatherSunrise = "Восход", "Sunrise", "Արևածագ";
    WeatherSunset = "Закат", "Sunset", "Մայրամուտ";
    WeatherAir = "Воздух", "Air", "Օդ";

    WmoUnknown = "Погода без описания", "Weather without a description", "Եղանակ առանց նկարագրի";
    WmoClear = "Ясно", "Clear", "Պարզ";
    WmoMostlyClear = "Преимущественно ясно", "Mostly clear", "Հիմնականում պարզ";
    WmoPartlyCloudy = "Переменная облачность", "Partly cloudy", "Փոփոխական ամպամածություն";
    WmoOvercast = "Пасмурно", "Overcast", "Մառախլապատ ամպամած";
    WmoFog = "Туман", "Fog", "Մառախուղ";
    WmoRimeFog = "Туман с изморозью", "Rime fog", "Մառախուղ եղյամով";
    WmoLightDrizzle = "Слабая морось", "Light drizzle", "Թույլ մաղում";
    WmoDrizzle = "Морось", "Drizzle", "Մաղում";
    WmoHeavyDrizzle = "Сильная морось", "Heavy drizzle", "Ուժեղ մաղում";
    WmoLightFreezingDrizzle =
        "Слабая ледяная морось", "Light freezing drizzle", "Թույլ սառցե մաղում";
    WmoFreezingDrizzle = "Ледяная морось", "Freezing drizzle", "Սառցե մաղում";
    WmoLightRain = "Слабый дождь", "Light rain", "Թույլ անձրև";
    WmoRain = "Дождь", "Rain", "Անձրև";
    WmoHeavyRain = "Сильный дождь", "Heavy rain", "Ուժեղ անձրև";
    WmoLightFreezingRain = "Слабый ледяной дождь", "Light freezing rain", "Թույլ սառցե անձրև";
    WmoFreezingRain = "Ледяной дождь", "Freezing rain", "Սառցե անձրև";
    WmoLightSnow = "Слабый снег", "Light snow", "Թույլ ձյուն";
    WmoSnow = "Снег", "Snow", "Ձյուն";
    WmoHeavySnow = "Сильный снег", "Heavy snow", "Ուժեղ ձյուն";
    WmoSnowGrains = "Снежные зёрна", "Snow grains", "Ձյան հատիկներ";
    WmoLightShowers = "Слабый ливень", "Light showers", "Թույլ տեղատարափ";
    WmoShowers = "Ливень", "Showers", "Տեղատարափ";
    WmoHeavyShowers = "Сильный ливень", "Heavy showers", "Ուժեղ տեղատարափ";
    WmoLightSnowShowers = "Слабый снегопад", "Light snow showers", "Թույլ ձնատեղում";
    WmoHeavySnowShowers = "Сильный снегопад", "Heavy snow showers", "Ուժեղ ձնատեղում";
    WmoThunder = "Гроза", "Thunderstorm", "Ամպրոպ";
    WmoThunderSmallHail =
        "Гроза с небольшим градом", "Thunderstorm with slight hail", "Ամպրոպ թեթև կարկուտով";
    WmoThunderHeavyHail =
        "Гроза с сильным градом", "Thunderstorm with heavy hail", "Ամպրոպ ուժեղ կարկուտով";

    // -----------------------------------------------------------------------
    // Раздача файлов на телефон
    // -----------------------------------------------------------------------

    TransferToComputer = "На компьютер", "To the computer", "Համակարգիչ";
    TransferToPhone = "На телефон", "To the phone", "Հեռախոս";

    // Ответы телефону и новости раздачи. Язык здесь тот же, что в окне: телефон
    // и компьютер — одного человека, и разводить их по языкам незачем.
    ShareTooManyConnections =
        "Слишком много подключений. Попробуйте через минуту.",
        "Too many connections. Try again in a minute.",
        "Չափից շատ միացումներ։ Փորձեք մեկ րոպեից։";
    ShareBadRequest = "Запрос не разобран.", "The request was not parsed.",
        "Հարցումը չվերծանվեց։";
    ShareBadPath = "Адрес не разобран.", "The address was not parsed.",
        "Հասցեն չվերծանվեց։";
    /// Ответ на запрос значка вкладки: страницы такой нет, и это не беда.
    ShareNothingHere = "Нет.", "Nothing here.", "Ոչինչ։";
    ShareMethodNotAllowed = "Так нельзя.", "Not allowed.", "Այդպես չի կարելի։";
    ShareNoSuchPage = "Такой страницы нет.", "There is no such page.", "Այդպիսի էջ չկա։";
    ShareNoSuchFile =
        "Такого файла в папке раздачи нет.", "There is no such file in the shared folder.",
        "Այդպիսի ֆայլ բաշխման թղթապանակում չկա։";
    ShareFileNotOpened = "Файл не открылся.", "The file did not open.", "Ֆայլը չբացվեց։";
    ShareSizeUnknown =
        "Размер файла не узнать.", "The file size cannot be read.",
        "Ֆայլի չափը հնարավոր չէ պարզել։";
    ShareFileRejected = "Файл не принят.", "The file was not accepted.", "Ֆայլն ընդունված չէ։";
    ShareFileNotSaved = "Файл не сохранён.", "The file was not saved.", "Ֆայլը չպահվեց։";
    ShareNoFileName = "У файла нет имени.", "The file has no name.", "Ֆայլն անուն չունի։";
    ShareNeedLength = "Нужна длина файла.", "The file length is required.",
        "Պետք է ֆայլի երկարությունը։";
    ShareCannotWriteDir =
        "Компьютер не может записать файл в папку.",
        "The computer cannot write the file into the folder.",
        "Համակարգիչը չի կարող ֆայլը գրել թղթապանակ։";

    SharePortsBusy =
        "Не удалось открыть порт для раздачи: все подходящие заняты другими программами. \
         Закройте лишнее и попробуйте ещё раз.",
        "Could not open a port for sharing: all the suitable ones are taken by other \
         programs. Close what you do not need and try again.",
        "Չհաջողվեց բացել բաշխման պորտ. բոլոր հարմարները զբաղված են այլ ծրագրերով։ Փակեք \
         ավելորդը և նորից փորձեք։";
    ShareNoLocalAddress =
        "У компьютера нет адреса в локальной сети — телефону некуда подключаться. \
         Проверьте, что компьютер подключён к Wi-Fi или кабелем к роутеру.",
        "The computer has no address on the local network — the phone has nowhere to connect. \
         Check that the computer is connected to Wi-Fi or to the router by cable.",
        "Համակարգիչը տեղական ցանցում հասցե չունի — հեռախոսին միանալու տեղ չկա։ Ստուգեք, որ \
         համակարգիչը միացած է Wi-Fi-ին կամ մալուխով՝ երթուղիչին։";
    /// `{}` — имя файла.
    ShareStoppedSending =
        "Раздача остановлена — «{}» не передан до конца.",
        "Sharing was stopped — “{}” was not sent in full.",
        "Բաշխումը կանգնեցվեց — «{}»-ը մինչև վերջ չփոխանցվեց։";
    ShareReadFailed =
        "«{}» не прочитался с диска до конца.",
        "“{}” was not read from disk in full.",
        "«{}»-ը սկավառակից մինչև վերջ չկարդացվեց։";
    SharePhoneStoppedReceiving =
        "Передача «{}» на телефон оборвалась: телефон перестал принимать.",
        "Sending “{}” to the phone broke off: the phone stopped receiving.",
        "«{}»-ի փոխանցումը հեռախոս ընդհատվեց. հեռախոսը դադարեց ընդունել։";
    /// `{}` — имя файла, `{}` — то, что сказала система.
    ShareCannotAcceptDir =
        "Не удалось принять «{}»: в папку раздачи нельзя записать ({}). Выберите другую папку.",
        "Could not accept “{}”: the shared folder cannot be written to ({}). Pick another \
         folder.",
        "Չհաջողվեց ընդունել «{}»-ը. բաշխման թղթապանակում գրել հնարավոր չէ ({})։ Ընտրեք այլ \
         թղթապանակ։";
    ShareStoppedReceiving =
        "Раздача остановлена — «{}» не принят и не сохранён.",
        "Sharing was stopped — “{}” was neither accepted nor saved.",
        "Բաշխումը կանգնեցվեց — «{}»-ը չընդունվեց և չպահվեց։";
    ShareDiskFull =
        "Не хватило места на диске — «{}» не сохранён.",
        "There was not enough room on the disk — “{}” was not saved.",
        "Սկավառակի վրա տեղը չբավականացրեց — «{}»-ը չպահվեց։";
    /// `{}` — имя файла, `{}` — то, что сказала система.
    ShareWriteFailed =
        "Не удалось записать «{}»: {}.", "Could not write “{}”: {}.",
        "Չհաջողվեց գրել «{}»-ը՝ {}։";
    SharePhoneStoppedSending =
        "Передача «{}» оборвалась: телефон перестал отправлять. Файл не сохранён — \
         отправьте его ещё раз.",
        "Receiving “{}” broke off: the phone stopped sending. The file was not saved — \
         send it again.",
        "«{}»-ի փոխանցումն ընդհատվեց. հեռախոսը դադարեց ուղարկել։ Ֆայլը չպահվեց — ուղարկեք \
         այն կրկին։";
    ShareNotMoved =
        "«{}» принят, но не лёг в папку: {}.",
        "“{}” was accepted but did not land in the folder: {}.",
        "«{}»-ն ընդունվեց, բայց թղթապանակ չընկավ՝ {}։";
    ShareNamesExhausted =
        "все имена от «(2)» до «(999)» уже заняты",
        "every name from “(2)” to “(999)” is already taken",
        "«(2)»-ից «(999)» բոլոր անունները արդեն զբաղված են";
    /// Подпись подключившегося, когда по `User-Agent` не узнать ничего.
    ShareUnknownDevice = "Устройство", "Device", "Սարք";
    /// Заголовок страницы для телефона, пришедшего со старым ключом.
    ShareStaleTitle = "Ссылка устарела", "The link is out of date", "Հղումը հնացել է";
    // Страница, которую открывает телефон. Подставляются эти строки в
    // размётку `assets/share/index.html` (см. `share::page`), поэтому ни
    // кавычек-лапок, ни `<`, ни обратной косой в них быть не должно: часть
    // из них попадает внутрь строкового литерала JavaScript. Держит это
    // `share::tests::the_phone_page_strings_are_safe_to_paste`.
    PageLangTag = "ru", "en", "hy";
    PageLocaleTag = "ru-RU", "en-GB", "hy-AM";
    PageTitle = "Savio — обмен файлами", "Savio — file exchange", "Savio — ֆայլերի փոխանակում";
    PageSubtitle = "обмен файлами", "file exchange", "ֆայլերի փոխանակում";
    PageUploadNote =
        "Фото, видео, музыка — любого размера. Файлы ложатся в папку раздачи как есть, \
         без пережатия.",
        "Photos, videos, music — any size. Files land in the shared folder as they are, \
         with no re-encoding.",
        "Լուսանկարներ, տեսանյութեր, երաժշտություն — ցանկացած չափի։ Ֆայլերը ընկնում են \
         բաշխման թղթապանակ այնպես, ինչպես կան, առանց վերասեղմման։";
    PagePickFiles = "Выбрать файлы", "Choose files", "Ընտրել ֆայլեր";
    PageIosNote =
        "iPhone может пережать видео, выбранное из «Фото». Чтобы отправить оригинал, \
         сохраните ролик в «Файлы» и выберите его оттуда.",
        "An iPhone may re-encode a video picked from Photos. To send the original, save the \
         clip into Files and pick it from there.",
        "iPhone-ը կարող է վերասեղմել «Լուսանկարներից» ընտրված տեսանյութը։ Բնօրինակն \
         ուղարկելու համար պահեք հոլովակը «Ֆայլերում» և ընտրեք այնտեղից։";
    PageAwakeNote =
        "Не гасите экран и не уходите со страницы, пока файлы идут: браузер телефона может \
         остановить передачу.",
        "Do not let the screen go dark and do not leave the page while files are going: the \
         phone browser may stop the transfer.",
        "Մի՛ հանգցրեք էկրանը և մի՛ հեռացեք էջից, քանի դեռ ֆայլերը գնում են. հեռախոսի \
         բրաուզերը կարող է կանգնեցնել փոխանցումը։";
    PageFromComputer = "С компьютера", "From the computer", "Համակարգչից";
    PageRefresh = "Обновить", "Refresh", "Թարմացնել";
    PageLoadingList = "Загружаю список…", "Loading the list…", "Բեռնում եմ ցանկը…";
    PageOffline =
        "Компьютер не отвечает: раздачу остановили, или телефон ушёл из той сети, где \
         компьютер.",
        "The computer is not answering: sharing was stopped, or the phone left the network \
         the computer is on.",
        "Համակարգիչը չի պատասխանում. բաշխումը կանգնեցվել է, կամ հեռախոսը հեռացել է այն \
         ցանցից, որտեղ համակարգիչն է։";
    /// Единицы объёма для скрипта страницы, через пробел.
    ///
    /// Через пробел, а не литералом-массивом: в разметку эта строка попадает
    /// внутрь строки JavaScript, и кавычки внутри неё закрыли бы её раньше
    /// времени. Пробела ни в одной единице нет ни на одном языке — на этом
    /// разбор и держится.
    PageByteUnits = "Б КБ МБ ГБ ТБ", "B KB MB GB TB", "Բ ԿԲ ՄԲ ԳԲ ՏԲ";
    PageQueued = "Ждёт очереди · ", "Waiting in line · ", "Սպասում է հերթին · ";
    PageSending = "Отправляю…", "Sending…", "Ուղարկում եմ…";
    /// `{}` — размер файла.
    PageDoneWithSize = "Готово · {}", "Done · {}", "Պատրաստ է · {}";
    /// `{}` — имя, под которым файл лёг в папку.
    PageSavedAs =
        "Готово · сохранён как «{}»", "Done · saved as “{}”", "Պատրաստ է · պահվել է որպես «{}»";
    PageNotAccepted =
        "Компьютер не принял файл. Попробуйте ещё раз.",
        "The computer did not accept the file. Try again.",
        "Համակարգիչը ֆայլը չընդունեց։ Փորձեք կրկին։";
    PageTransferBroke =
        "Передача оборвалась. Проверьте, что телефон в той же сети.",
        "The transfer broke off. Check that the phone is on the same network.",
        "Փոխանցումն ընդհատվեց։ Ստուգեք, որ հեռախոսը նույն ցանցում է։";
    PageSendAgain = "Отправить ещё раз", "Send again", "Ուղարկել կրկին";
    PageListFailedRetry =
        "Список не пришёл. Нажмите «Обновить».",
        "The list did not arrive. Press “Refresh”.",
        "Ցանկը չեկավ։ Սեղմեք «Թարմացնել»։";
    PageListFailed = "Список не пришёл.", "The list did not arrive.", "Ցանկը չեկավ։";
    PageFolderContents =
        "Что лежит в папке раздачи. Свежие сверху.",
        "What is in the shared folder. Newest on top.",
        "Ինչ կա բաշխման թղթապանակում։ Թարմերը վերևում։";
    PageFolderEmpty = "Папка раздачи пуста.", "The shared folder is empty.",
        "Բաշխման թղթապանակը դատարկ է։";
    PageOpen = "Открыть", "Open", "Բացել";
    PageDownload = "Скачать", "Download", "Ներբեռնել";

    ShareStaleNote =
        "Откройте адрес заново из окна Savio — ключ в нём меняется при каждом запуске \
         раздачи.",
        "Open the address again from the Savio window — the key in it changes every time \
         sharing starts.",
        "Բացեք հասցեն նորից Savio-ի պատուհանից — դրա բանալին փոխվում է բաշխման յուրաքանչյուր \
         մեկնարկի ժամանակ։";

    // Погода: стадии и отказы серверов.
    StageLocatingByIp =
        "Определяю место по IP-адресу…", "Working out the place from the IP address…",
        "Որոշում եմ վայրն ըստ IP-հասցեի…";
    StageFetchingForecast =
        "Запрашиваю прогноз…", "Requesting the forecast…", "Հարցում եմ կանխատեսումը…";
    WeatherServerForecast = "Сервер погоды", "The weather server", "Եղանակի սերվերը";
    WeatherServerSearch =
        "Сервер поиска городов", "The city search server", "Քաղաքների որոնման սերվերը";
    WeatherServerLocate =
        "Сервер определения места", "The location server", "Վայրի որոշման սերվերը";
    /// `{}` — имя сервера из трёх выше.
    WeatherUnreadableAnswer =
        "{} прислал ответ, который не удалось дочитать.",
        "{} sent an answer that could not be read to the end.",
        "{} ուղարկեց պատասխան, որը չհաջողվեց մինչև վերջ կարդալ։";
    WeatherUnknownShape =
        "{} прислал ответ в незнакомом виде. Попробуйте позже.",
        "{} sent an answer in an unfamiliar shape. Try later.",
        "{} ուղարկեց պատասխան անծանոթ տեսքով։ Փորձեք ավելի ուշ։";
    WeatherTimedOut =
        "{} не ответил вовремя. Проверьте подключение к интернету.",
        "{} did not answer in time. Check your internet connection.",
        "{} ժամանակին չպատասխանեց։ Ստուգեք ինտերնետ կապը։";
    WeatherUnreachable =
        "{} недоступен. Проверьте подключение к интернету.",
        "{} is unreachable. Check your internet connection.",
        "{} հասանելի չէ։ Ստուգեք ինտերնետ կապը։";
    WeatherBadCoordinates =
        "У выбранного места неверные координаты. Найдите его заново через поиск.",
        "The chosen place has wrong coordinates. Find it again through the search.",
        "Ընտրված վայրի կոորդինատները սխալ են։ Գտեք այն նորից որոնման միջոցով։";
    /// `{}` — имя сервера, `{}` — код ответа.
    WeatherServerDown =
        "{} сейчас не работает (код {}). Попробуйте позже.",
        "{} is not working right now (code {}). Try later.",
        "{} այս պահին չի աշխատում (կոդ {})։ Փորձեք ավելի ուշ։";
    /// `{}` — имя сервера.
    WeatherTooManyRequests =
        "{} просит подождать: с вашего адреса слишком много запросов. Попробуйте через минуту.",
        "{} asks you to wait: too many requests are coming from your address. Try again in \
         a minute.",
        "{} խնդրում է սպասել. ձեր հասցեից չափից շատ հարցումներ են գալիս։ Փորձեք մեկ րոպեից։";
    WeatherLocateFailed =
        "Не удалось определить место по IP-адресу: серверы определения места не ответили. \
         Найдите свой город через поиск.",
        "Could not work out the place from the IP address: the location servers did not \
         answer. Find your city through the search.",
        "Չհաջողվեց որոշել վայրն ըստ IP-հասցեի. վայրի որոշման սերվերները չպատասխանեցին։ \
         Գտեք ձեր քաղաքը որոնման միջոցով։";
    /// `{}` — имя сервера, `{}` — объяснение от него же.
    WeatherRefused = "{} отказал: {}", "{} refused: {}", "{} մերժեց՝ {}";
    WeatherStatusCode =
        "{} ответил кодом {}.", "{} answered with code {}.", "{} պատասխանեց {} կոդով։";
    WeatherNoForecast =
        "Сервер погоды прислал ответ без прогноза. Попробуйте обновить позже.",
        "The weather server sent an answer without a forecast. Try refreshing later.",
        "Եղանակի սերվերն ուղարկեց պատասխան առանց կանխատեսման։ Փորձեք թարմացնել ավելի ուշ։";

    // -----------------------------------------------------------------------
    // Окно: разделы, состояния, кнопки, подписи полей
    // -----------------------------------------------------------------------

    // Подписи разделов стоят в дорожке шапки, а её ширина в окне 520
    // выбрана впритык: по-русски запас — считаные точки. Поэтому здесь
    // берутся самые короткие верные слова, как «Ещё» вместо «Ещё разделы».
    // Держит это `the_header_fits_the_smallest_window`.
    TabDownload = "Загрузка", "Download", "Բեռնում";
    TabMetadata = "Метаданные", "Metadata", "Մետատվյալ";
    TabMachine = "Машина", "Machine", "Մեքենա";
    TabMore = "Ещё", "More", "Այլ";
    TabWeather = "Погода", "Weather", "Եղանակ";
    TabPhone = "Телефон", "Phone", "Հեռախոս";

    // Группы рельса разделов и короткие пояснения к ним. Пояснение стоит
    // в строке заголовка, справа от названия раздела, и отвечает на «что
    // я тут увижу» до того, как человек начнёт читать карточки.
    NavGroupFiles = "Файлы", "Files", "Ֆայլեր";
    NavGroupComputer = "Компьютер", "Computer", "Համակարգիչ";
    NavGroupNearby = "Рядом", "Nearby", "Մոտակայքում";
    NavDownloadNote =
        "вставьте ссылку, остальное можно не трогать",
        "paste a link, the rest can stay as it is",
        "տեղադրեք հղումը, մնացածին կարելի է չդիպչել";
    NavDownloadBusyNote =
        "ход загрузки — в правой колонке",
        "the progress is in the right column",
        "ներբեռնման ընթացքը՝ աջ սյունակում";
    NavMetadataNote =
        "что файл рассказывает о вас — и как это стереть",
        "what the file tells about you — and how to wipe it",
        "ինչ է ֆայլը պատմում ձեր մասին և ինչպես ջնջել այն";
    NavMachineNowNote =
        "замер раз в секунду, пока открыт этот раздел",
        "measured once a second while this section is open",
        "չափում վայրկյանը մեկ, քանի դեռ այս բաժինը բաց է";
    NavMachineSpecNote =
        "сведения о железе этой машины",
        "what hardware this machine has",
        "տեղեկություններ այս մեքենայի սարքավորումների մասին";
    NavWeatherNote =
        "сейчас, по часам и на неделю",
        "now, by the hour and for the week",
        "հիմա, ժամերով և շաբաթվա համար";
    NavPhoneNote =
        "файлы между телефоном и компьютером по своей сети Wi-Fi",
        "files between the phone and the computer over your own Wi-Fi",
        "ֆայլեր հեռախոսի և համակարգչի միջև ձեր Wi-Fi ցանցով";
    NavPhoneOnNote =
        "страница открыта, телефон подключился",
        "the page is open, the phone has connected",
        "էջը բաց է, հեռախոսը միացել է";
    /// Подсказка кнопки, которая раскрывает нижний блок свёрнутого рельса.
    NavMoreHint =
        "Обслуживание и настройки окна: версии инструментов, тема, язык, журнал.",
        "Upkeep and window settings: tool versions, theme, language, log.",
        "Սպասարկում և պատուհանի կարգավորումներ՝ գործիքների տարբերակներ, տեսք, լեզու, մատյան։";
    UiAllInPlaceShort = "Всё на месте", "All in place", "Ամեն ինչ տեղում է";
    MachineNow = "Сейчас", "Now", "Հիմա";
    /// По-английски «Make-up» читалось косметикой, а не составом машины.
    /// Пока подпись жила внутри вкладки «Машина», это сходило с рук; в рельсе
    /// она стоит отдельным пунктом верхнего уровня, и контекста рядом нет.
    MachineSpec = "Состав", "Hardware", "Կազմ";
    RailQueue = "Очередь", "Queue", "Հերթ";
    RailHistory = "История", "History", "Պատմություն";

    QueueWaiting = "Ожидает", "Waiting", "Սպասում է";
    QueueRunning = "Качается", "Downloading", "Ներբեռնվում է";
    QueueDone = "Готово", "Done", "Պատրաստ է";
    QueueFailed = "Ошибка", "Error", "Սխալ";
    QueueCancelled = "Снято", "Cancelled", "Հանված";

    SummaryRunning = "Идёт", "Going", "Ընթանում է";
    SummaryWaiting = "В очереди", "In queue", "Հերթում";
    SummaryDone = "Готово", "Done", "Պատրաստ";
    SummaryFailed = "Ошибок", "Errors", "Սխալներ";
    SummaryCancelled = "Снято", "Cancelled", "Հանված";
    /// «Идёт: 1»: `{}` — слово, `{}` — число.
    SummaryPair = "{}: {}", "{}: {}", "{}՝ {}";

    StateIdle = "Готов к работе", "Ready", "Պատրաստ է աշխատանքի";
    StateQueued = "В очереди", "In queue", "Հերթում";
    StateRunning = "Загрузка", "Downloading", "Ներբեռնում";
    StateDone = "Готово", "Done", "Պատրաստ է";
    StateFailed = "Ошибка", "Error", "Սխալ";
    StateCancelled = "Отменено", "Cancelled", "Չեղարկված";

    StageStarting = "Запуск…", "Starting…", "Գործարկում…";
    StageCancelled = "Отменено", "Cancelled", "Չեղարկված է";
    StageDone = "Готово", "Done", "Պատրաստ է";
    StageError = "Ошибка", "Error", "Սխալ";
    StageCheckingMissing =
        "Проверяю, чего не хватает…", "Checking what is missing…",
        "Ստուգում եմ, թե ինչ է պակասում…";
    StageCheckingVersion = "Проверяю версию…", "Checking the version…", "Ստուգում եմ տարբերակը…";
    StageGettingReady = "Готовлюсь скачивать…", "Getting ready to download…",
        "Պատրաստվում եմ ներբեռնելու…";

    GpuTooLarge =
        "Окно оказалось больше, чем может отрисовать видеокарта (предел — 8192 точки по \
         стороне). Картинка временно не обновляется; уменьшите окно, и рисование \
         восстановится. Загрузка при этом не прервана.",
        "The window turned out larger than the graphics card can draw (the limit is 8192 \
         points a side). The picture is not being refreshed for now; make the window smaller \
         and drawing will come back. The download has not been interrupted.",
        "Պատուհանն ավելի մեծ ստացվեց, քան կարող է նկարել վիդեոքարտը (սահմանը՝ կողմում 8192 \
         կետ)։ Պատկերը ժամանակավորապես չի թարմացվում. փոքրացրեք պատուհանը, և նկարելը \
         կվերականգնվի։ Ներբեռնումն այդ ընթացքում ընդհատված չէ։";
    /// `{}` — сообщение от wgpu, как есть.
    GpuDrawError = "Ошибка отрисовки: {}", "Drawing error: {}", "Նկարման սխալ՝ {}";

    UiPickCookieFile = "Выбрать файл…", "Choose a file…", "Ընտրել ֆայլ…";
    UiNoFileChosen = "файл не выбран", "no file chosen", "ֆայլն ընտրված չէ";
    UiNoFolderChosen = "не выбрана", "not chosen", "ընտրված չէ";
    /// Имя автора — транслитерация, а не перевод: у имени перевода не бывает,
    /// но чужой алфавит посреди своего читается хуже, чем то же имя своими
    /// буквами.
    UiAuthor = "Эрик Чолахян", "Erik Cholakhyan", "Էրիկ Չոլախյան";
    UiAboutText =
        "Savio — это кроссплатформенное десктопное приложение для скачивания видео и аудио \
         с популярных онлайн-платформ. Приложение позволяет быстро загружать контент по \
         ссылке: по умолчанию — в максимально доступном качестве, а при желании можно \
         выбрать разрешение видео или битрейт звука самому.",
        "Savio is a cross-platform desktop application for downloading video and audio from \
         popular online platforms. It lets you fetch content quickly by link: by default in \
         the best quality available, and if you like you can pick the video resolution or \
         the audio bitrate yourself.",
        "Savio-ն բազմահարթակ սեղանադիր ծրագիր է՝ հայտնի առցանց հարթակներից տեսանյութ և \
         ձայն ներբեռնելու համար։ Ծրագիրը թույլ է տալիս արագ ներբեռնել բովանդակությունը \
         հղումով՝ լռելյայն առավելագույն հասանելի որակով, իսկ ցանկության դեպքում կարելի է \
         ինքնուրույն ընտրել տեսանյութի լուծաչափը կամ ձայնի բիթրեյթը։";

    // Экран загрузки: поле ссылки, предпросмотр, фрагмент, вшивание.
    UiUrlHint = "Вставьте ссылку: https://…", "Paste a link: https://…", "Տեղադրեք հղում՝ https://…";
    UiNotALink =
        "Похоже, это не ссылка. Нужен адрес вида https://…",
        "This does not look like a link. An address of the form https://… is needed.",
        "Սա հղման նման չէ։ Պետք է https://… տեսքի հասցե։";
    UiPreviewAsking =
        "Смотрю, что это за ролик…", "Looking at what this video is…",
        "Նայում եմ, թե ինչ տեսանյութ է…";
    UiPreviewFailed =
        "Что это за ролик, выяснить не вышло: сайт не ответил или не поддерживается. \
         Скачать всё равно можно — нажмите «Скачать».",
        "It was not possible to find out what this video is: the site did not answer, or it \
         is not supported. You can still download it — press “Download”.",
        "Չհաջողվեց պարզել, թե ինչ տեսանյութ է. կայքը չպատասխանեց կամ չի աջակցվում։ \
         Ներբեռնել միևնույն է կարելի — սեղմեք «Ներբեռնել»։";
    /// `{}` — высота кадра, которую источник вправду отдаёт (дважды).
    UiQualityNote =
        "Выше {}p этот ролик не отдают — скачается {}p.",
        "This video is not served above {}p — {}p will be downloaded.",
        "Այս տեսանյութը {}p-ից բարձր չեն տալիս — կներբեռնվի {}p։";
    /// `{}` — высота кадра, которую источник вправду отдаёт.
    UiUpToHeight = "до {}p", "up to {}p", "մինչև {}p";
    /// `{}` — сколько осталось, «ч:мм:сс».
    UiTimeLeft = "осталось {}", "{} left", "մնաց {}";

    // Заголовки шагов экрана загрузки. Номер рядом с названием отвечает на
    // «с чего начинать» раньше, чем человек прочтёт подписи: три шага подряд
    // читаются последовательностью, а три равноправных заголовка — списком
    // настроек, в котором непонятно, что из этого обязательно.
    UiStepLink = "Ссылка на ролик", "The video link", "Տեսանյութի հղումը";
    UiStepWhat = "Что скачать", "What to download", "Ինչ ներբեռնել";
    UiStepWhere = "Куда сохранить", "Where to save", "Ուր պահպանել";
    UiLinkSourcesNote =
        "YouTube, VK Видео, Rutube, Vimeo, SoundCloud и ещё полторы тысячи сайтов. \
         Сверить, тот ли это ролик, можно будет прямо здесь — до загрузки.",
        "YouTube, VK Video, Rutube, Vimeo, SoundCloud and fifteen hundred more sites. \
         You will be able to check that this is the right video right here — before \
         downloading.",
        "YouTube, VK Video, Rutube, Vimeo, SoundCloud և ևս հազար հինգ հարյուր կայք։ \
         Ստուգել՝ արդյոք դա ճիշտ տեսանյութն է, կարելի կլինի հենց այստեղ՝ մինչև \
         ներբեռնումը։";
    UiFoundVideo = "Ролик найден", "The video was found", "Տեսանյութը գտնվեց";
    UiQualityMaxNote =
        "«Макс.» берёт лучшее, что отдаёт сайт. Ступень ниже пригодится на медленном \
         интернете и когда мало места на диске.",
        "“Max.” takes the best the site gives. A step lower is useful on slow internet \
         and when there is little room on the disk.",
        "«Մաքս․»-ը վերցնում է լավագույնը, ինչ տալիս է կայքը։ Ավելի ցածր աստիճանը պետք \
         կգա դանդաղ ինտերնետի և սկավառակի քիչ տեղի դեպքում։";
    UiEmbedNote =
        "Название, автор и картинка окажутся внутри файла — плеер покажет их вместо \
         голого имени. Ничего не отмечено — файл скачается как есть.",
        "The title, author and picture will end up inside the file — a player will show \
         them instead of a bare name. Nothing ticked — the file is downloaded as is.",
        "Վերնագիրը, հեղինակը և նկարը կհայտնվեն ֆայլի ներսում՝ նվագարկիչը դրանք ցույց \
         կտա մերկ անվան փոխարեն։ Ոչինչ նշված չէ՝ ֆայլը ներբեռնվում է ինչպես կա։";
    UiQueueButtonNote =
        "Отложит ссылку и освободит поле",
        "Sets the link aside and frees the field",
        "Կհետաձգի հղումը և կազատի դաշտը";
    UiDownloadButtonNote = "Начнёт прямо сейчас", "Starts right now", "Կսկսի հենց հիմա";
    /// Причина, по которой «Скачать» выключена, — на виду, а не по наведению.
    /// Подсказка остаётся, но перестаёт быть единственным способом узнать её.
    UiDownloadNeedsLink =
        "Вставьте ссылку — и кнопка оживёт",
        "Paste a link and the button comes alive",
        "Տեղադրեք հղումը՝ և կոճակը կաշխատի";

    UiFormat = "Формат", "Format", "Ձևաչափ";
    UiEmbed = "Вшить", "Embed", "Ներկարել";
    UiSectionField = "Фрагмент", "Fragment", "Հատված";
    UiSectionFrom = "с 0:00", "from 0:00", "0:00-ից";
    UiSectionTo = "до конца", "to the end", "մինչև վերջ";
    UiSectionHint =
        "Пусто — ролик скачается целиком. Время можно писать как «90», «1:30» или «1:02:03».",
        "Empty means the whole video. Time can be written as “90”, “1:30” or “1:02:03”.",
        "Դատարկը նշանակում է ամբողջ տեսանյութը։ Ժամանակը կարելի է գրել «90», «1:30» կամ \
         «1:02:03» տեսքով։";
    UiSectionNoFfmpeg =
        "Вырезать нечем: ffmpeg не найден. Ролик скачается целиком.",
        "There is nothing to cut with: ffmpeg was not found. The whole video will be \
         downloaded.",
        "Կտրելու բան չկա. ffmpeg չգտնվեց։ Տեսանյութը կներբեռնվի ամբողջությամբ։";
    UiSectionStreamNote =
        "Фрагмент вырезает ffmpeg прямо по ходу загрузки: она идёт заметно медленнее \
         обычной, а проценты и скорость при этом не показываются. У MP4 начало \
         сдвигается к ближайшему ключевому кадру — файл может начаться на \
         секунду-другую раньше запрошенного.",
        "ffmpeg cuts the fragment as the download goes: it runs noticeably slower than \
         usual, and percentages and speed are not shown. With MP4 the start shifts to the \
         nearest keyframe — the file may begin a second or two earlier than requested.",
        "Հատվածը ffmpeg-ը կտրում է հենց ներբեռնման ընթացքում. այն ընթանում է զգալիորեն \
         ավելի դանդաղ, և տոկոսներն ու արագությունը ցույց չեն տրվում։ MP4-ի դեպքում սկիզբը \
         տեղաշարժվում է դեպի մոտակա հիմնական կադրը՝ ֆայլը կարող է սկսվել պահանջվածից \
         մեկ-երկու վայրկյան շուտ։";
    UiSectionCutAfterNote =
        "Фрагмент занимает больше половины ролика, и Savio возьмёт его быстрым путём: \
         скачает ролик целиком, вырежет кусок и оставит на диске только его. Проценты \
         и скорость при этом видны, зато из сети придёт целый ролик. У MP4 начало \
         сдвигается к ближайшему ключевому кадру — файл может начаться на \
         секунду-другую раньше запрошенного.",
        "The fragment takes up more than half of the video, so Savio will take it the fast \
         way: download the whole video, cut the piece out and keep only it on disk. \
         Percentages and speed are visible, but the whole video comes over the network. \
         With MP4 the start shifts to the nearest keyframe — the file may begin a second \
         or two earlier than requested.",
        "Հատվածը զբաղեցնում է տեսանյութի կեսից ավելին, և Savio-ն այն կվերցնի արագ \
         ճանապարհով՝ կներբեռնի տեսանյութն ամբողջությամբ, կկտրի կտորը և սկավառակին կթողնի \
         միայն այն։ Տոկոսներն ու արագությունը տեսանելի են, բայց ցանցից կգա ամբողջ \
         տեսանյութը։ MP4-ի դեպքում սկիզբը տեղաշարժվում է դեպի մոտակա հիմնական կադրը՝ \
         ֆայլը կարող է սկսվել պահանջվածից մեկ-երկու վայրկյան շուտ։";

    UiEmbedMetadata = "Метаданные", "Metadata", "Մետատվյալներ";
    UiEmbedMetadataHint =
        "Название, автор и дата уедут в сам файл.",
        "The title, author and date will go into the file itself.",
        "Անվանումը, հեղինակը և ամսաթիվը կգնան հենց ֆայլի մեջ։";
    UiEmbedThumbnail = "Обложку", "Cover art", "Շապիկը";
    UiEmbedThumbnailHint =
        "Картинка ролика станет обложкой файла.",
        "The video thumbnail will become the file cover.",
        "Տեսանյութի պատկերը կդառնա ֆայլի շապիկը։";
    UiEmbedSubs = "Субтитры", "Subtitles", "Ենթագրեր";
    UiEmbedSubsDisabled =
        "Субтитры бывают только у видео — выберите MP4.",
        "Subtitles only come with video — pick MP4.",
        "Ենթագրեր լինում են միայն տեսանյութի մոտ — ընտրեք MP4։";
    UiAutoSubs = "Можно автоматические", "Automatic will do", "Ավտոմատը նույնպես կլինի";
    UiAutoSubsHint =
        "Распознанные роботом субтитры лучше, чем никаких, но ошибки в них обычное дело.",
        "Subtitles recognised by a robot are better than none, but mistakes in them are \
         commonplace.",
        "Ռոբոտի ճանաչած ենթագրերն ավելի լավ են, քան ոչինչը, բայց սխալները դրանցում \
         սովորական բան են։";
    UiEmbedNoFfmpeg =
        "Вшивать нечем: ffmpeg не найден. Файл скачается, но без метаданных, обложки \
         и субтитров.",
        "There is nothing to embed with: ffmpeg was not found. The file will download, but \
         without metadata, cover art and subtitles.",
        "Ներկարելու բան չկա. ffmpeg չգտնվեց։ Ֆայլը կներբեռնվի, բայց առանց մետատվյալների, \
         շապիկի և ենթագրերի։";

    UiAdvanced = "Тонкие настройки", "Fine settings", "Նուրբ կարգավորումներ";
    UiSiteLogin = "Вход на сайт", "Site login", "Մուտք կայք";
    UiSubtitleLanguage = "Язык субтитров", "Subtitle language", "Ենթագրերի լեզու";
    UiSubsAutomatic = "Автоматические", "Automatic", "Ավտոմատ";
    UiSubsOwn = "Свои", "Authored", "Հեղինակային";
    UiSubsAutomaticHint =
        "Автоматические субтитры распознаёт робот: опечатки, слипшиеся слова и пропуски \
         в них обычное дело.",
        "Automatic subtitles are recognised by a robot: typos, run-together words and gaps \
         in them are commonplace.",
        "Ավտոմատ ենթագրերը ճանաչում է ռոբոտը. վրիպակները, կպած բառերը և բացթողումները \
         դրանցում սովորական բան են։";
    UiSubsOwnHint =
        "Свои субтитры выкладывает автор ролика, и они точные — но есть далеко не у каждого.",
        "Authored subtitles are posted by the video's author and they are accurate — but far \
         from every video has them.",
        "Հեղինակային ենթագրերը տեղադրում է տեսանյութի հեղինակը, և դրանք ճշգրիտ են — բայց \
         հեռու է, որ բոլորն ունենան։";
    UiCookiesWhy =
        "Для возрастных, приватных и «подтвердите, что вы не робот» роликов. У YouTube \
         cookies чаще мешают, чем помогают: сайт отвечает на них урезанным списком дорожек \
         — если ролик перестал скачиваться, верните «Не использовать».",
        "For age-restricted, private and “confirm you are not a robot” videos. With YouTube, \
         cookies more often hinder than help: the site answers them with a stripped-down \
         track list — if a video stopped downloading, set it back to “Do not use”.",
        "Տարիքային սահմանափակմամբ, մասնավոր և «հաստատեք, որ ռոբոտ չեք» տեսանյութերի համար։ \
         YouTube-ի մոտ cookies-ն ավելի հաճախ խանգարում է, քան օգնում. կայքը դրանց \
         պատասխանում է հոսքերի կրճատ ցանկով — եթե տեսանյութը դադարեց ներբեռնվել, \
         վերադարձրեք «Չօգտագործել»։";
    UiCookieFileWhy =
        "Нужен файл формата Netscape — такой выгружает расширение браузера вроде \
         «Get cookies.txt». Годится там, где сам браузер cookies не отдаёт.",
        "A file in the Netscape format is needed — a browser extension such as \
         “Get cookies.txt” saves one. It works where the browser itself does not give up \
         its cookies.",
        "Պետք է Netscape ձևաչափի ֆայլ — այդպիսին արտահանում է «Get cookies.txt» տիպի \
         բրաուզերի ընդլայնումը։ Պիտանի է այնտեղ, որտեղ բրաուզերն ինքը cookies չի տալիս։";
    UiCookieCloseBrowser =
        "Закройте браузер перед загрузкой: пока он открыт, файл cookies занят, и прочитать \
         его нельзя.",
        "Close the browser before downloading: while it is open the cookie file is busy and \
         cannot be read.",
        "Փակեք բրաուզերը ներբեռնումից առաջ. քանի դեռ այն բաց է, cookies ֆայլը զբաղված է, \
         և այն կարդալ հնարավոր չէ։";
    UiCookieSourceHint =
        "Откуда взять вход на сайт. Нажмите, чтобы выбрать другой файл.",
        "Where to take the site login from. Press to choose another file.",
        "Որտեղից վերցնել մուտքը կայք։ Սեղմեք՝ այլ ֆայլ ընտրելու համար։";
    UiOutDirHint =
        "Куда сохранять готовые файлы. Нажмите, чтобы выбрать другую папку.",
        "Where to save the finished files. Press to choose another folder.",
        "Ուր պահել պատրաստ ֆայլերը։ Սեղմեք՝ այլ թղթապանակ ընտրելու համար։";

    FilterCookieFiles = "Файлы cookies", "Cookie files", "Cookies ֆայլեր";
    FilterAllFiles = "Все файлы", "All files", "Բոլոր ֆայլերը";
    FilterSupported = "Поддерживаемые файлы", "Supported files", "Աջակցվող ֆայլեր";
    FilterTextFile = "Текстовый файл", "Text file", "Տեքստային ֆայլ";

    // Кнопки запуска и их причины отказа.
    /// Не «В очередь»: без подписи под кнопкой её путали со «Скачать» —
    /// оба глагола читались как «поехали», и разницу приходилось выяснять
    /// нажатием. Глагол «отложить» говорит про отсрочку сам.
    UiEnqueue = "Отложить в очередь", "Put in the queue", "Հետաձգել հերթ";
    UiEnqueueHint =
        "Ссылка встанет в конец очереди, а поле освободится под следующую.",
        "The link will go to the end of the queue and the field will free up for the next one.",
        "Հղումը կկանգնի հերթի վերջում, իսկ դաշտը կազատվի հաջորդի համար։";
    UiQueueFull = "Очередь заполнена: дождитесь, пока что-нибудь скачается.",
        "The queue is full: wait until something downloads.",
        "Հերթը լցված է. սպասեք, մինչև ինչ-որ բան ներբեռնվի։";
    UiPasteLinkForQueue =
        "Вставьте ссылку — она встанет в конец очереди.",
        "Paste a link — it will go to the end of the queue.",
        "Տեղադրեք հղում — այն կկանգնի հերթի վերջում։";
    UiFixSection = "Поправьте границы фрагмента.", "Fix the fragment bounds.",
        "Ուղղեք հատվածի սահմանները։";
    UiPickFolderFirst = "Сначала выберите папку сохранения.", "First choose a save folder.",
        "Նախ ընտրեք պահպանման թղթապանակ։";
    UiNeedYtdlp = "Сначала нужен yt-dlp.", "yt-dlp is needed first.", "Նախ պետք է yt-dlp։";
    UiQueueFullNote =
        "В очереди больше некуда: полсотни ссылок ещё ждут. Как только что-нибудь \
         скачается, место освободится.",
        "There is no more room in the queue: fifty links are still waiting. As soon as \
         something downloads, room will free up.",
        "Հերթում այլևս տեղ չկա. հիսուն հղում դեռ սպասում է։ Հենց ինչ-որ բան ներբեռնվի, \
         տեղ կազատվի։";
    UiCancel = "Отмена", "Cancel", "Չեղարկել";
    UiCancelHint =
        "Остановит идущую загрузку. Остальные ссылки останутся в очереди.",
        "Stops the download in progress. The other links will stay in the queue.",
        "Կկանգնեցնի ընթացող ներբեռնումը։ Մնացած հղումները կմնան հերթում։";
    UiPasteOrQueue =
        "Вставьте ссылку или поставьте что-нибудь в очередь.",
        "Paste a link, or put something in the queue.",
        "Տեղադրեք հղում կամ ինչ-որ բան դրեք հերթ։";
    UiDownload = "Скачать", "Download", "Ներբեռնել";
    UiOpenFolder = "Открыть папку", "Open the folder", "Բացել թղթապանակը";
    UiCancelledNote = "Загрузка отменена.", "The download was cancelled.",
        "Ներբեռնումը չեղարկված է։";
    UiQueuedNote =
        "Ссылки ждут в очереди. Нажмите «Скачать» — они пойдут сверху вниз, по одной.",
        "The links are waiting in the queue. Press “Download” — they will go top to bottom, \
         one at a time.",
        "Հղումները սպասում են հերթում։ Սեղմեք «Ներբեռնել» — դրանք կգնան վերևից ներքև, \
         մեկ առ մեկ։";
    UiIdleNote =
        "Здесь будет видно, как идёт загрузка: проценты, скорость, сколько \
         осталось — и путь к готовому файлу с кнопкой «Открыть папку».",
        "Here you will see how the download goes: per cent, speed, how much is \
         left — and the path to the finished file with an “Open folder” button.",
        "Այստեղ կերևա, թե ինչպես է ընթանում ներբեռնումը՝ տոկոսները, արագությունը, \
         որքան է մնացել, և պատրաստ ֆայլի ուղին՝ «Բացել պանակը» կոճակով։";

    UiClear = "Очистить", "Clear", "Մաքրել";
    UiClearHint =
        "Список опустеет: уйдут и скачанные, и те, что ещё ждут.",
        "The list will empty: both the downloaded ones and those still waiting will go.",
        "Ցանկը կդատարկվի. կգնան և՛ ներբեռնվածները, և՛ դեռ սպասողները։";
    UiQueueEmptyNote =
        "Очередь нужна, когда ссылок несколько. «Отложить в очередь» кладёт ссылку \
         сюда и очищает поле — так десяток ссылок набирается за полминуты. \
         Потом одно «Скачать», и Savio пройдёт список сверху вниз, пока вас нет.",
        "The queue is for when there are several links. “Put in the queue” places \
         the link here and clears the field — a dozen links are collected in half \
         a minute. Then one “Download”, and Savio walks the list top to bottom.",
        "Հերթը պետք է, երբ հղումները մի քանիսն են։ «Հետաձգել հերթ»-ը հղումը դնում \
         է այստեղ և մաքրում դաշտը՝ այսպես տասնյակ հղում հավաքվում է կես րոպեում։ \
         Հետո մեկ «Ներբեռնել», և Savio-ն կանցնի ցանկը վերևից ներքև։";
    UiQueueNote =
        "Качаются по одной, сверху вниз; сорвавшаяся не останавливает остальные. \
         Каждая ссылка уедет с теми настройками, которые стояли, когда её \
         отложили, — переключатели слева можно трогать.",
        "They download one at a time, top to bottom; one that fails does not stop \
         the rest. Each link leaves with the settings that were set when it was \
         put aside — the switches on the left can be touched.",
        "Ներբեռնվում են մեկ առ մեկ, վերևից ներքև. ձախողվածը մնացածը չի կանգնեցնում։ \
         Յուրաքանչյուր հղում կմեկնի այն կարգավորումներով, որոնք դրված էին \
         հետաձգելու պահին՝ ձախի փոխարկիչներին կարելի է դիպչել։";
    UiHistoryEmptyNote =
        "Пока пусто. Сюда попадёт всё, что вы скачаете за этот запуск.",
        "Empty for now. Everything you download in this run will land here.",
        "Առայժմ դատարկ է։ Այստեղ կհայտնվի այն ամենը, ինչ ներբեռնեք այս աշխատանքի ընթացքում։";
    UiRemoveFromQueue = "Убрать из очереди", "Remove from the queue", "Հեռացնել հերթից";

    UiCopy = "Скопировать", "Copy", "Պատճենել";
    UiCopyHint =
        "Журнал уйдёт в буфер обмена — его можно вставить в сообщение о проблеме.",
        "The log will go to the clipboard — you can paste it into a problem report.",
        "Մատյանը կգնա փոխանակման բուֆեր — այն կարելի է տեղադրել խնդրի մասին հաղորդագրության մեջ։";
    UiCopied = "Скопировано", "Copied", "Պատճենված է";
    UiLog = "Журнал", "Log", "Մատյան";
    UiLogEmpty = "Пока нечего показывать.", "Nothing to show yet.", "Առայժմ ցույց տալու բան չկա։";

    // Шапка и подвал.
    UiUpdateEngine = "Обновить движок", "Update the engine", "Թարմացնել շարժիչը";
    UiUpdateEngineHint =
        "Сайты меняются, и старый yt-dlp перестаёт их скачивать. Обновление занимает \
         несколько секунд.",
        "Sites change, and an old yt-dlp stops downloading from them. The update takes \
         a few seconds.",
        "Կայքերը փոխվում են, և հին yt-dlp-ն դադարում է դրանցից ներբեռնել։ Թարմացումը \
         տևում է մի քանի վայրկյան։";
    UiUpdateFfmpeg = "Обновить ffmpeg", "Update ffmpeg", "Թարմացնել ffmpeg-ը";
    UiUpdateFfmpegHint =
        "Свежая сборка ffmpeg качается целиком — больше сотни мегабайт.",
        "A fresh ffmpeg build downloads in full — more than a hundred megabytes.",
        "ffmpeg-ի թարմ հավաքածուն ներբեռնվում է ամբողջությամբ՝ հարյուր մեգաբայթից ավելի։";
    UiAbout = "О программе", "About", "Ծրագրի մասին";
    UiAboutHint =
        "Кто сделал Savio, куда писать об ошибках и что изменилось в этой версии.",
        "Who made Savio, where to report bugs and what changed in this version.",
        "Ով է ստեղծել Savio-ն, ուր գրել սխալների մասին և ինչ է փոխվել այս տարբերակում։";
    UiAboutVersionHint =
        "О программе: кто сделал Savio и куда писать, если что-то не так.",
        "About: who made Savio and where to write if something is wrong.",
        "Ծրագրի մասին. ով է ստեղծել Savio-ն և ուր գրել, եթե ինչ-որ բան այն չէ։";
    /// Подсказка у таблетки языка в шапке. Про системные диалоги сказано
    /// здесь намеренно: выбор папки и файла рисует система, и её язык Savio
    /// не меняет — молчаливое расхождение выглядело бы недоделкой.
    UiLanguageHint =
        "Язык окна Savio. Диалоги выбора файла и папки рисует система — они \
         останутся на её языке.",
        "The language of the Savio window. File and folder dialogs are drawn by the system \
         and stay in its language.",
        "Savio-ի պատուհանի լեզուն։ Ֆայլի և թղթապանակի ընտրության պատուհանները նկարում է \
         համակարգը՝ դրանք կմնան իր լեզվով։";
    // Экран приветствия при первом запуске. Тексты из макета v2, но две
    // строки в нём ссылались на шапку окна, которой в этой же версии не
    // стало: приветствие — первое, что читает человек, и отправлять его
    // к тому, чего нет, хуже всего именно здесь.
    WelcomeTitle =
        "Скачивает видео и звук по ссылке. Три шага — и файл на диске.",
        "Downloads video and audio by link. Three steps, and the file is on your disk.",
        "Ներբեռնում է տեսանյութ և ձայն հղումով։ Երեք քայլ՝ և ֆայլը սկավառակի վրա է։";
    WelcomeNote =
        "Остальные разделы Savio — в рельсе слева. Ими можно не пользоваться вовсе: загрузка \
         работает сама по себе.",
        "Savio's other sections are in the rail on the left. You need not use them at all: \
         downloading works on its own.",
        "Savio-ի մնացած բաժինները՝ ձախ ռելսում։ Դրանցից կարելի է ընդհանրապես չօգտվել՝ ներբեռնումն \
         ինքնուրույն է աշխատում։";
    WelcomeStepLinkTitle = "Вставьте ссылку", "Paste a link", "Տեղադրեք հղումը";
    WelcomeStepLinkNote =
        "Через секунду под полем появятся название, автор и обложка — сверьтесь, что это тот \
         самый ролик.",
        "In a second the title, author and cover appear under the field — check that this is the \
         right video.",
        "Մեկ վայրկյանից դաշտի տակ կհայտնվեն վերնագիրը, հեղինակը և շապիկը՝ համոզվեք, որ սա հենց \
         այն տեսանյութն է։";
    WelcomeStepPickTitle = "Выберите, что нужно", "Choose what you need", "Ընտրեք անհրաժեշտը";
    WelcomeStepPickNote =
        "MP4 — видео, MP3 — только звук. Качество по умолчанию максимальное: менять его не \
         обязательно.",
        "MP4 is video, MP3 is sound only. Quality is at maximum by default — you need not change \
         it.",
        "MP4-ը տեսանյութ է, MP3-ը՝ միայն ձայն։ Որակը լռելյայն առավելագույնն է՝ փոխելը պարտադիր չէ։";
    WelcomeStepGoTitle = "Нажмите «Скачать»", "Press “Download”", "Սեղմեք «Ներբեռնել»";
    WelcomeStepGoNote =
        "Ход загрузки и путь к готовому файлу — в правой колонке. Ссылок много? Откладывайте их в \
         очередь.",
        "Progress and the path to the finished file are in the right column. Many links? Set them \
         aside in the queue.",
        "Ներբեռնման ընթացքը և պատրաստ ֆայլի ուղին՝ աջ սյունակում։ Հղումները շա՞տ են՝ դրեք դրանք \
         հերթ։";
    WelcomeStart = "Начать", "Start", "Սկսել";
    WelcomeOnce =
        "Это окно больше не появится. Открыть его снова — кнопкой «?» внизу рельса.",
        "This window will not appear again. Open it later with the “?” button at the bottom of \
         the rail.",
        "Այս պատուհանն այլևս չի հայտնվի։ Կրկին բացելու համար՝ ռելսի ներքևի «?» կոճակը։";
    UiAboutButtonsNote =
        "«Показать приветствие» снова откроет экран с тремя шагами — тот же, что при первом \
         запуске. «Написать» откроет почтовую программу с номером версии в теме письма; если она \
         не настроена, помогут «Скопировать адрес».",
        "“Show the welcome screen” opens the three-step screen again — the same as on first run. \
         “Write” opens the mail program with the version number in the subject; if it is not set \
         up, “Copy the address” helps.",
        "«Ցույց տալ ողջույնը» կրկին կբացի երեք քայլով էկրանը՝ նույնը, ինչ առաջին գործարկման \
         ժամանակ։ «Գրել»-ը կբացի փոստային ծրագիրը՝ նամակի թեմայում տարբերակի համարով. եթե այն \
         կարգավորված չէ, կօգնի «Պատճենել հասցեն»-ը։";
    UiShowWelcome = "Показать приветствие", "Show the welcome screen", "Ցույց տալ ողջույնը";
    UiHelpButtonHint =
        "Открыть приветствие с тремя шагами — то же, что при первом запуске.",
        "Open the welcome screen with the three steps — the same as on first run.",
        "Բացել ողջույնի էկրանը երեք քայլով՝ նույնը, ինչ առաջին գործարկման ժամանակ։";

    UiThemeDark = "Тёмная", "Dark", "Մուգ";
    UiThemeLight = "Светлая", "Light", "Բաց";
    UiThemeHint =
        "Тема окна. Выбор запоминается вместе с языком и папкой сохранения.",
        "The window theme. The choice is remembered along with the language and the \
         save folder.",
        "Պատուհանի տեսքը։ Ընտրությունը հիշվում է լեզվի և պահպանման թղթապանակի հետ միասին։";

    UiSmooth = "Плавные переходы", "Smooth transitions", "Սահուն անցումներ";
    UiSmoothHint =
        "Снимите, если движение в окне мешает или машина слабая.",
        "Clear it if motion in the window gets in the way, or the machine is weak.",
        "Հանեք, եթե պատուհանում շարժումը խանգարում է կամ մեքենան թույլ է։";

    UiUpdatingEngineTitle = "Обновление движка", "Updating the engine", "Շարժիչի թարմացում";
    UiUpdatingEngineText =
        "Savio скачивает свежий yt-dlp. Это занимает несколько секунд.",
        "Savio is downloading a fresh yt-dlp. This takes a few seconds.",
        "Savio-ն ներբեռնում է թարմ yt-dlp։ Սա տևում է մի քանի վայրկյան։";
    UiUpdatingFfmpegTitle = "Обновление ffmpeg", "Updating ffmpeg", "ffmpeg-ի թարմացում";
    UiUpdatingFfmpegText =
        "Savio скачивает свежую сборку ffmpeg целиком — это больше сотни мегабайт, так что \
         на медленном канале придётся подождать.",
        "Savio is downloading a fresh ffmpeg build in full — that is more than a hundred \
         megabytes, so on a slow connection you will have to wait.",
        "Savio-ն ներբեռնում է ffmpeg-ի թարմ հավաքածուն ամբողջությամբ — սա հարյուր \
         մեգաբայթից ավելի է, այնպես որ դանդաղ կապի դեպքում պետք կլինի սպասել։";
    UiInstallingTitle = "Установка зависимостей", "Installing dependencies",
        "Կախվածությունների տեղակայում";
    UiInstallingText =
        "Savio догружает недостающие программы. Это делается один раз при первом запуске.",
        "Savio is fetching the missing programs. This is done once, at the first start.",
        "Savio-ն ներբեռնում է պակասող ծրագրերը։ Սա արվում է մեկ անգամ՝ առաջին մեկնարկի ժամանակ։";
    UiCancelButton = "Отменить", "Cancel", "Չեղարկել";

    // Сводка тонких настроек.
    UiSummarySectionBad = "фрагмент задан неверно", "the fragment is set wrong",
        "հատվածը սխալ է նշված";
    UiSummarySection = "фрагмент", "fragment", "հատված";
    UiSummaryLogin = "вход на сайт", "site login", "մուտք կայք";
    /// `{}` — код языка субтитров.
    UiSummarySubs = "субтитры: {}", "subtitles: {}", "ենթագրեր՝ {}";
    UiSummaryPlaceholder =
        "фрагмент, вход на сайт, язык субтитров", "fragment, site login, subtitle language",
        "հատված, մուտք կայք, ենթագրերի լեզու";

    /// `{}` — имя программы, `{}` — её версия.
    UiToolVersion = "{} — {}", "{} — {}", "{} — {}";
    /// `{}` — имя программы.
    UiToolVersionUnknown =
        "{} — версия неизвестна", "{} — version unknown", "{} — տարբերակն անհայտ է";
    UiToolMissing = "{} — не найден", "{} — not found", "{} — չի գտնվել";

    // Вкладка «Метаданные».
    UiMetaTitle = "Что файл рассказывает о вас", "What the file tells about you",
        "Ինչ է ֆայլը պատմում ձեր մասին";
    UiMetaNote =
        "Модель камеры, дата съёмки, координаты места, автор, обложка и десяток служебных \
         записей. Всё это уезжает вместе с файлом, когда вы отправляете его дальше.",
        "The camera model, the date taken, the coordinates of the place, the author, the \
         cover art and a dozen service records. All of it travels with the file when you \
         send it on.",
        "Ֆոտոխցիկի մոդելը, նկարահանման ամսաթիվը, վայրի կոորդինատները, հեղինակը, շապիկը և \
         մեկ տասնյակ ծառայողական գրառումներ։ Այս ամենը գնում է ֆայլի հետ, երբ ուղարկում եք \
         այն հետագա։";
    UiMetaPickHint =
        "Нажмите, чтобы выбрать MP3 или изображение.",
        "Press to choose an MP3 or an image.",
        "Սեղմեք՝ MP3 կամ պատկեր ընտրելու համար։";
    UiMetaPickFirst = "Сначала выберите файл.", "First choose a file.", "Նախ ընտրեք ֆայլ։";
    UiMetaRead = "Прочитать", "Read", "Կարդալ";
    UiMetaWipe = "Стереть всё", "Wipe it all", "Ջնջել ամենը";
    UiMetaHowTo =
        "Выберите MP3 или изображение. «Прочитать» покажет, что в нём записано, «Стереть \
         всё» уберёт это из самого файла.",
        "Choose an MP3 or an image. “Read” will show what is written in it, “Wipe it all” \
         will remove that from the file itself.",
        "Ընտրեք MP3 կամ պատկեր։ «Կարդալ»-ը ցույց կտա, թե ինչ է գրված դրանում, «Ջնջել \
         ամենը»-ն կհեռացնի դա հենց ֆայլից։";
    UiMetaFileTags = "Метаданные файла", "File metadata", "Ֆայլի մետատվյալներ";
    UiMetaNothingFound = "Метаданные не найдены.", "No metadata was found.",
        "Մետատվյալներ չեն գտնվել։";
    UiClose = "Закрыть", "Close", "Փակել";
    UiMetaOverwriteTitle = "Перезаписать файл?", "Overwrite the file?", "Վերագրե՞լ ֆայլը։";
    UiMetaOverwriteText =
        "Метаданные будут стёрты из самого файла, копия не создаётся. Вернуть стёртое будет \
         нельзя.",
        "The metadata will be wiped from the file itself; no copy is made. There will be no \
         way to bring back what is wiped.",
        "Մետատվյալները կջնջվեն հենց ֆայլից, պատճեն չի ստեղծվում։ Ջնջվածը վերադարձնել \
         հնարավոր չի լինի։";
    UiDelete = "Удалить", "Delete", "Ջնջել";
    UiMetaWillOverwrite = "Файл перезапишется", "The file will be overwritten",
        "Ֆայլը կվերագրվի";
    UiMetaWillOverwriteText =
        "Копия рядом не создаётся, вернуть стёртое будет нельзя — поэтому перед очисткой \
         Savio спрашивает подтверждение.",
        "No copy is made alongside, and there will be no way to bring back what is wiped — \
         so Savio asks for confirmation before cleaning.",
        "Կողքին պատճեն չի ստեղծվում, ջնջվածը վերադարձնել հնարավոր չի լինի — դրա համար \
         մաքրելուց առաջ Savio-ն հարցնում է հաստատում։";
    UiMetaSupported = "Что поддерживается", "What is supported", "Ինչ է աջակցվում";
    UiMetaTiffReadOnly = "TIFF — только чтение", "TIFF — read only", "TIFF — միայն ընթերցում";
    UiMetaVideoNotYet = "видео — пока нет", "video — not yet", "տեսանյութ — առայժմ ոչ";
    UiMetaNothingToWipe =
        "Удалять было нечего: метаданных в файле нет.",
        "There was nothing to remove: the file has no metadata.",
        "Ջնջելու բան չկար. ֆայլում մետատվյալներ չկան։";
    /// `{}` — сколько байт освободилось.
    UiMetaWiped =
        "Метаданные удалены, освобождено {}", "Metadata removed, {} freed",
        "Մետատվյալները ջնջված են, ազատվեց {}";

    // Окно «О программе».
    UiVersion = "Версия", "Version", "Տարբերակ";
    UiDeveloper = "Разработчик", "Developer", "Մշակող";
    UiLicense = "Лицензия", "License", "Լիցենզիա";
    UiFeedback = "Отзывы и ошибки", "Feedback and bugs", "Կարծիքներ և սխալներ";
    UiWrite = "Написать", "Write", "Գրել";
    UiWriteHint =
        "Откроет почтовую программу — с версией Savio в теме письма.",
        "Opens the mail program — with the Savio version in the subject.",
        "Կբացի փոստային ծրագիրը՝ նամակի թեմայում Savio-ի տարբերակով։";
    UiCopyAddress = "Скопировать адрес", "Copy the address", "Պատճենել հասցեն";
    UiProjectPage = "Страница проекта", "Project page", "Նախագծի էջ";
    UiWhatChanged = "Что изменилось", "What changed", "Ինչ է փոխվել";

    // Вкладка «Машина»: снимок и монитор.
    UiMachinePolling = "Опрос идёт, пока открыт этот раздел",
        "Polling runs while this section is open", "Հարցումն ընթանում է, քանի դեռ բաժինը բաց է";
    UiSystemNoAnswer =
        "Опрос не дал ответа. Попробуйте «Проверить снова».",
        "The poll gave no answer. Try “Check again”.",
        "Հարցումը պատասխան չտվեց։ Փորձեք «Ստուգել կրկին»։";
    UiSystemAbout = "Сведения о железе этой машины.", "Details about this machine's hardware.",
        "Տեղեկություններ այս մեքենայի սարքավորումների մասին։";
    UiSystemNote =
        "Показано то, что система отдаёт без прав администратора. Температур, оборотов \
         вентиляторов и SMART здесь нет: честно получить их без элевации нельзя.",
        "What is shown is what the system gives without administrator rights. Temperatures, \
         fan speeds and SMART are not here: they cannot be obtained honestly without \
         elevation.",
        "Ցուցադրված է այն, ինչ համակարգը տալիս է առանց ադմինիստրատորի իրավունքների։ \
         Ջերմաստիճաններ, օդափոխիչների պտույտներ և SMART այստեղ չկան. դրանք ազնվորեն \
         ստանալ առանց բարձրացման հնարավոր չէ։";
    UiCheckAgain = "Проверить снова", "Check again", "Ստուգել կրկին";
    UiSaveReport = "Сохранить отчёт…", "Save the report…", "Պահպանել հաշվետվությունը…";
    UiWaitForPoll = "Сначала дождитесь опроса.", "First wait for the poll.",
        "Նախ սպասեք հարցմանը։";
    UiReportFileName = "savio-система.txt", "savio-system.txt", "savio-համակարգ.txt";
    /// `{}` — путь к сохранённому файлу.
    UiReportSaved = "Отчёт сохранён: {}", "The report was saved: {}",
        "Հաշվետվությունը պահվեց՝ {}";
    /// `{}` — то, что сказала система.
    UiReportSaveFailed =
        "Не удалось сохранить отчёт: {}", "Could not save the report: {}",
        "Չհաջողվեց պահպանել հաշվետվությունը՝ {}";
    UiSystemValueMissing =
        "Система не сообщила это значение.", "The system did not report this value.",
        "Համակարգն այս արժեքը չհաղորդեց։";

    UiMonitorWarmingUp =
        "Замеряю… Первые числа появятся через секунду: загрузка — это разница между двумя \
         замерами.",
        "Measuring… The first numbers will appear in a second: load is the difference \
         between two samples.",
        "Չափում եմ… Առաջին թվերը կհայտնվեն մեկ վայրկյանից. բեռնվածությունը երկու \
         չափումների տարբերությունն է։";
    UiMonitorMeasuring = "Замеряю…", "Measuring…", "Չափում եմ…";
    UiRefresh = "Обновить", "Refresh", "Թարմացնել";
    UiWaitForSystem = "Сначала дождитесь ответа системы.", "First wait for the system's answer.",
        "Նախ սպասեք համակարգի պատասխանին։";
    UiMonitorNote =
        "Показания снимаются раз в секунду, пока открыта эта половина раздела. Ушли \
         отсюда — опрос останавливается, и ноутбук не греется зря.",
        "Readings are taken once a second while this half of the section is open. Leave it, \
         and the polling stops — the laptop does not heat up for nothing.",
        "Ցուցմունքները վերցվում են վայրկյանը մեկ, քանի դեռ բաժնի այս կեսը բաց է։ Հեռացաք \
         այստեղից — հարցումը կանգնում է, և նոութբուքը իզուր չի տաքանում։";
    UiMonitorNoGpuLoad =
        "Загрузки видеокарты здесь нет: система отдаёт её только с правами администратора.",
        "There is no graphics card load here: the system gives it only with administrator \
         rights.",
        "Վիդեոքարտի բեռնվածությունն այստեղ չկա. համակարգը տալիս է այն միայն ադմինիստրատորի \
         իրավունքներով։";
    UiOverlay = "Оверлей поверх других окон", "Overlay on top of other windows",
        "Վերադիր այլ պատուհանների վրա";
    UiOverlayPassthrough =
        "Пропускать щелчки мыши сквозь оверлей", "Let mouse clicks pass through the overlay",
        "Թողնել մկնիկի սեղմումները վերադիրի միջով";
    UiOverlayFirst = "Сначала включите оверлей.", "First turn the overlay on.",
        "Նախ միացրեք վերադիրը։";
    UiOverlayNote =
        "Оверлей — обычное окно поверх остальных, и виден он только поверх окон. В играх \
         во весь экран его не будет: там кадр рисует сама игра.",
        "The overlay is an ordinary window on top of the rest, and it shows only over \
         windows. It will not be there in full-screen games: the game draws the frame itself.",
        "Վերադիրը սովորական պատուհան է մնացածների վրա, և այն երևում է միայն պատուհանների \
         վրա։ Լիաէկրան խաղերում այն չի լինի. այնտեղ կադրը նկարում է հենց խաղը։";
    UiOverlayTitle = "Savio — монитор", "Savio — monitor", "Savio — մոնիտոր";
    UiIo = "Ввод-вывод", "Input and output", "Մուտք-ելք";
    UiNetwork = "Сеть", "Network", "Ցանց";
    UiDisks = "Диски", "Disks", "Սկավառակներ";
    UiProcesses = "Процессы", "Processes", "Գործընթացներ";
    UiNoProcesses = "Список процессов система не отдала.",
        "The system did not give the process list.", "Համակարգը գործընթացների ցանկը չտվեց։";
    UiProcessesNote =
        "Сверху те, кто занимает процессор. Доля уже поделена на число ядер, так что сто \
         процентов — это вся машина.",
        "At the top are those taking up the processor. The share is already divided by the \
         number of cores, so a hundred per cent is the whole machine.",
        "Վերևում նրանք են, ովքեր զբաղեցնում են պրոցեսորը։ Բաժինն արդեն բաժանված է \
         միջուկների թվի վրա, այնպես որ հարյուր տոկոսը ամբողջ մեքենան է։";
    UiOverlayCpu = "ЦП", "CPU", "ՊՐ";
    UiOverlayRam = "ОЗУ", "RAM", "ՕՊ";
    UiOverlayDisk = "Диск", "Disk", "Սկավառակ";

    // Карточка «Питание».
    UiPower = "Питание", "Power", "Սնուցում";
    UiPowerAsking = "Спрашиваю систему…", "Asking the system…", "Հարցնում եմ համակարգին…";
    UiPowerPlan = "Схема электропитания", "Power plan", "Սնուցման սխեմա";
    UiPowerMode = "Режим питания", "Power mode", "Սնուցման ռեժիմ";
    UiPowerUnknownMode =
        "Машина работает в режиме, которого Savio не знает, — переключатель показывает \
         не всё.",
        "The machine runs in a mode Savio does not know — the switch does not show everything.",
        "Մեքենան աշխատում է Savio-ին անհայտ ռեժիմում — փոխարկիչը ամեն ինչ չի ցույց տալիս։";
    UiPowerBalancedFallback = "Сбалансированная", "Balanced", "Հավասարակշռված";
    UiPowerOtherPlan = "другая схема", "another plan", "այլ սխեմա";
    UiPowerModeUnnamed = "система его не назвала", "the system did not name it",
        "համակարգը այն չանվանեց";
    /// `{}` — запомненный режим, `{}` — действующий, `{}` — «Сбалансированная»,
    /// `{}` — название активной схемы.
    UiPowerModeIgnoredHint =
        "Windows запомнила режим «{}», но машина работает в другом: {}. Режим питания \
         применяется только при схеме «{}», а сейчас активна {}.",
        "Windows remembered the “{}” mode, but the machine runs in another one: {}. The \
         power mode only applies under the “{}” plan, and right now {} is active.",
        "Windows-ը հիշեց «{}» ռեժիմը, բայց մեքենան աշխատում է այլ ռեժիմում՝ {}։ Սնուցման \
         ռեժիմը կիրառվում է միայն «{}» սխեմայի դեպքում, իսկ հիմա ակտիվ է {}-ը։";
    /// `{}` — название активной схемы, `{}` — «Сбалансированная».
    UiPowerWrongPlanHint =
        "Сейчас активна {}, а режим питания Windows применяет только при «{}»: выбор она \
         запомнит, но машина будет работать по-прежнему.",
        "Right now {} is active, and Windows applies the power mode only under “{}”: it will \
         remember the choice, but the machine will keep working as before.",
        "Հիմա ակտիվ է {}-ը, իսկ սնուցման ռեժիմը Windows-ը կիրառում է միայն «{}»-ի դեպքում. \
         ընտրությունը կհիշի, բայց մեքենան կաշխատի նախկինի պես։";

    // Вкладка «Погода».
    UiWeatherNoPlace = "Место не выбрано", "No place chosen", "Վայրն ընտրված չէ";
    UiWaitForAnswer = "Сначала дождитесь ответа.", "First wait for the answer.",
        "Նախ սպասեք պատասխանին։";
    UiPickPlaceFirst = "Сначала выберите место.", "First choose a place.", "Նախ ընտրեք վայր։";
    UiWeatherNoPlaceNote =
        "Место ещё не определено. Найдите свой город поиском ниже или нажмите «Определить \
         по IP».",
        "The place has not been worked out yet. Find your city with the search below, or \
         press “Locate by IP”.",
        "Վայրը դեռ որոշված չէ։ Գտեք ձեր քաղաքը ներքևի որոնմամբ կամ սեղմեք «Որոշել ըստ IP»։";
    UiWeatherLocatedNote =
        "Место определено по IP-адресу — с точностью до города. Через VPN это будет чужая \
         страна: тогда найдите свой город поиском.",
        "The place was worked out from the IP address — accurate to the city. Through a VPN \
         it will be someone else's country: find your city with the search then.",
        "Վայրը որոշվել է ըստ IP-հասցեի՝ քաղաքի ճշտությամբ։ VPN-ի միջոցով սա կլինի օտար \
         երկիր. այդ դեպքում գտեք ձեր քաղաքը որոնմամբ։";
    UiWeatherFindCity = "Найти город", "Find a city", "Գտնել քաղաք";
    UiWeatherFind = "Найти", "Find", "Գտնել";
    UiWeatherSearchHint = "Например, Ереван", "For example, Yerevan", "Օրինակ՝ Երևան";
    UiWeatherSearching = "Ищу…", "Searching…", "Փնտրում եմ…";
    UiWeatherTwoLetters = "Наберите хотя бы две буквы названия.",
        "Type at least two letters of the name.", "Մուտքագրեք անվան առնվազն երկու տառ։";
    /// `{}` — то, что искали.
    UiWeatherNothingFound =
        "По запросу «{}» ничего не нашлось. Проверьте написание — и ищите город, а не улицу.",
        "Nothing was found for “{}”. Check the spelling — and look for a city, not a street.",
        "«{}» հարցմամբ ոչինչ չգտնվեց։ Ստուգեք ուղղագրությունը — և փնտրեք քաղաք, ոչ թե փողոց։";
    UiWeatherLocateByIp = "Определить по IP", "Locate by IP", "Որոշել ըստ IP";
    UiWeatherLocateHint =
        "Спросит у ipwho.is (запасной — ipapi.co), где находится ваш IP-адрес.",
        "Asks ipwho.is (with ipapi.co as a fallback) where your IP address is.",
        "Կհարցնի ipwho.is-ին (պահեստայինը՝ ipapi.co), թե որտեղ է ձեր IP-հասցեն։";
    UiWeatherInFavorites = "В избранном", "In favourites", "Ընտրյալներում";
    UiWeatherToFavorites = "В избранное", "To favourites", "Ընտրյալներ";
    UiWeatherFavoritesFull =
        "В избранном нет места: уберите оттуда одно из мест ниже.",
        "There is no room in favourites: remove one of the places below from it.",
        "Ընտրյալներում տեղ չկա. հեռացրեք այնտեղից ներքևի վայրերից մեկը։";
    UiWeatherRemoveFavorite = "Нажмите, чтобы убрать это место из избранного.",
        "Press to remove this place from favourites.",
        "Սեղմեք՝ այս վայրը ընտրյալներից հեռացնելու համար։";
    UiWeatherAddFavorite =
        "Место появится в списке ниже: переключаться между ними — один щелчок.",
        "The place will appear in the list below: switching between them is one click.",
        "Վայրը կհայտնվի ներքևի ցանկում. դրանց միջև անցնելը մեկ սեղմում է։";
    UiWeatherFavorites = "Избранное", "Favourites", "Ընտրյալներ";
    UiWeatherNoAir =
        "Сведения о качестве воздуха не пришли — на прогноз это не влияет.",
        "The air quality data did not arrive — this does not affect the forecast.",
        "Օդի որակի տվյալները չեկան — սա կանխատեսման վրա չի ազդում։";
    UiWeatherAirQuality = "Качество воздуха", "Air quality", "Օդի որակ";
    UiWeatherTwoDays = "Ближайшие двое суток", "The next two days", "Առաջիկա երկու օրը";
    UiWeatherNoHours = "Почасового прогноза в ответе сервера нет.",
        "There is no hourly forecast in the server's answer.",
        "Սերվերի պատասխանում ժամային կանխատեսում չկա։";
    UiWeatherWeek = "Неделя", "The week", "Շաբաթ";
    UiWeatherNoDays = "Прогноза по дням в ответе сервера нет.",
        "There is no daily forecast in the server's answer.",
        "Սերվերի պատասխանում օրական կանխատեսում չկա։";
    UiWeatherUnits = "Единицы", "Units", "Միավորներ";
    UiWeatherTemperature = "Температура", "Temperature", "Ջերմաստիճան";
    UiWeatherValueMissing = "Сервер погоды не сообщил это значение.",
        "The weather server did not report this value.", "Եղանակի սերվերն այս արժեքը չհաղորդեց։";

    // Экран «Телефон».
    UiShareOff = "Выключена", "Off", "Անջատված է";
    UiShareStarting = "Запускается", "Starting", "Գործարկվում է";
    UiShareOn = "Раздача идёт", "Sharing is on", "Բաշխումն ընթանում է";
    UiShareNote =
        "Телефон и компьютер — в одной сети Wi-Fi. Savio откроет страницу, которую телефон \
         увидит в браузере: с неё забирают файлы и на неё же их кладут.",
        "The phone and the computer are on the same Wi-Fi network. Savio will open a page \
         the phone sees in its browser: files are taken from it and put onto it.",
        "Հեռախոսն ու համակարգիչը նույն Wi-Fi ցանցում են։ Savio-ն կբացի էջ, որը հեռախոսը \
         կտեսնի բրաուզերում. այնտեղից վերցնում են ֆայլերը և այնտեղ էլ դնում։";
    // Два направления словами. Абзац `UiShareNote` говорит про обмен вообще,
    // и из него не видно, что передача идёт в обе стороны: половина людей
    // так и не догадывалась, что с телефона можно отправить.
    UiShareToPhone = "На телефон", "To the phone", "Հեռախոսին";
    UiShareToPhoneNote =
        "Файлы из папки ниже телефон видит списком и забирает нажатием.",
        "The phone sees the files from the folder below as a list and takes them with a tap.",
        "Ներքևի թղթապանակի ֆայլերը հեռախոսը տեսնում է ցանկով և վերցնում հպումով։";
    UiShareToComputer = "На компьютер", "To the computer", "Համակարգչին";
    UiShareToComputerNote =
        "То, что телефон отправит со страницы, ляжет в эту же папку.",
        "What the phone sends from the page will land in this same folder.",
        "Այն, ինչ հեռախոսը կուղարկի էջից, կհայտնվի այս նույն թղթապանակում։";

    // Три причины, по которым страница не открывается. Тот же текст приезжает
    // баннером после полуминуты тишины (`UiShareHelp`), но это уже после
    // неудачи — а список стоит до старта, когда его ещё можно прочесть.
    UiShareChecklist = "Проверьте до старта", "Check before starting", "Ստուգեք մինչ մեկնարկը";
    UiShareCheckNetwork =
        "Телефон в той же сети Wi-Fi",
        "The phone is on the same Wi-Fi",
        "Հեռախոսը նույն Wi-Fi ցանցում է";
    UiShareCheckNetworkNote =
        "Не в мобильном интернете и не в гостевой сети кафе — там устройства друг друга \
         не видят.",
        "Not on mobile internet and not on a cafe guest network — devices do not see each \
         other there.",
        "Ոչ բջջային ինտերնետում և ոչ սրճարանի հյուրային ցանցում՝ այնտեղ սարքերը միմյանց \
         չեն տեսնում։";
    UiShareCheckFirewall =
        "Savio разрешён в частных сетях",
        "Savio is allowed on private networks",
        "Savio-ն թույլատրված է մասնավոր ցանցերում";
    UiShareCheckFirewallNote =
        "Если система спрашивала, пускать ли его в сеть, — разрешите: иначе брандмауэр \
         закроет вход.",
        "If the system asked whether to let it onto the network, allow it: otherwise the \
         firewall closes the way in.",
        "Եթե համակարգը հարցրել է՝ թույլ տալ այն ցանց, թույլատրեք. հակառակ դեպքում \
         պատնեշը կփակի մուտքը։";
    UiShareCheckScreen =
        "Этот экран остаётся открытым",
        "This screen stays open",
        "Այս էկրանը մնում է բաց";
    UiShareStartNote =
        "Savio откроет порт и покажет QR-код с адресом — их и наводят камерой телефона.",
        "Savio will open a port and show a QR code with the address — that is what the \
         phone camera is pointed at.",
        "Savio-ն կբացի պորտ և ցույց կտա QR-կոդ հասցեով՝ հենց դրան են ուղղում հեռախոսի \
         տեսախցիկը։";
    UiShareOpenWarning =
        "Папка открыта для сети, пока вы на этом экране. Уйдёте на другой раздел или \
         закроете Savio — раздача остановится.",
        "The folder is open to the network while you are on this screen. Leave for another \
         section or close Savio, and sharing stops.",
        "Թղթապանակը բաց է ցանցի համար, քանի դեռ այս էկրանին եք։ Կգնաք այլ բաժին կամ \
         կփակեք Savio-ն՝ բաշխումը կկանգնի։";

    UiShareFolder = "Папка раздачи", "Shared folder", "Բաշխման թղթապանակ";
    UiShareChangeFolder = "Изменить", "Change", "Փոխել";
    UiShareOpenFolderHint = "Показать папку раздачи в проводнике.",
        "Show the shared folder in the file manager.",
        "Ցույց տալ բաշխման թղթապանակը ֆայլերի կառավարիչում։";
    UiSharePickFolderHint = "Нажмите, чтобы раздать другую папку.",
        "Press to share another folder.", "Սեղմեք՝ այլ թղթապանակ բաշխելու համար։";
    UiShareFolderLocked = "Папку раздачи меняют, когда раздача остановлена.",
        "The shared folder is changed while sharing is stopped.",
        "Բաշխման թղթապանակը փոխում են, երբ բաշխումը կանգնեցված է։";
    UiShareOwnFolderNote =
        "Отсюда телефон забирает файлы, сюда же кладёт свои. Вложенные папки не \
         раздаются — только файлы верхнего уровня.",
        "The phone takes files from here and puts its own here too. Nested folders are not \
         shared — only top-level files.",
        "Այստեղից հեռախոսը վերցնում է ֆայլերը, այստեղ էլ դնում իրենը։ Ներդիր թղթապանակները \
         չեն բաշխվում — միայն վերին մակարդակի ֆայլերը։";
    UiShareDefaultFolderNote =
        "Сейчас это папка сохранения загрузок. Отсюда телефон забирает файлы, сюда же \
         кладёт свои.",
        "Right now this is the download save folder. The phone takes files from here and \
         puts its own here too.",
        "Հիմա սա ներբեռնումների պահպանման թղթապանակն է։ Այստեղից հեռախոսը վերցնում է \
         ֆայլերը, այստեղ էլ դնում իրենը։";
    UiShareStop = "Остановить", "Stop", "Կանգնեցնել";
    UiShareStopHint = "Закроет страницу для телефона и оборвёт идущие передачи.",
        "Closes the page for the phone and breaks off the transfers in progress.",
        "Կփակի էջը հեռախոսի համար և կընդհատի ընթացող փոխանցումները։";
    UiShareStart = "Раздать файлы", "Share files", "Բաշխել ֆայլերը";
    UiSharePickFolderFirst = "Сначала выберите папку раздачи.",
        "First choose a shared folder.", "Նախ ընտրեք բաշխման թղթապանակ։";
    UiShareAutoStop =
        "Раздача остановится сама, если уйти с этого экрана или закрыть Savio.",
        "Sharing will stop by itself if you leave this screen or close Savio.",
        "Բաշխումն ինքն իրեն կկանգնի, եթե հեռանաք այս էկրանից կամ փակեք Savio-ն։";
    UiShareOpening = "Открываю порт и ищу адрес компьютера…",
        "Opening a port and looking for the computer's address…",
        "Բացում եմ պորտը և փնտրում համակարգչի հասցեն…";
    UiShareAddressForPhone = "Адрес для телефона", "Address for the phone", "Հասցե հեռախոսի համար";
    UiShareQrHint =
        "Наведите камеру телефона на код или наберите адрес в браузере телефона.",
        "Point the phone camera at the code, or type the address into the phone's browser.",
        "Ուղղեք հեռախոսի տեսախցիկը կոդի վրա կամ մուտքագրեք հասցեն հեռախոսի բրաուզերում։";
    UiShareComputerAddress = "Адрес компьютера", "The computer's address", "Համակարգչի հասցեն";
    UiShareAddressesHint =
        "Адресов несколько: VPN, WSL и виртуальные машины заводят свои. Нужен тот, что \
         в одной сети с телефоном.",
        "There are several addresses: a VPN, WSL and virtual machines add their own. You \
         need the one on the same network as the phone.",
        "Հասցեները մի քանիսն են. VPN-ը, WSL-ը և վիրտուալ մեքենաները ստեղծում են իրենցը։ \
         Պետք է այն, որը հեռախոսի հետ նույն ցանցում է։";
    UiShareWaiting = "Ждём телефон…", "Waiting for the phone…", "Սպասում ենք հեռախոսին…";
    UiShareVisitors = "Подключились", "Connected", "Միացան";
    UiShareTransfers = "Передачи", "Transfers", "Փոխանցումներ";
    UiShareNoTransfers =
        "Пока ничего не передавалось. Здесь появится каждый файл: куда он идёт, сколько \
         прошло и чем кончилось.",
        "Nothing has been transferred yet. Every file will appear here: where it is going, \
         how much has passed and how it ended.",
        "Առայժմ ոչինչ չի փոխանցվել։ Այստեղ կհայտնվի ամեն ֆայլ՝ ուր է գնում, որքան է անցել \
         և ինչով ավարտվեց։";
    /// Объяснение для того, к кому телефон так и не подключился. Savio узнать
    /// это сам не может: подключение к своему же адресу идёт мимо брандмауэра.
    UiShareHelp =
        "Страница не открывается на телефоне? Проверьте, что телефон в той же сети Wi-Fi, \
         что и компьютер, — не в мобильном интернете и не в гостевой сети. Если система \
         спрашивала, пускать ли Savio в сеть, — разрешите (в Windows — для частных сетей): \
         брандмауэр мог закрыть вход, а в сети с профилем «Общедоступная» входящие \
         подключения закрыты всегда. В гостевых сетях кафе и отелей устройства друг друга \
         не видят вовсе — там поможет только другая сеть, например точка доступа на самом \
         телефоне.",
        "The page does not open on the phone? Check that the phone is on the same Wi-Fi \
         network as the computer — not on mobile internet and not on a guest network. If \
         the system asked whether to let Savio onto the network, allow it (on Windows — for \
         private networks): the firewall may have closed the way in, and on a network with \
         the “Public” profile incoming connections are always closed. On guest networks in \
         cafes and hotels devices do not see each other at all — there only another network \
         helps, for example a hotspot on the phone itself.",
        "Էջը հեռախոսում չի՞ բացվում։ Ստուգեք, որ հեռախոսը նույն Wi-Fi ցանցում է, ինչ \
         համակարգիչը — ոչ բջջային ինտերնետում և ոչ հյուրի ցանցում։ Եթե համակարգը հարցրել է, \
         թույլ տա՞լ Savio-ին ցանց — թույլատրեք (Windows-ում՝ մասնավոր ցանցերի համար). \
         բրանդմաուերը կարող էր փակել մուտքը, իսկ «Հանրային» պրոֆիլով ցանցում մուտքային \
         միացումները միշտ փակ են։ Սրճարանների և հյուրանոցների հյուրի ցանցերում սարքերն \
         իրար ընդհանրապես չեն տեսնում — այնտեղ կօգնի միայն այլ ցանց, օրինակ՝ հենց \
         հեռախոսի մուտքի կետը։";
    UiShareStoppedUnfinished =
        "Раздача остановлена — передача не закончена.",
        "Sharing was stopped — the transfer is not finished.",
        "Բաշխումը կանգնեցվել է — փոխանցումն ավարտված չէ։";
    UiShareLeftScreen =
        "Раздача остановлена: вы ушли с экрана «Телефон». Папка больше не открыта для сети.",
        "Sharing was stopped: you left the “Phone” screen. The folder is no longer open to \
         the network.",
        "Բաշխումը կանգնեցվել է. դուք հեռացաք «Հեռախոս» էկրանից։ Թղթապանակն այլևս բաց չէ \
         ցանցի համար։";
    UiShareDied = "Раздача прекратилась. Запустите её заново.",
        "Sharing has ended. Start it again.", "Բաշխումը դադարեց։ Գործարկեք այն կրկին։";
    /// `{}` — направление передачи, `{}` — размер файла.
    UiShareTransferDone = "{} · готово · {}", "{} · done · {}", "{} · պատրաստ է · {}";
    UiOpen = "Открыть", "Open", "Բացել";

    // -----------------------------------------------------------------------
    // Установка и обновление инструментов
    // -----------------------------------------------------------------------

    SetupNoToolsDir =
        "Не удалось определить папку для инструментов: не задана домашняя папка.",
        "Could not work out the folder for the tools: no home folder is set.",
        "Չհաջողվեց որոշել գործիքների թղթապանակը. տնային թղթապանակը սահմանված չէ։";
    /// `{}` — путь к папке, `{}` — то, что сказала система.
    SetupMkdirFailed =
        "Не удалось создать папку {}: {}", "Could not create the folder {}: {}",
        "Չհաջողվեց ստեղծել {} թղթապանակը՝ {}";
    /// `{}` — причина отказа.
    SetupFfmpegFailed =
        "Не удалось установить ffmpeg: {}. Склейка видео со звуком и конвертация в MP3 \
         работать не будут.",
        "Could not install ffmpeg: {}. Merging video with audio and converting to MP3 will \
         not work.",
        "Չհաջողվեց տեղակայել ffmpeg-ը՝ {}։ Տեսանյութի ու ձայնի միացումը և MP3 փոխարկումը \
         չեն աշխատի։";

    StageFindingYtdlp =
        "Ищу свежий выпуск yt-dlp…", "Looking for the latest yt-dlp release…",
        "Փնտրում եմ yt-dlp-ի թարմ թողարկումը…";
    StageDownloadingYtdlp = "Скачиваю yt-dlp…", "Downloading yt-dlp…", "Ներբեռնում եմ yt-dlp-ն…";
    StageDownloadingFfmpeg = "Скачиваю ffmpeg…", "Downloading ffmpeg…", "Ներբեռնում եմ ffmpeg-ը…";
    StageExtractingFfmpeg =
        "Распаковываю ffmpeg…", "Extracting ffmpeg…", "Բացում եմ ffmpeg-ի արխիվը…";

    /// `{}` — номер выпуска.
    LogYtdlpRelease = "yt-dlp: выпуск {}", "yt-dlp: release {}", "yt-dlp՝ թողարկում {}";
    /// `{}` — путь к установленному файлу.
    LogYtdlpInstalled =
        "yt-dlp установлен: {}", "yt-dlp installed: {}", "yt-dlp-ը տեղակայված է՝ {}";
    LogFfmpegInstalled =
        "ffmpeg установлен: {}", "ffmpeg installed: {}", "ffmpeg-ը տեղակայված է՝ {}";
    /// `{}` — номер источника по порядку, `{}` — причина.
    LogFfmpegSourceFailed =
        "Источник ffmpeg №{} не сработал: {}", "ffmpeg source no. {} did not work: {}",
        "ffmpeg-ի աղբյուր №{}-ը չաշխատեց՝ {}";
    /// `{}` — причина недоступности.
    LogSumsUnavailable =
        "Список контрольных сумм ffmpeg недоступен ({}) — ставлю без сверки.",
        "The ffmpeg checksum list is unavailable ({}) — installing without verification.",
        "ffmpeg-ի ստուգիչ գումարների ցանկն անհասանելի է ({}) — տեղակայում եմ առանց ստուգման։";
    /// `{}` — имя файла архива.
    LogNoSumLine =
        "В списке контрольных сумм ffmpeg нет строки для {} — ставлю без сверки.",
        "The ffmpeg checksum list has no line for {} — installing without verification.",
        "ffmpeg-ի ստուգիչ գումարների ցանկում {}-ի համար տող չկա — տեղակայում եմ առանց \
         ստուգման։";

    /// `{}` — то, что сказала сеть.
    SetupYtdlpSumsFailed =
        "Не удалось получить контрольные суммы yt-dlp: {}",
        "Could not fetch the yt-dlp checksums: {}",
        "Չհաջողվեց ստանալ yt-dlp-ի ստուգիչ գումարները՝ {}";
    /// `{}` — имя файла.
    SetupYtdlpNoSumLine =
        "В списке контрольных сумм yt-dlp нет строки для {}.",
        "The yt-dlp checksum list has no line for {}.",
        "yt-dlp-ի ստուգիչ գումարների ցանկում {}-ի համար տող չկա։";
    SetupYtdlpCorrupt =
        "Скачанный yt-dlp повреждён: контрольная сумма не совпала. Попробуйте запустить \
         Savio ещё раз.",
        "The downloaded yt-dlp is damaged: the checksum did not match. Try starting Savio \
         once more.",
        "Ներբեռնված yt-dlp-ը վնասված է. ստուգիչ գումարը չհամընկավ։ Փորձեք նորից գործարկել \
         Savio-ն։";
    SetupFfmpegCorrupt =
        "скачанный архив ffmpeg повреждён: контрольная сумма не совпала",
        "the downloaded ffmpeg archive is damaged: the checksum did not match",
        "ներբեռնված ffmpeg արխիվը վնասված է. ստուգիչ գումարը չհամընկավ";
    SetupNoFfmpegSources =
        "не задан ни один источник ffmpeg", "no ffmpeg source is configured",
        "ffmpeg-ի ոչ մի աղբյուր սահմանված չէ";
    /// `{}` — сколько источников перепробовано, `{}` — последняя ошибка.
    SetupAllSourcesFailed =
        "перепробованы все источники ({}), последняя ошибка — {}",
        "all sources have been tried ({}), the last error was {}",
        "փորձվել են բոլոր աղբյուրները ({}), վերջին սխալը՝ {}";
    /// `{}` — имя файла, которого не хватило.
    SetupMissingAfterExtract =
        "после распаковки не найден {}: содержимое архива отличается от ожидаемого",
        "{} was not found after extraction: the archive contents differ from what was expected",
        "բացելուց հետո {} չգտնվեց. արխիվի պարունակությունը տարբերվում է սպասվածից";

    /// Совет тому, у кого нет своего пакетного менеджера с готовой командой.
    SetupSystemPackageManager =
        "менеджером пакетов вашей системы", "with your system's package manager",
        "ձեր համակարգի փաթեթների կառավարիչով";
    /// `{}` — имя программы, `{}` — путь к ней, `{}` — чем её обновить.
    SetupDeclinedSystem =
        "{} установлен в системе, а не Savio:\n{}\n\nОбновите его так же, как ставили: {}. \
         Savio подменять чужой файл не станет — иначе он разойдётся с пакетным менеджером.",
        "{} is installed by the system, not by Savio:\n{}\n\nUpdate it the same way you \
         installed it: {}. Savio will not replace someone else's file — otherwise it would \
         fall out of step with the package manager.",
        "{}-ը տեղակայված է համակարգում, ոչ թե Savio-ի կողմից՝\n{}\n\nԹարմացրեք այն \
         այնպես, ինչպես տեղակայել եք՝ {}։ Savio-ն ուրիշի ֆայլը չի փոխարինի — հակառակ \
         դեպքում այն կհեռանա փաթեթների կառավարչից։";
    /// `{}` — имя программы, `{}` — путь к ней.
    SetupDeclinedPortable =
        "{} лежит рядом с Savio:\n{}\n\nЭто портативная поставка — обновите её целиком или \
         замените этот файл вручную. Savio его не трогает, чтобы не сломать сборку.",
        "{} sits next to Savio:\n{}\n\nThis is a portable build — update it as a whole, or \
         replace this file by hand. Savio leaves it alone so as not to break the build.",
        "{}-ը գտնվում է Savio-ի կողքին՝\n{}\n\nՍա շարժական փաթեթ է — թարմացրեք այն \
         ամբողջությամբ կամ փոխարինեք այս ֆայլը ձեռքով։ Savio-ն այն չի դիպչում, որպեսզի \
         չփչացնի հավաքածուն։";

    /// `{}` — причина отказа.
    SetupFfmpegUpdateFailed =
        "Не удалось обновить ffmpeg: {}.", "Could not update ffmpeg: {}.",
        "Չհաջողվեց թարմացնել ffmpeg-ը՝ {}։";
    /// Приписывается к «уже последней версии» после перекачки целого архива.
    SetupSameBuildNote =
        " На сервере лежит та же сборка.", " The server has the same build.",
        " Սերվերում նույն հավաքածուն է։";
    SetupFfmpegRedownloaded =
        "ffmpeg скачан заново. Версию узнать не вышло.",
        "ffmpeg has been downloaded again. Its version could not be read.",
        "ffmpeg-ը ներբեռնվել է կրկին։ Տարբերակը պարզել չհաջողվեց։";
    /// `{}` — имя программы, `{}` — версия, `{}` — оговорка (бывает пустой).
    SetupAlreadyLatest =
        "{} уже последней версии ({}).{}", "{} is already the latest version ({}).{}",
        "{}-ն արդեն վերջին տարբերակի է ({})։{}";
    /// `{}` — имя программы, `{}` — прежняя версия, `{}` — новая.
    SetupUpdated =
        "{} обновлён: было {}, стало {}.", "{} updated: was {}, now {}.",
        "{}-ը թարմացվեց՝ էր {}, դարձավ {}։";
    /// `{}` — имя программы, `{}` — версия.
    SetupInstalledVersion =
        "{} установлен, версия {}.", "{} installed, version {}.",
        "{}-ը տեղակայվեց, տարբերակ {}։";

    /// `{}` — то, что сказала сеть.
    SetupTagFailed =
        "Не удалось узнать свежий выпуск yt-dlp: {}",
        "Could not find out the latest yt-dlp release: {}",
        "Չհաջողվեց պարզել yt-dlp-ի թարմ թողարկումը՝ {}";
    SetupGithubUnexpected =
        "GitHub вернул неожиданный ответ о выпуске yt-dlp.",
        "GitHub returned an unexpected answer about the yt-dlp release.",
        "GitHub-ը վերադարձրեց անսպասելի պատասխան yt-dlp-ի թողարկման մասին։";
    SetupGithubNoTag =
        "В ответе GitHub нет номера выпуска yt-dlp.",
        "GitHub's answer has no yt-dlp release number.",
        "GitHub-ի պատասխանում yt-dlp-ի թողարկման համարը չկա։";
    SetupCancelled = "Установка отменена.", "The installation was cancelled.",
        "Տեղակայումը չեղարկվեց։";

    /// `{}` — то, что сказала сеть.
    NetReadFailed =
        "не удалось прочитать ответ: {}", "could not read the answer: {}",
        "չհաջողվեց կարդալ պատասխանը՝ {}";
    /// `{}` — код ответа сервера.
    NetStatusCode =
        "сервер ответил кодом {}", "the server answered with code {}",
        "սերվերը պատասխանեց {} կոդով";
    NetTimeout =
        "истекло время ожидания. Проверьте подключение к интернету",
        "the wait timed out. Check your internet connection",
        "սպասման ժամանակը սպառվեց։ Ստուգեք ինտերնետ կապը";
    /// `{}` — подробности от библиотеки.
    NetNoConnection =
        "нет связи с сервером ({})", "no connection to the server ({})",
        "սերվերի հետ կապ չկա ({})";

    /// `{}` — путь к файлу, `{}` — то, что сказала система.
    SetupCreateFileFailed =
        "не удалось создать файл {}: {}", "could not create the file {}: {}",
        "չհաջողվեց ստեղծել {} ֆայլը՝ {}";
    SetupDownloadBroken =
        "обрыв загрузки: {}", "the download broke off: {}", "ներբեռնումն ընդհատվեց՝ {}";
    SetupWriteFileFailed =
        "не удалось записать файл: {}", "could not write the file: {}",
        "չհաջողվեց գրել ֆայլը՝ {}";
    SetupFlushFailed =
        "не удалось дописать файл: {}", "could not finish writing the file: {}",
        "չհաջողվեց լրացնել ֆայլը՝ {}";
    /// `{}` — путь назначения, `{}` — то, что сказала система.
    SetupMoveFailed =
        "не удалось переместить файл в {}: {}", "could not move the file to {}: {}",
        "չհաջողվեց տեղափոխել ֆայլը {}՝ {}";
    SetupChmodFailed =
        "не удалось сделать файл исполняемым: {}",
        "could not make the file executable: {}",
        "չհաջողվեց ֆայլը դարձնել գործարկելի՝ {}";

    /// `{}` — путь к tar, `{}` — то, что сказала система.
    TarLaunchFailed =
        "не удалось запустить {} для распаковки: {}",
        "could not start {} to extract the archive: {}",
        "չհաջողվեց գործարկել {}-ն արխիվը բացելու համար՝ {}";
    TarNoXz =
        "не найдена программа xz, нужная для распаковки. Установите пакет xz-utils",
        "the xz program needed for extraction was not found. Install the xz-utils package",
        "չգտնվեց արխիվը բացելու համար անհրաժեշտ xz ծրագիրը։ Տեղակայեք xz-utils փաթեթը";
    TarFailed =
        "tar не смог распаковать архив", "tar could not extract the archive",
        "tar-ը չկարողացավ բացել արխիվը";
    /// `{}` — последняя строка ругани tar.
    TarFailedWithTail =
        "tar не смог распаковать архив: {}", "tar could not extract the archive: {}",
        "tar-ը չկարողացավ բացել արխիվը՝ {}";

    // -----------------------------------------------------------------------
    // Метаданные файла: имена тегов, поломки контейнеров, ошибки записи
    //
    // Имена тегов — тоже интерфейс: их видно во вкладке «Метаданные». Забыть
    // их легко, потому что лежат они далеко от `app.rs` и попадают на экран
    // через `Event::Tags`.
    // -----------------------------------------------------------------------

    MetaVideoUnsupported =
        "Очистка видео временно не поддерживается.",
        "Cleaning video is not supported for now.",
        "Տեսանյութի մաքրումը առայժմ չի աջակցվում։";
    MetaTiffReadOnly =
        "Для TIFF доступно только чтение: удалить теги, не пересобрав файл целиком, нельзя, \
         а пересборка рискует испортить снимок.",
        "TIFF is read-only here: tags cannot be removed without rebuilding the whole file, \
         and rebuilding risks ruining the shot.",
        "TIFF-ի համար հասանելի է միայն ընթերցումը. պիտակները հեռացնել առանց ֆայլն \
         ամբողջությամբ վերակառուցելու հնարավոր չէ, իսկ վերակառուցումը ռիսկ է պարունակում \
         փչացնել լուսանկարը։";
    MetaFormatUnsupported =
        "Этот формат не поддерживается. Выберите MP3 или изображение (JPG, PNG, WebP, GIF).",
        "This format is not supported. Pick an MP3 or an image (JPG, PNG, WebP, GIF).",
        "Այս ձևաչափը չի աջակցվում։ Ընտրեք MP3 կամ պատկեր (JPG, PNG, WebP, GIF)։";

    MetaNotJpeg =
        "Это не JPEG: файл не начинается с сигнатуры JPEG.",
        "This is not a JPEG: the file does not start with the JPEG signature.",
        "Սա JPEG չէ. ֆայլը չի սկսվում JPEG ստորագրությամբ։";
    MetaJpegLengthBroken =
        "JPEG повреждён: обрыв на длине сегмента.",
        "The JPEG is damaged: it breaks off at a segment length.",
        "JPEG-ը վնասված է. ընդհատում՝ հատվածի երկարության վրա։";
    MetaJpegZeroSegment =
        "JPEG повреждён: сегмент нулевой длины.",
        "The JPEG is damaged: a segment of zero length.",
        "JPEG-ը վնասված է. զրոյական երկարության հատված։";
    MetaJpegSegmentOverrun =
        "JPEG повреждён: сегмент выходит за конец файла.",
        "The JPEG is damaged: a segment runs past the end of the file.",
        "JPEG-ը վնասված է. հատվածը դուրս է գալիս ֆայլի վերջից։";

    MetaNotPng =
        "Это не PNG: файл не начинается с сигнатуры PNG.",
        "This is not a PNG: the file does not start with the PNG signature.",
        "Սա PNG չէ. ֆայլը չի սկսվում PNG ստորագրությամբ։";
    MetaPngLengthBroken =
        "PNG повреждён: обрыв на длине чанка.",
        "The PNG is damaged: it breaks off at a chunk length.",
        "PNG-ն վնասված է. ընդհատում՝ բլոկի երկարության վրա։";
    MetaPngTypeBroken =
        "PNG повреждён: обрыв на типе чанка.",
        "The PNG is damaged: it breaks off at a chunk type.",
        "PNG-ն վնասված է. ընդհատում՝ բլոկի տեսակի վրա։";
    MetaPngImplausibleLength =
        "PNG повреждён: неправдоподобная длина чанка.",
        "The PNG is damaged: an implausible chunk length.",
        "PNG-ն վնասված է. բլոկի անհավանական երկարություն։";
    MetaPngChunkOverrun =
        "PNG повреждён: чанк выходит за конец файла.",
        "The PNG is damaged: a chunk runs past the end of the file.",
        "PNG-ն վնասված է. բլոկը դուրս է գալիս ֆայլի վերջից։";

    MetaNotWebp =
        "Это не WebP: файл не начинается с сигнатуры RIFF/WEBP.",
        "This is not a WebP: the file does not start with the RIFF/WEBP signature.",
        "Սա WebP չէ. ֆայլը չի սկսվում RIFF/WEBP ստորագրությամբ։";
    MetaWebpTypeBroken =
        "WebP повреждён: обрыв на типе чанка.",
        "The WebP is damaged: it breaks off at a chunk type.",
        "WebP-ն վնասված է. ընդհատում՝ բլոկի տեսակի վրա։";
    MetaWebpLengthBroken =
        "WebP повреждён: обрыв на длине чанка.",
        "The WebP is damaged: it breaks off at a chunk length.",
        "WebP-ն վնասված է. ընդհատում՝ բլոկի երկարության վրա։";
    MetaWebpImplausibleLength =
        "WebP повреждён: неправдоподобная длина чанка.",
        "The WebP is damaged: an implausible chunk length.",
        "WebP-ն վնասված է. բլոկի անհավանական երկարություն։";
    MetaWebpChunkOverrun =
        "WebP повреждён: чанк выходит за конец файла.",
        "The WebP is damaged: a chunk runs past the end of the file.",
        "WebP-ն վնասված է. բլոկը դուրս է գալիս ֆայլի վերջից։";

    MetaNotGif =
        "Это не GIF: файл не начинается с сигнатуры GIF.",
        "This is not a GIF: the file does not start with the GIF signature.",
        "Սա GIF չէ. ֆայլը չի սկսվում GIF ստորագրությամբ։";
    MetaGifExtensionBroken =
        "GIF повреждён: обрыв на расширении.",
        "The GIF is damaged: it breaks off at an extension.",
        "GIF-ը վնասված է. ընդհատում՝ ընդլայնման վրա։";
    MetaGifExtensionOverrun =
        "GIF повреждён: расширение выходит за конец файла.",
        "The GIF is damaged: an extension runs past the end of the file.",
        "GIF-ը վնասված է. ընդլայնումը դուրս է գալիս ֆայլի վերջից։";
    MetaGifFrameBroken =
        "GIF повреждён: обрыв на кадре.",
        "The GIF is damaged: it breaks off at a frame.",
        "GIF-ը վնասված է. ընդհատում՝ կադրի վրա։";
    MetaGifFrameOverrun =
        "GIF повреждён: кадр выходит за конец файла.",
        "The GIF is damaged: a frame runs past the end of the file.",
        "GIF-ը վնասված է. կադրը դուրս է գալիս ֆայլի վերջից։";
    MetaGifUnknownBlock =
        "GIF повреждён: неизвестный блок.",
        "The GIF is damaged: an unknown block.",
        "GIF-ը վնասված է. անհայտ բլոկ։";
    MetaGifPaletteBroken =
        "GIF повреждён: обрыв на таблице цветов.",
        "The GIF is damaged: it breaks off at the colour table.",
        "GIF-ը վնասված է. ընդհատում՝ գույների աղյուսակի վրա։";

    /// `{}` — сколько байт занимает запись.
    MetaPresentBytes =
        "присутствует, {} Б", "present, {} B", "առկա է, {} Բ";
    MetaTextCompressed = "текст (сжатый)", "text (compressed)", "տեքստ (սեղմված)";
    MetaTextUtf8 = "текст (UTF-8)", "text (UTF-8)", "տեքստ (UTF-8)";
    WordPresent = "присутствует", "present", "առկա է";
    /// `{}` — число байт.
    MetaRawBytes = "{} Б", "{} B", "{} Բ";

    TagComment = "Комментарий", "Comment", "Մեկնաբանություն";
    TagModified = "Изменён", "Modified", "Փոփոխվել է";
    TagAppExtension =
        "Расширение приложения", "Application extension", "Ծրագրի ընդլայնում";
    TagTextBlock = "Текстовый блок", "Text block", "Տեքստային բլոկ";

    TagGpsLatitudeRef = "GPS: широта (полушарие)", "GPS: latitude (hemisphere)", "GPS՝ լայնություն (կիսագունդ)";
    TagGpsLatitude = "GPS: широта", "GPS: latitude", "GPS՝ լայնություն";
    TagGpsLongitudeRef = "GPS: долгота (полушарие)", "GPS: longitude (hemisphere)", "GPS՝ երկայնություն (կիսագունդ)";
    TagGpsLongitude = "GPS: долгота", "GPS: longitude", "GPS՝ երկայնություն";
    TagGpsAltitude = "GPS: высота", "GPS: altitude", "GPS՝ բարձրություն";
    TagGpsTime = "GPS: время съёмки (UTC)", "GPS: time taken (UTC)", "GPS՝ նկարահանման ժամ (UTC)";
    TagGpsDate = "GPS: дата", "GPS: date", "GPS՝ ամսաթիվ";

    TagDescription = "Описание", "Description", "Նկարագրություն";
    TagMaker = "Производитель", "Maker", "Արտադրող";
    TagCameraModel = "Модель камеры", "Camera model", "Ֆոտոխցիկի մոդել";
    TagOrientation = "Ориентация", "Orientation", "Կողմնորոշում";
    TagSoftware = "Программа", "Software", "Ծրագիր";
    TagDateModified = "Дата изменения", "Date modified", "Փոփոխման ամսաթիվ";
    TagAuthor = "Автор", "Author", "Հեղինակ";
    TagCopyright = "Авторские права", "Copyright", "Հեղինակային իրավունք";
    TagExposure = "Выдержка", "Exposure", "Ձգան";
    TagAperture = "Диафрагма", "Aperture", "Դիաֆրագմա";
    TagDateTaken = "Дата съёмки", "Date taken", "Նկարահանման ամսաթիվ";
    TagDateDigitized = "Дата оцифровки", "Date digitised", "Թվայնացման ամսաթիվ";
    TagFocalLength = "Фокусное расстояние", "Focal length", "Կիզակետային հեռավորություն";
    TagWidth = "Ширина", "Width", "Լայնություն";
    TagHeight = "Высота", "Height", "Բարձրություն";
    TagCameraOwner = "Владелец камеры", "Camera owner", "Ֆոտոխցիկի սեփականատեր";
    TagLensMaker = "Производитель объектива", "Lens maker", "Օբյեկտիվի արտադրող";
    TagLensModel = "Модель объектива", "Lens model", "Օբյեկտիվի մոդել";
    TagLensSerial = "Серийный номер объектива", "Lens serial number", "Օբյեկտիվի սերիական համար";
    TagCameraSerial = "Серийный номер камеры", "Camera serial number", "Ֆոտոխցիկի սերիական համար";

    MetaTagBytes = "Объём тегов", "Tag size", "Պիտակների ծավալ";
    /// `{}` — число байт.
    MetaTagBytesValue =
        "{} Б (включая обложку, если она есть)",
        "{} B (including the cover art, if there is any)",
        "{} Բ (ներառյալ շապիկը, եթե այն կա)";
    TagTags = "Теги", "Tags", "Պիտակներ";
    MetaTagsUnreadable =
        "присутствуют, но прочитать их нечем: не найден ffprobe",
        "present, but there is nothing to read them with: ffprobe was not found",
        "առկա են, բայց կարդալու բան չկա. ffprobe չգտնվեց";

    /// `{}` — то, что сказала система.
    MetaFfprobeLaunchFailed =
        "Не удалось запустить ffprobe: {}", "Could not start ffprobe: {}",
        "Չհաջողվեց գործարկել ffprobe-ը՝ {}";
    MetaFfprobeFailed =
        "ffprobe не смог прочитать файл.", "ffprobe could not read the file.",
        "ffprobe-ը չկարողացավ կարդալ ֆայլը։";
    MetaFfprobeGarbage =
        "ffprobe вернул неразборчивый ответ.", "ffprobe returned an unreadable answer.",
        "ffprobe-ը վերադարձրեց անընթեռնելի պատասխան։";

    TagDuration = "Длительность", "Duration", "Տևողություն";
    TagBitrate = "Битрейт", "Bitrate", "Բիթրեյթ";
    /// `{}` — килобиты в секунду.
    MetaKbpsValue = "{} кбит/с", "{} kbps", "{} կբիթ/վ";
    TagCover = "Обложка", "Cover art", "Շապիկ";
    MetaCoverEmbedded = "встроена в файл", "embedded in the file", "ներկարված է ֆայլում";

    TagTitle = "Название", "Title", "Անվանում";
    TagArtist = "Исполнитель", "Artist", "Կատարող";
    TagAlbum = "Альбом", "Album", "Ալբոմ";
    TagAlbumArtist = "Исполнитель альбома", "Album artist", "Ալբոմի կատարող";
    TagYear = "Год", "Year", "Տարի";
    TagTrack = "Трек", "Track", "Թրեք";
    TagGenre = "Жанр", "Genre", "Ժանր";
    TagComposer = "Композитор", "Composer", "Կոմպոզիտոր";
    TagEncoder = "Кодировщик", "Encoder", "Կոդավորիչ";
    TagPublisher = "Издатель", "Publisher", "Հրատարակիչ";
    TagLanguage = "Язык", "Language", "Լեզու";
    TagLyrics = "Текст песни", "Lyrics", "Երգի տեքստ";

    /// `{}` — то, что сказала система.
    MetaReadFileFailed =
        "Не удалось прочитать файл: {}", "Could not read the file: {}",
        "Չհաջողվեց կարդալ ֆայլը՝ {}";
    MetaOpenFileFailed =
        "Не удалось открыть файл: {}", "Could not open the file: {}",
        "Չհաջողվեց բացել ֆայլը՝ {}";
    MetaReadSizeFailed =
        "Не удалось прочитать размер файла: {}", "Could not read the file size: {}",
        "Չհաջողվեց կարդալ ֆայլի չափը՝ {}";
    MetaReadHeadFailed =
        "Не удалось прочитать начало файла: {}", "Could not read the start of the file: {}",
        "Չհաջողվեց կարդալ ֆայլի սկիզբը՝ {}";
    MetaSeekEndFailed =
        "Не удалось перейти к концу файла: {}", "Could not seek to the end of the file: {}",
        "Չհաջողվեց անցնել ֆայլի վերջ՝ {}";
    MetaReadTailFailed =
        "Не удалось прочитать конец файла: {}", "Could not read the end of the file: {}",
        "Չհաջողվեց կարդալ ֆայլի վերջը՝ {}";
    MetaWriteFileFailed =
        "Не удалось записать файл: {}", "Could not write the file: {}",
        "Չհաջողվեց գրել ֆայլը՝ {}";
    MetaSeekAudioFailed =
        "Не удалось перейти к началу звука: {}", "Could not seek to the start of the audio: {}",
        "Չհաջողվեց անցնել ձայնի սկիզբ՝ {}";
    MetaCopyAudioFailed =
        "Не удалось скопировать звук: {}", "Could not copy the audio: {}",
        "Չհաջողվեց պատճենել ձայնը՝ {}";
    MetaTempCreateFailed =
        "Не удалось создать временный файл: {}", "Could not create the temporary file: {}",
        "Չհաջողվեց ստեղծել ժամանակավոր ֆայլը՝ {}";
    MetaTempFlushFailed =
        "Не удалось дописать временный файл: {}",
        "Could not finish writing the temporary file: {}",
        "Չհաջողվեց լրացնել ժամանակավոր ֆայլը՝ {}";
    MetaTempSyncFailed =
        "Не удалось сохранить временный файл: {}", "Could not save the temporary file: {}",
        "Չհաջողվեց պահպանել ժամանակավոր ֆայլը՝ {}";
    MetaReplaceFailed =
        "Не удалось заменить исходный файл: {}", "Could not replace the original file: {}",
        "Չհաջողվեց փոխարինել սկզբնական ֆայլը՝ {}";
    MetaEmptyResultKeepsOriginal =
        "Внутренняя ошибка: очистка дала пустой файл, исходный не тронут.",
        "Internal error: cleaning produced an empty file; the original was left untouched.",
        "Ներքին սխալ. մաքրումը տվեց դատարկ ֆայլ, սկզբնականը մնաց անփոփոխ։";
    MetaEmptyResult =
        "Внутренняя ошибка: очистка дала пустой файл.",
        "Internal error: cleaning produced an empty file.",
        "Ներքին սխալ. մաքրումը տվեց դատարկ ֆայլ։";
    MetaTempEmpty =
        "Внутренняя ошибка: временный файл пуст.",
        "Internal error: the temporary file is empty.",
        "Ներքին սխալ. ժամանակավոր ֆայլը դատարկ է։";

    // -----------------------------------------------------------------------
    // Снимок системы: названия карточек, строк и советов
    //
    // Названия железа (модель процессора, имя тома, производитель батареи)
    // сюда не попадают и попасть не могут: их даёт система, и перевода у них
    // нет. Переводятся только наши подписи вокруг них.
    // -----------------------------------------------------------------------

    StageProbingSystem = "Опрашиваю систему…", "Probing the system…", "Հարցում եմ համակարգին…";

    HwSystem = "Система", "System", "Համակարգ";
    HwKernel = "Ядро", "Kernel", "Միջուկ";
    HwHostName = "Имя машины", "Machine name", "Մեքենայի անունը";
    HwBitness = "Разрядность", "Architecture", "Բիթայնություն";
    HwUptime = "Работает", "Up for", "Աշխատում է";
    HwSystemUnnamed =
        "Система себя не назвала.", "The system did not name itself.",
        "Համակարգն իրեն չանվանեց։";

    HwCpu = "Процессор", "Processor", "Պրոցեսոր";
    HwVendor = "Производитель", "Vendor", "Արտադրող";
    HwPhysicalCores = "Физических ядер", "Physical cores", "Ֆիզիկական միջուկներ";
    HwLogicalCores = "Логических ядер", "Logical cores", "Տրամաբանական միջուկներ";
    HwFrequency = "Частота", "Frequency", "Հաճախություն";
    HwLoad = "Загрузка", "Load", "Բեռնվածություն";
    HwCpuUnnamed =
        "Процессор себя не назвал.", "The processor did not name itself.",
        "Պրոցեսորն իրեն չանվանեց։";

    HwMemory = "Память", "Memory", "Հիշողություն";
    HwMemoryUnknown =
        "Система не сообщила объём памяти.", "The system did not report the memory size.",
        "Համակարգը հիշողության ծավալը չհաղորդեց։";
    HwTotal = "Всего", "Total", "Ընդամենը";
    HwUsed = "Занято", "Used", "Զբաղված";
    HwAvailable = "Доступно", "Available", "Հասանելի";
    HwSwap = "Подкачка", "Swap", "Փոխանակում";
    /// Честный ноль, а не «нет данных»: подкачку выключил сам человек.
    HwSwapOff = "выключена", "turned off", "անջատված է";
    /// `{}` — сколько свободно, `{}` — сколько всего.
    HwFreeOfTotal = "{} из {} свободно", "{} of {} free", "{}՝ {}-ից ազատ";
    HwMemoryLowAdvice =
        "Свободной памяти меньше десятой части. Закройте лишние программы: при нехватке \
         система начнёт выгружать их на диск, и всё замедлится.",
        "Less than a tenth of the memory is free. Close the programs you do not need: when \
         memory runs short the system starts paging them out to disk, and everything slows \
         down.",
        "Ազատ հիշողությունը տասներորդից քիչ է։ Փակեք ավելորդ ծրագրերը. պակասի դեպքում \
         համակարգը կսկսի դրանք տեղափոխել սկավառակ, և ամեն ինչ կդանդաղի։";

    HwDisks = "Диски", "Disks", "Սկավառակներ";
    HwNoVolumes =
        "Система не перечислила ни одного тома.", "The system listed no volumes at all.",
        "Համակարգը ոչ մի հատոր չթվարկեց։";
    /// `{}` — точка монтирования.
    HwVolume = "Том {}", "Volume {}", "Հատոր {}";
    HwVolumeUnknown =
        "Система не сообщила объём тома.", "The system did not report the volume size.",
        "Համակարգը հատորի ծավալը չհաղորդեց։";
    HwMountPoint = "Точка монтирования", "Mount point", "Միացման կետ";
    HwFileSystem = "Файловая система", "File system", "Ֆայլային համակարգ";
    HwFree = "Свободно", "Free", "Ազատ";
    HwKind = "Тип", "Kind", "Տեսակ";
    HwHardDisk = "жёсткий диск", "hard disk", "կոշտ սկավառակ";
    HwRemovable = "Съёмный", "Removable", "Հանովի";
    WordYes = "да", "yes", "այո";
    WordNo = "нет", "no", "ոչ";
    HwDiskLowAdvice =
        "На томе осталось меньше десятой части места. Скачивать сюда большие ролики уже \
         рискованно: yt-dlp прервётся на середине.",
        "Less than a tenth of the space is left on the volume. Downloading big videos here is \
         already risky: yt-dlp will break off halfway.",
        "Հատորում մնացել է տեղի տասներորդից քիչը։ Այստեղ մեծ տեսանյութեր ներբեռնելն արդեն \
         ռիսկային է. yt-dlp-ն կընդհատվի կեսից։";

    HwNetwork = "Сеть", "Network", "Ցանց";
    HwNoInterfaces =
        "Система не перечислила сетевые интерфейсы.",
        "The system listed no network interfaces.",
        "Համակարգը ցանցային միջերեսներ չթվարկեց։";
    HwInterfaceOne = "интерфейс", "interface", "միջերես";
    HwInterfaceFew = "интерфейса", "interfaces", "միջերես";
    HwInterfaceMany = "интерфейсов", "interfaces", "միջերես";

    HwBattery = "Батарея", "Battery", "Մարտկոց";
    /// `{}` — то, что сказала система.
    HwBatteryManagerFailed =
        "Не удалось обратиться к батарее: {}", "Could not reach the battery: {}",
        "Չհաջողվեց դիմել մարտկոցին՝ {}";
    HwBatteryListFailed =
        "Не удалось перечислить батареи: {}", "Could not list the batteries: {}",
        "Չհաջողվեց թվարկել մարտկոցները՝ {}";
    HwNoBattery =
        "Батарея не обнаружена — обычное дело для настольной машины.",
        "No battery was found — an ordinary thing on a desktop machine.",
        "Մարտկոց չհայտնաբերվեց — սովորական բան սեղանադիր մեքենայի համար։";
    HwCharge = "Заряд", "Charge", "Լիցք";
    HwState = "Состояние", "State", "Վիճակ";
    HwCharging = "заряжается", "charging", "լիցքավորվում է";
    HwDischarging = "разряжается", "discharging", "լիցքաթափվում է";
    HwDrained = "разряжена", "drained", "լիցքաթափված է";
    HwOnMains = "от сети, зарядка не идёт", "on mains, not charging", "ցանցից, լիցքավորում չկա";
    HwStateUnknown = "неизвестно", "unknown", "անհայտ";
    HwCapacityNow = "Ёмкость сейчас", "Capacity now", "Ընթացիկ տարողություն";
    HwCapacityDesign = "Ёмкость проектная", "Design capacity", "Նախագծային տարողություն";
    HwWear = "Износ", "Wear", "Մաշվածություն";
    HwCycles = "Циклов заряда", "Charge cycles", "Լիցքավորման ցիկլեր";
    HwVoltage = "Напряжение", "Voltage", "Լարում";
    HwPowerDraw = "Отдаёт", "Drawing", "Տալիս է";
    HwModel = "Модель", "Model", "Մոդել";
    HwBatteryNoCapacity =
        "Батарея есть, но ёмкость драйвер не сообщает — износ посчитать не из чего.",
        "There is a battery, but the driver does not report its capacity — there is nothing \
         to compute the wear from.",
        "Մարտկոց կա, բայց դրայվերը տարողությունը չի հաղորդում — մաշվածությունը հաշվելու \
         բան չկա։";
    /// `{}` — заряд, `{}` — износ.
    HwBatterySummary = "Заряд {}, износ {}", "Charge {}, wear {}", "Լիցք {}, մաշվածություն {}";
    HwChargeUnknown = "неизвестен", "unknown", "անհայտ";
    HwBatteryWornAdvice =
        "Батарея держит меньше четырёх пятых от проектной ёмкости. Это не поломка, но время \
         работы от неё будет заметно меньше заявленного, и дальше оно продолжит уменьшаться.",
        "The battery holds less than four fifths of its design capacity. That is not a fault, \
         but the time it runs for will be noticeably shorter than stated, and it will keep \
         going down.",
        "Մարտկոցը պահում է նախագծային տարողության չորս հինգերորդից քիչը։ Սա անսարքություն \
         չէ, բայց դրանից աշխատելու ժամանակը զգալիորեն ավելի քիչ կլինի հայտարարվածից և \
         հետագայում կշարունակի նվազել։";

    UnitWattHour = "Вт·ч", "Wh", "Վտ·ժ";
    UnitVolt = "В", "V", "Վ";
    UnitWatt = "Вт", "W", "Վտ";
    UnitMegabitPerSecond = "Мбит/с", "Mbps", "Մբիթ/վ";
    UnitGigabitPerSecond = "Гбит/с", "Gbps", "Գբիթ/վ";

    HwUsb = "USB-устройства", "USB devices", "USB-սարքեր";
    /// `{}` — то, что сказала система.
    HwUsbListFailed =
        "Не удалось перечислить устройства: {}", "Could not list the devices: {}",
        "Չհաջողվեց թվարկել սարքերը՝ {}";
    /// `{}` — идентификатор производителя и продукта, шестнадцатеричный.
    HwUsbUnnamedDevice = "Устройство {}", "Device {}", "Սարք {}";
    /// `{}` — версия USB.
    HwUsbVersion = "USB {}", "USB {}", "USB {}";
    /// `{}` — версия USB, `{}` — скорость вместе с единицей.
    HwUsbVersionWithSpeed = "USB {}, {}", "USB {}, {}", "USB {}, {}";
    HwAndMore = "И ещё", "And more", "Եվ ևս";
    HwDeviceOne = "устройство", "device", "սարք";
    HwDeviceFew = "устройства", "devices", "սարք";
    HwDeviceMany = "устройств", "devices", "սարք";
    HwNoUsb =
        "Подключённых устройств не найдено.", "No connected devices were found.",
        "Միացված սարքեր չգտնվեցին։";

    // Монитор производительности.
    MonCoreOne = "ядро", "core", "միջուկ";
    MonCoreFew = "ядра", "cores", "միջուկ";
    MonCoreMany = "ядер", "cores", "միջուկ";
    /// `{}` — число ядер, `{}` — слово «ядро», `{}` — частота.
    MonCoresWithFrequency = "{} {} · {}", "{} {} · {}", "{} {} · {}";
    /// `{}` — число ядер, `{}` — слово «ядро».
    MonCoresOnly = "{} {}", "{} {}", "{} {}";
    /// `{}` — скорость приёма, `{}` — скорость отдачи.
    MonNetworkLine =
        "Приём {} · Отдача {}", "In {} · Out {}", "Ընդունում {} · Հանձնում {}";
    /// `{}` — скорость чтения, `{}` — скорость записи.
    MonDiskLine = "Чтение {} · Запись {}", "Read {} · Write {}", "Ընթերցում {} · Գրառում {}";

    HwGpu = "Видеокарта", "Graphics card", "Վիդեոքարտ";
    HwDriver = "Драйвер", "Driver", "Դրայվեր";
    HwRendering = "Отрисовка", "Rendering", "Արտապատկերում";
    HwGpuDiscrete = "дискретная", "discrete", "դիսկրետ";
    HwGpuIntegrated = "встроенная", "integrated", "ներկառուցված";
    HwGpuSoftware = "программная отрисовка", "software rendering", "ծրագրային արտապատկերում";
    HwGpuVirtual = "виртуальная", "virtual", "վիրտուալ";
    HwGpuUnknownKind = "неизвестного типа", "of an unknown kind", "անհայտ տեսակի";
    /// `{}` — название, `{}` — тип.
    HwNameAndKind = "{} ({})", "{} ({})", "{} ({})";

    // -----------------------------------------------------------------------
    // Движок: стадии, предупреждения, ошибки запуска
    // -----------------------------------------------------------------------

    EngineFfmpegMissingLog =
        "ffmpeg не найден — склейка видео со звуком и конвертация в MP3 работать не будут.",
        "ffmpeg was not found — merging video with audio and converting to MP3 will not work.",
        "ffmpeg չգտնվեց — տեսանյութի ու ձայնի միացումը և MP3 փոխարկումը չեն աշխատի։";
    EngineNoFfmpegBoth =
        "Без ffmpeg не выйдет ни вырезать фрагмент, ни вшить метаданные, обложку и \
         субтитры: ролик скачается целиком и без них.",
        "Without ffmpeg neither cutting a fragment nor embedding metadata, the thumbnail \
         and subtitles will work: the video will be downloaded whole and without them.",
        "Առանց ffmpeg-ի չի ստացվի ո՛չ հատված կտրել, ո՛չ ներկարել մետատվյալները, շապիկը \
         և ենթագրերը. տեսանյութը կներբեռնվի ամբողջությամբ և առանց դրանց։";
    EngineNoFfmpegSection =
        "Вырезать фрагмент без ffmpeg нельзя — ролик скачается целиком.",
        "A fragment cannot be cut without ffmpeg — the video will be downloaded whole.",
        "Առանց ffmpeg-ի հատված կտրել հնարավոր չէ — տեսանյութը կներբեռնվի ամբողջությամբ։";
    EngineNoFfmpegEmbed =
        "Вшить метаданные, обложку и субтитры без ffmpeg нельзя — файл сохранится без них.",
        "Metadata, the thumbnail and subtitles cannot be embedded without ffmpeg — the file \
         will be saved without them.",
        "Մետատվյալները, շապիկը և ենթագրերն առանց ffmpeg-ի ներկարել հնարավոր չէ — ֆայլը \
         կպահվի առանց դրանց։";
    /// Приписывается к любому из трёх предупреждений выше.
    EngineRestartForFfmpeg =
        "Перезапустите Savio: при старте он сам попробует скачать ffmpeg ещё раз.",
        "Restart Savio: on start it will try to download ffmpeg once more by itself.",
        "Վերագործարկեք Savio-ն. մեկնարկելիս այն ինքը կփորձի նորից ներբեռնել ffmpeg-ը։";
    /// `{}` и `{}` — два пути к папкам.
    EngineFfprobeApart =
        "ffprobe лежит не рядом с ffmpeg ({} и {}) — yt-dlp его не увидит.",
        "ffprobe is not next to ffmpeg ({} and {}) — yt-dlp will not see it.",
        "ffprobe-ը ffmpeg-ի կողքին չէ ({} և {}) — yt-dlp-ն այն չի տեսնի։";
    EngineSyncError =
        "Внутренняя ошибка синхронизации",
        "Internal synchronisation error",
        "Ներքին համաժամացման սխալ";

    CookieFileNotPicked =
        "Вход просят из файла, а сам файл не выбран — ролик скачается без входа в аккаунт. \
         Выберите файл под списком «Вход на сайт» или верните в нём пункт «Не использовать».",
        "A file login is asked for, but no file is picked — the video will be downloaded \
         without signing in. Pick a file under the “Site login” list, or set that list back \
         to “Do not use”.",
        "Մուտքը խնդրվում է ֆայլից, բայց ֆայլն ընտրված չէ — տեսանյութը կներբեռնվի առանց \
         հաշիվ մուտք գործելու։ Ընտրեք ֆայլ «Մուտք կայք» ցանկի տակ կամ վերադարձրեք դրա \
         «Չօգտագործել» կետը։";
    /// `{}` — имя файла. Имя, а не путь: баннер от полного пути растянулся бы
    /// на три строки, а сам путь и так виден строкой под списком.
    CookieFileMissing =
        "Файл cookies «{}» не найден — ролик скачается без входа в аккаунт, и закрытый \
         сайт его, скорее всего, не отдаст. Файл переименовали, перенесли или удалили: \
         выберите его заново под списком «Вход на сайт».",
        "The cookies file “{}” was not found — the video will be downloaded without signing \
         in, and a closed site will most likely refuse it. The file was renamed, moved or \
         deleted: pick it again under the “Site login” list.",
        "«{}» cookies ֆայլը չի գտնվել — տեսանյութը կներբեռնվի առանց հաշիվ մուտք գործելու, \
         և փակ կայքը, ամենայն հավանականությամբ, այն չի տա։ Ֆայլը վերանվանել, տեղափոխել \
         կամ ջնջել են. ընտրեք այն նորից «Մուտք կայք» ցանկի տակ։";
    CookieFileReadonly =
        "Файл cookies «{}» закрыт для записи, а yt-dlp дописывает в него свежие cookies \
         после загрузки — и оборвётся ошибкой, когда ролик уже будет скачан. Снимите \
         с файла «Только чтение» или скопируйте его в обычную папку.",
        "The cookies file “{}” is write-protected, and yt-dlp appends fresh cookies to it \
         after the download — so it will break off with an error once the video has already \
         been downloaded. Clear “Read-only” on the file, or copy it into an ordinary folder.",
        "«{}» cookies ֆայլը գրելու համար փակ է, իսկ yt-dlp-ն ներբեռնումից հետո դրան \
         ավելացնում է թարմ cookies — և կընդհատվի սխալով, երբ տեսանյութն արդեն ներբեռնված \
         կլինի։ Ֆայլից հանեք «Միայն կարդալու» հատկանիշը կամ պատճենեք այն սովորական \
         թղթապանակ։";

    StageReadingLink = "Читаю ссылку…", "Reading the link…", "Կարդում եմ հղումը…";
    StageDownloading = "Загрузка…", "Downloading…", "Ներբեռնում…";
    StageCutting = "Вырезаю фрагмент…", "Cutting the fragment…", "Կտրում եմ հատվածը…";
    StageAlreadyExisted =
        "Готово (файл уже существовал)",
        "Done (the file already existed)",
        "Պատրաստ է (ֆայլն արդեն կար)";
    StageReadingMetadata =
        "Читаю метаданные…", "Reading the metadata…", "Կարդում եմ մետատվյալները…";
    StageStrippingMetadata =
        "Удаляю метаданные…", "Removing the metadata…", "Ջնջում եմ մետատվյալները…";

    /// `{}` — начало фрагмента, `{}` — длительность ролика.
    WarnSectionStartPastEnd =
        "Начало фрагмента ({}) лежит за концом ролика ({}) — вырезать оттуда нечего, \
         и файл получится не тот, что вы ждёте. Поправьте границы и запустите снова.",
        "The fragment start ({}) lies past the end of the video ({}) — there is nothing to \
         cut there, and the file will not be the one you expect. Fix the bounds and start \
         again.",
        "Հատվածի սկիզբը ({}) տեսանյութի վերջից այն կողմ է ({}) — այնտեղից կտրելու բան չկա, \
         և ֆայլը կստացվի ոչ այն, ինչ սպասում եք։ Ուղղեք սահմանները և նորից գործարկեք։";
    LogCutAfterPlan =
        "Фрагмент занимает больше половины ролика: качаем целиком быстрым путём и вырежем \
         кусок сами.",
        "The fragment takes up more than half of the video: downloading it whole by the fast \
         path and cutting the piece ourselves.",
        "Հատվածը զբաղեցնում է տեսանյութի կեսից ավելին. ներբեռնում ենք ամբողջությամբ արագ \
         ճանապարհով և կտորը կկտրենք ինքներս։";
    /// `{}` — объяснение от ffmpeg.
    WarnCutFailed =
        "Вырезать фрагмент не вышло — ролик сохранён целиком.\n\n{}\n\nЧтобы попробовать \
         ещё раз, уберите или переименуйте сохранённый файл: пока он лежит на месте, \
         загрузка считается сделанной и повторяться не будет.",
        "Cutting the fragment did not work out — the video has been saved whole.\n\n{}\n\n\
         To try again, remove or rename the saved file: while it stays in place the download \
         counts as done and will not repeat.",
        "Հատվածը կտրել չստացվեց — տեսանյութը պահվել է ամբողջությամբ։\n\n{}\n\nԿրկին \
         փորձելու համար հեռացրեք կամ վերանվանեք պահված ֆայլը. քանի դեռ այն տեղում է, \
         ներբեռնումը համարվում է կատարված և չի կրկնվի։";

    /// `{}` — то, что сказала система.
    ErrYtdlpLaunch =
        "Не удалось запустить yt-dlp: {}", "Could not start yt-dlp: {}",
        "Չհաջողվեց գործարկել yt-dlp-ն՝ {}";
    ErrNoStdout = "Нет stdout у yt-dlp", "yt-dlp has no stdout", "yt-dlp-ն stdout չունի";
    ErrNoStderr = "Нет stderr у yt-dlp", "yt-dlp has no stderr", "yt-dlp-ն stderr չունի";
    ErrWaitFailed = "Сбой ожидания: {}", "Waiting failed: {}", "Սպասումը ձախողվեց՝ {}";
    ErrProcessLost = "Процесс потерян", "The process is lost", "Գործընթացը կորել է";
    /// `{}` — то, что сказала сеть.
    LogThumbnailFailed =
        "Обложка не загрузилась: {}", "The thumbnail did not load: {}",
        "Շապիկը չբեռնվեց՝ {}";

    /// `{}` — имя бинарника, своё на каждой системе.
    ToolYtdlpMissing =
        "Не найден {}. Положите его рядом с Savio или установите так, чтобы он был \
         доступен в PATH.",
        "{} was not found. Put it next to Savio, or install it so that it is available \
         in PATH.",
        "{} չի գտնվել։ Տեղադրեք այն Savio-ի կողքին կամ տեղակայեք այնպես, որ այն հասանելի \
         լինի PATH-ում։";

    // Всё это — хвост к `LogThumbnailFailed`, то есть строка в журнале, а не
    // баннер: обложка украшение, и её неудача загрузку не роняет.
    CoverServerRefused =
        "сервер не отдал обложку ({})", "the server did not return the thumbnail ({})",
        "սերվերը շապիկը չտվեց ({})";
    CoverReadFailed =
        "не удалось прочитать обложку ({})", "could not read the thumbnail ({})",
        "չհաջողվեց կարդալ շապիկը ({})";
    CoverFormatUnknown =
        "не удалось определить формат обложки ({})",
        "could not determine the thumbnail format ({})",
        "չհաջողվեց որոշել շապիկի ձևաչափը ({})";
    CoverDecodeFailed =
        "не удалось разобрать обложку ({})", "could not decode the thumbnail ({})",
        "չհաջողվեց վերծանել շապիկը ({})";
    // -----------------------------------------------------------------------
    // Почему загрузка не удалась
    //
    // Переводятся **объяснения**, а не приметы. Приметы (`not a bot`,
    // `in your country`, `cookie database`) принадлежат yt-dlp и сайтам,
    // лежат в `FAILURE_HINTS` английскими и переводу не подлежат: переведи
    // их — и беда перестанет узнаваться вовсе, причём молча.
    // -----------------------------------------------------------------------

    /// `{}` — адрес списка сайтов, которые умеет yt-dlp.
    FailUnsupportedSite =
        "Этот сайт не поддерживается.\n\nSavio скачивает через yt-dlp, а он не умеет \
         работать с этим адресом. Дело не в ссылке и не в приложении — сайта просто нет \
         в списке поддерживаемых:\n{}\n\nЕсли сайт там есть, движок устарел — нажмите \
         «Обновить движок».",
        "This site is not supported.\n\nSavio downloads through yt-dlp, and it cannot work \
         with this address. It is neither the link nor the application — the site simply is \
         not in the list of supported ones:\n{}\n\nIf the site is there, the engine is out \
         of date — press “Update the engine”.",
        "Այս կայքը չի աջակցվում։\n\nSavio-ն ներբեռնում է yt-dlp-ի միջոցով, իսկ այն չի \
         կարողանում աշխատել այս հասցեի հետ։ Խնդիրը ո՛չ հղումն է, ո՛չ ծրագիրը — կայքը \
         պարզապես չկա աջակցվողների ցանկում՝\n{}\n\nԵթե կայքն այնտեղ կա, ուրեմն շարժիչը \
         հնացել է — սեղմեք «Թարմացնել շարժիչը»։";
    FailEmptyFormatsWithCookies =
        "Сайт не отдал ни одной дорожки — похоже, из-за cookies.\n\nВерните в списке \
         «Вход на сайт» пункт «Не использовать» и попробуйте снова. YouTube почти всегда \
         отвечает так на запрос с cookies: он переключается на урезанный ответ, в котором \
         дорожек нет вовсе. Включать cookies стоит только для тех роликов, которые без них \
         не скачиваются.",
        "The site returned no tracks at all — cookies look like the reason.\n\nSet the \
         “Site login” list back to “Do not use” and try again. YouTube almost always answers \
         a request with cookies this way: it switches to a stripped-down reply in which there \
         are no tracks at all. Cookies are worth turning on only for those videos that do not \
         download without them.",
        "Կայքը ոչ մի հոսք չտվեց — ըստ ամենայնի cookies-ի պատճառով։\n\nՎերադարձրեք «Մուտք \
         կայք» ցանկում «Չօգտագործել» կետը և նորից փորձեք։ YouTube-ը գրեթե միշտ այդպես է \
         պատասխանում cookies-ով հարցմանը. այն անցնում է կրճատ պատասխանի, որում հոսքեր \
         ընդհանրապես չկան։ Cookies-ը միացնելն արժե միայն այն տեսանյութերի համար, որոնք \
         առանց դրա չեն ներբեռնվում։";
    FailCookieDbLocked =
        "Браузер не отдал cookies: файл занят.\n\nПока браузер работает, он держит свою \
         базу cookies открытой, и прочитать её нельзя. Закройте браузер полностью — вместе \
         со значком в области уведомлений — и попробуйте снова.",
        "The browser did not give up its cookies: the file is busy.\n\nWhile the browser is \
         running it keeps its cookie database open, and it cannot be read. Close the browser \
         completely — together with the icon in the notification area — and try again.",
        "Բրաուզերը cookies չտվեց. ֆայլը զբաղված է։\n\nՔանի դեռ բրաուզերն աշխատում է, այն \
         իր cookies-ի բազան պահում է բաց, և այն կարդալ հնարավոր չէ։ Փակեք բրաուզերն \
         ամբողջությամբ՝ ծանուցումների տիրույթի պատկերակի հետ միասին, և նորից փորձեք։";
    FailCookieDpapi =
        "Этот браузер не отдаёт cookies.\n\nChrome и браузеры на его основе (Edge, Brave, \
         Opera, Vivaldi) в свежих версиях шифруют cookies так, что снаружи их не прочитать. \
         Это защита самого браузера, обойти её Savio не может. Выходов два: выбрать в списке \
         Mozilla Firefox — его cookies читаются — или выгрузить cookies из этого же браузера \
         расширением вроде «Get cookies.txt» и указать в списке «Из файла…» то, что оно \
         сохранит.",
        "This browser does not give up its cookies.\n\nChrome and the browsers based on it \
         (Edge, Brave, Opera, Vivaldi) encrypt cookies in recent versions so that they cannot \
         be read from outside. That is the browser's own protection, and Savio cannot get \
         around it. There are two ways out: pick Mozilla Firefox in the list — its cookies can \
         be read — or export the cookies from this same browser with an extension such as \
         “Get cookies.txt” and point the “From a file…” entry at what it saves.",
        "Այս բրաուզերը cookies չի տալիս։\n\nChrome-ը և դրա հիման վրա ստեղծված բրաուզերները \
         (Edge, Brave, Opera, Vivaldi) վերջին տարբերակներում cookies-ը գաղտնագրում են \
         այնպես, որ դրսից կարդալ հնարավոր չէ։ Սա հենց բրաուզերի պաշտպանությունն է, և \
         Savio-ն չի կարող այն շրջանցել։ Ելքը երկուսն է՝ ցանկից ընտրել Mozilla Firefox — \
         դրա cookies-ը կարդացվում է — կամ նույն բրաուզերից cookies-ը արտահանել «Get \
         cookies.txt» տիպի ընդլայնմամբ և ցանկի «Ֆայլից…» կետում նշել այն, ինչ այն կպահի։";
    FailCookieDbMissing =
        "В этом браузере cookies не нашлись.\n\nSavio не нашёл его базу cookies: скорее \
         всего браузер не установлен или вы ни разу его не открывали. Выберите тот браузер, \
         в котором открыт нужный сайт.",
        "No cookies were found in this browser.\n\nSavio did not find its cookie database: \
         most likely the browser is not installed, or you have never opened it. Pick the \
         browser in which the site you need is open.",
        "Այս բրաուզերում cookies չգտնվեց։\n\nSavio-ն չգտավ դրա cookies-ի բազան. ըստ \
         ամենայնի բրաուզերը տեղակայված չէ կամ դուք այն երբեք չեք բացել։ Ընտրեք այն \
         բրաուզերը, որում բաց է անհրաժեշտ կայքը։";
    FailCookieFileNotNetscape =
        "Выбранный файл — не файл cookies.\n\nНужен текстовый файл формата Netscape: такой \
         выгружает расширение браузера, например «Get cookies.txt», кнопкой «Экспорт». Тем \
         же ответом кончается и пустой файл. Выберите в строке под списком другой файл — \
         или верните пункт «Не использовать».",
        "The chosen file is not a cookies file.\n\nA text file in the Netscape format is \
         needed: a browser extension such as “Get cookies.txt” saves one with its “Export” \
         button. An empty file ends with the same answer. Pick another file in the row under \
         the list — or set the list back to “Do not use”.",
        "Ընտրված ֆայլը cookies ֆայլ չէ։\n\nՊետք է Netscape ձևաչափի տեքստային ֆայլ. \
         այդպիսին արտահանում է բրաուզերի ընդլայնումը, օրինակ «Get cookies.txt»-ը՝ «Export» \
         կոճակով։ Նույն պատասխանով է ավարտվում նաև դատարկ ֆայլը։ Ցանկի տակի տողում ընտրեք \
         այլ ֆայլ կամ վերադարձրեք «Չօգտագործել» կետը։";
    FailCookieFileWrite =
        "В файл cookies не удалось записать.\n\nПосле работы yt-dlp дописывает в этот файл \
         свежие cookies — и не смог: скорее всего у файла стоит «Только чтение» либо он \
         лежит там, куда писать нельзя. Ролик при этом, скорее всего, уже скачан — \
         загляните в папку сохранения. Снимите с файла защиту от записи или скопируйте его \
         в обычную папку.",
        "Writing to the cookies file failed.\n\nAfter its work yt-dlp appends fresh cookies \
         to this file — and could not: most likely the file is marked “Read-only”, or it lies \
         somewhere that cannot be written to. The video itself has most likely already been \
         downloaded — look into the save folder. Clear the write protection on the file, or \
         copy it into an ordinary folder.",
        "Cookies ֆայլում գրել չհաջողվեց։\n\nԱշխատանքից հետո yt-dlp-ն այդ ֆայլին ավելացնում \
         է թարմ cookies — և չկարողացավ. ըստ ամենայնի ֆայլի վրա դրված է «Միայն կարդալու», \
         կամ այն ընկած է այնտեղ, ուր գրել չի կարելի։ Տեսանյութն այդ դեպքում, ըստ ամենայնի, \
         արդեն ներբեռնված է — նայեք պահպանման թղթապանակը։ Ֆայլից հանեք գրելու \
         պաշտպանությունը կամ պատճենեք այն սովորական թղթապանակ։";
    FailNotABot =
        "Сайт требует подтвердить, что вы не робот.\n\nТак отвечают, когда с вашего адреса \
         приходит слишком много запросов. Нажмите «Обновить движок» и попробуйте снова через \
         несколько минут. Если включён VPN — выключите его: одним адресом пользуются многие, \
         и проверка на нём срабатывает чаще. А если вы вошли на этот сайт в браузере — \
         выберите его в списке «Вход на сайт».",
        "The site asks you to confirm that you are not a robot.\n\nThat is the answer when \
         too many requests come from your address. Press “Update the engine” and try again in \
         a few minutes. If a VPN is on, turn it off: one address is used by many people, and \
         the check fires on it more often. And if you are signed in to this site in a browser, \
         pick that browser in the “Site login” list.",
        "Կայքը պահանջում է հաստատել, որ դուք ռոբոտ չեք։\n\nԱյդպես պատասխանում են, երբ ձեր \
         հասցեից չափից շատ հարցումներ են գալիս։ Սեղմեք «Թարմացնել շարժիչը» և մի քանի րոպեից \
         նորից փորձեք։ Եթե VPN-ը միացված է, անջատեք այն. մեկ հասցեից օգտվում են շատերը, և \
         ստուգումն այնտեղ ավելի հաճախ է գործում։ Իսկ եթե դուք այս կայք մուտք եք գործել \
         բրաուզերում, ընտրեք այն «Մուտք կայք» ցանկում։";
    FailAgeRestricted =
        "Видео с возрастным ограничением.\n\nСайт отдаёт его только тем, кто вошёл в аккаунт. \
         Выберите в списке «Вход на сайт» тот браузер, где вы вошли, — Savio возьмёт вход \
         оттуда. Иногда помогает и «Обновить движок»: свежий yt-dlp обходит часть таких \
         проверок.",
        "The video is age-restricted.\n\nThe site gives it only to those who are signed in. \
         Pick the browser you are signed in with in the “Site login” list — Savio will take \
         the login from there. “Update the engine” sometimes helps too: a fresh yt-dlp gets \
         around some of these checks.",
        "Տեսանյութը տարիքային սահմանափակմամբ է։\n\nԿայքը տալիս է այն միայն նրանց, ովքեր \
         մուտք են գործել հաշիվ։ «Մուտք կայք» ցանկում ընտրեք այն բրաուզերը, որտեղ մուտք եք \
         գործել — Savio-ն մուտքը կվերցնի այնտեղից։ Երբեմն օգնում է նաև «Թարմացնել \
         շարժիչը». թարմ yt-dlp-ն շրջանցում է նման ստուգումների մի մասը։";
    FailPrivateVideo =
        "Доступ к видео закрыт.\n\nВладелец сделал его приватным — оно отдаётся только тем, \
         кому он открыл доступ. Если доступ открыт вам, выберите в списке «Вход на сайт» тот \
         браузер, где вы вошли в аккаунт. Иначе остаётся поискать открытую копию по другой \
         ссылке.",
        "Access to the video is closed.\n\nThe owner made it private — it is given only to \
         those they granted access to. If access is open to you, pick the browser you are \
         signed in with in the “Site login” list. Otherwise all that is left is to look for an \
         open copy at another link.",
        "Տեսանյութի հասանելիությունը փակ է։\n\nՍեփականատերն այն դարձրել է մասնավոր — տրվում \
         է միայն նրանց, ում նա հասանելիություն է տվել։ Եթե հասանելիությունը ձեզ բաց է, \
         «Մուտք կայք» ցանկում ընտրեք այն բրաուզերը, որտեղ մուտք եք գործել հաշիվ։ Հակառակ \
         դեպքում մնում է այլ հղումով բաց պատճեն փնտրել։";
    FailGeoBlocked =
        "Видео недоступно в вашей стране.\n\nСайт закрыл его по региону — дело не в ссылке \
         и не в Savio. Помогает только смена региона: VPN или прокси, включённые до начала \
         загрузки.",
        "The video is not available in your country.\n\nThe site closed it by region — it is \
         neither the link nor Savio. Only changing the region helps: a VPN or a proxy turned \
         on before the download starts.",
        "Տեսանյութը ձեր երկրում հասանելի չէ։\n\nԿայքը փակել է այն ըստ տարածաշրջանի — խնդիրը \
         ո՛չ հղումն է, ո՛չ Savio-ն։ Օգնում է միայն տարածաշրջանի փոփոխությունը՝ VPN կամ \
         պրոքսի, միացված մինչև ներբեռնման սկիզբը։";
    FailForbidden403 =
        "Сервер отказал в доступе (ошибка 403).\n\nЧаще всего это значит, что сайт сменил \
         защиту и движок устарел — нажмите «Обновить движок». Если не помогло, откройте \
         страницу заново и скопируйте ссылку: прежняя могла быть одноразовой и уже истечь.",
        "The server refused access (error 403).\n\nMost often this means the site changed its \
         protection and the engine is out of date — press “Update the engine”. If that did not \
         help, open the page again and copy the link: the old one may have been one-off and \
         already expired.",
        "Սերվերը մերժեց հասանելիությունը (սխալ 403)։\n\nԱմենից հաճախ սա նշանակում է, որ \
         կայքը փոխել է պաշտպանությունը, և շարժիչը հնացել է — սեղմեք «Թարմացնել շարժիչը»։ \
         Եթե չօգնեց, բացեք էջը նորից և պատճենեք հղումը. նախորդը կարող էր լինել միանվագ և \
         արդեն սպառված։";

    FailCode101 =
        "загрузка остановлена (лимит или файл уже есть)",
        "the download was stopped (a limit, or the file already exists)",
        "ներբեռնումը կանգնեցվել է (սահմանաչափ կամ ֆայլն արդեն կա)";
    FailCode2 =
        "yt-dlp не принял аргументы — это баг Savio",
        "yt-dlp did not accept the arguments — this is a Savio bug",
        "yt-dlp-ն չընդունեց արգումենտները — սա Savio-ի սխալ է";
    FailNoDetails =
        "yt-dlp завершился с ошибкой без подробностей",
        "yt-dlp exited with an error without any details",
        "yt-dlp-ն ավարտվեց սխալով՝ առանց մանրամասների";
    /// `{}` — код возврата, `{}` — короткая подсказка.
    FailWithHint = "Ошибка (код {}): {}", "Error (code {}): {}", "Սխալ (կոդ {})՝ {}";
    /// `{}` — код возврата, `{}` — сырой хвост stderr.
    FailWithTail = "Ошибка (код {}):\n{}", "Error (code {}):\n{}", "Սխալ (կոդ {})՝\n{}";

    /// `{}` — имя постобработчика, как его называет сам yt-dlp.
    StageProcessing = "Обработка: {}", "Processing: {}", "Մշակում՝ {}";
    /// Подставляется, когда yt-dlp имени не назвал.
    StageProcessingUnnamed = "обработка", "processing", "մշակում";
    /// Встаёт в журнале на месте неразобранного пути к файлу cookies.
    LogCookieFilePlaceholder = "файл cookies", "cookies file", "cookies ֆայլ";

    // Своя обрезка. Всё это уезжает внутрь `WarnCutFailed` или в журнал.
    CutFfmpegLaunch =
        "не удалось запустить ffmpeg: {}", "could not start ffmpeg: {}",
        "չհաջողվեց գործարկել ffmpeg-ը՝ {}";
    CutNoStderr = "у ffmpeg нет stderr", "ffmpeg has no stderr", "ffmpeg-ը stderr չունի";
    CutFfmpegLost = "ffmpeg потерян", "ffmpeg is lost", "ffmpeg-ը կորել է";
    /// `{}` — код возврата.
    CutFfmpegFailed =
        "ffmpeg завершился с ошибкой (код {})", "ffmpeg exited with an error (code {})",
        "ffmpeg-ն ավարտվեց սխալով (կոդ {})";
    /// `{}` — код возврата, `{}` — первые строки ругани.
    CutFfmpegFailedWithTail =
        "ffmpeg завершился с ошибкой (код {}):\n{}",
        "ffmpeg exited with an error (code {}):\n{}",
        "ffmpeg-ն ավարտվեց սխալով (կոդ {})՝\n{}";
    CutNoOutput =
        "ffmpeg отчитался об успехе, а файла с фрагментом не оставил",
        "ffmpeg reported success but left no file with the fragment",
        "ffmpeg-ը հաջողության մասին հաղորդեց, բայց հատվածով ֆայլ չթողեց";
    CutRenameFailed =
        "не удалось заменить файл вырезанным куском: {}",
        "could not replace the file with the cut piece: {}",
        "չհաջողվեց ֆայլը փոխարինել կտրված կտորով՝ {}";
    CutLeftoverWholeFile =
        "Скачанный целиком ролик не удалось убрать после отмены: {}",
        "The video downloaded in full could not be removed after the cancellation: {}",
        "Ամբողջությամբ ներբեռնված տեսանյութը չհաջողվեց հեռացնել չեղարկումից հետո՝ {}";

    /// `{}` — ширина, `{}` — высота, `{}` — длина буфера в байтах.
    CoverImplausible =
        "обложка разобрана неправдоподобно: {}×{} при {} байтах",
        "the thumbnail decoded implausibly: {}×{} with {} bytes",
        "շապիկը վերծանվեց անհավանական կերպով՝ {}×{} {} բայթի դեպքում";
}

/// Подставляет значения в шаблон вместо `{}`, по порядку.
///
/// Своя подстановка, а не `format!`, потому что `format!` требует строку-литерал
/// на этапе компиляции, а шаблон приезжает из таблицы переводов. Порядок
/// подстановки — порядок `{}` в самом шаблоне, и это главное, ради чего она
/// вообще нужна: в русском «осталось 5 минут», а в армянском число и единица
/// стоят иначе, и склеивать строку кусками на стороне вызова значило бы
/// намертво зашить русский порядок слов во все три языка.
///
/// Лишний `{}` (значений меньше, чем мест) остаётся в строке как есть:
/// видимая дыра лучше молчаливо съеденного куска текста.
///
/// Собирает `String`, поэтому в кадре отрисовки ей не место — зовут её из
/// обработчиков событий, как и весь остальной сбор готовых строк.
pub fn fill(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + args.iter().map(|a| a.len()).sum::<usize>());
    let mut rest = template;
    let mut args = args.iter();
    while let Some(at) = rest.find("{}") {
        let Some(arg) = args.next() else {
            break;
        };
        out.push_str(&rest[..at]);
        out.push_str(arg);
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// Форма слова при числительном.
///
/// Три формы, потому что их требует русский: «1 день, 2 дня, 5 дней». У
/// английского форм две, и `few` он не использует вовсе. У армянского при
/// числительном существительное остаётся в единственном числе («5 օր»), то
/// есть ему всегда годится `one` — и это не упрощение, а правило языка:
/// «5 օրեր» звучит так же неправильно, как «5 день».
pub fn plural(
    lang: Lang,
    n: u64,
    one: &'static str,
    few: &'static str,
    many: &'static str,
) -> &'static str {
    match lang {
        Lang::Ru => {
            // Одиннадцать-четырнадцать — исключение: «11 дней», а не «11 день».
            // Без этой проверки правило по последней цифре ошибается на каждом
            // втором десятке.
            if (11..=14).contains(&(n % 100)) {
                return many;
            }
            match n % 10 {
                1 => one,
                2..=4 => few,
                _ => many,
            }
        }
        Lang::En => {
            if n == 1 {
                one
            } else {
                many
            }
        }
        Lang::Am => one,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пустая подпись в окне выглядит поломкой, а не пропущенным переводом,
    /// и найти её можно только глазами и только на том языке, где её забыли.
    /// Таблица большая, и пропуск в ней — самая вероятная ошибка задачи.
    #[test]
    fn every_key_says_something_in_every_language() {
        for (key, name) in ALL_KEYS {
            for lang in Lang::ALL {
                assert!(
                    !t(lang, *key).trim().is_empty(),
                    "{name} на {lang:?}: пустая строка"
                );
            }
        }
    }

    /// Кавычки у каждого языка свои, и чужие в переводе заметны сразу.
    ///
    /// Перевод пишется копированием русской строки, так что «ёлочки» уезжают
    /// в английский текст целыми пачками: «Read» вместо “Read”. Ошибка не
    /// ломает ничего и потому живёт долго — на неё натыкается только тот, кто
    /// читает окно по-английски. По-армянски «ёлочки» как раз уместны:
    /// армянская типографика их и использует.
    /// Ни в одной подписи нет дыры из нескольких пробелов подряд.
    ///
    /// Длинные строки в этой таблице переносятся обратным слэшем на конце
    /// строки: Rust съедает и перенос, и отступ следующей строки. Стоит
    /// слэшу пропасть — и отступ остаётся в тексте: «проверьте, что
    /// ␣␣␣␣␣␣␣␣␣это тот ролик». В коде это не видно вовсе (строка выглядит
    /// ровно так же, как соседние), сборка молчит, а в окне посреди фразы
    /// зияет провал в полслова.
    ///
    /// Ровно так и случилось с десятком строк приветствия, и нашлось это
    /// глазами на снимке экрана. Проверка стоит доли секунды и ловит всю
    /// семью промахов разом: два пробела подряд в подписи не нужны никогда.
    #[test]
    fn no_string_has_a_gap_in_it() {
        let mut bad = Vec::new();
        for (key, name) in ALL_KEYS {
            for lang in Lang::ALL {
                let text = t(lang, *key);
                if text.contains("  ") {
                    bad.push(format!("{name} ({lang:?}): «{text}»"));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "в подписях дыра из пробелов — потерян перенос строки:
{}",
            bad.join("
")
        );
    }

    /// Список собирается целиком, а не падает на первом: правится это одной
    /// вычиткой, и узнавать про следующую строку отдельным прогоном — значит
    /// прогонять тесты столько раз, сколько в таблице промахов.
    #[test]
    fn each_language_uses_its_own_quotation_marks() {
        let mut wrong = Vec::new();
        for (key, name) in ALL_KEYS {
            let en = t(Lang::En, *key);
            if en.contains('\u{ab}') || en.contains('\u{bb}') {
                wrong.push(format!("{name} (англ.): {en}"));
            }
            let ru = t(Lang::Ru, *key);
            if ru.contains('\u{201c}') || ru.contains('\u{201d}') {
                wrong.push(format!("{name} (рус.): {ru}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "кавычки не того языка в {} строках:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    /// Шаблон с подстановкой обязан иметь одинаковое число мест во всех трёх
    /// языках: потерянное `{}` — это молча пропавшее в строке число, а лишнее
    /// оставляет в окне видимую «{}». Порядок мест при этом свой у каждого
    /// языка, и проверять его нечем — на то и подстановка.
    #[test]
    fn a_template_keeps_its_placeholders_in_every_language() {
        for (key, name) in ALL_KEYS {
            let places = |lang| t(lang, *key).matches("{}").count();
            let ru = places(Lang::Ru);
            for lang in [Lang::En, Lang::Am] {
                assert_eq!(
                    places(lang),
                    ru,
                    "{name} на {lang:?}: мест для подстановки {}, а в русском {ru}",
                    places(lang)
                );
            }
        }
    }

    /// Коды языков уходят в файл настроек и читаются оттуда обратно.
    /// Совпавшие коды означали бы, что один из языков невозможно запомнить.
    #[test]
    fn language_codes_survive_a_round_trip() {
        let mut seen: Vec<&str> = Vec::new();
        for lang in Lang::ALL {
            let code = lang.code();
            assert_eq!(Lang::from_code(code), Some(lang));
            assert!(!seen.contains(&code), "{code}: код повторяется");
            seen.push(code);
        }
        assert_eq!(Lang::from_code("zz"), None);
        assert_eq!(Lang::default(), Lang::Ru);
    }

    /// Подпись переключателя человек ищет глазами. Одинаковые подписи
    /// превратили бы выбор языка в угадайку.
    #[test]
    fn language_labels_are_distinct() {
        let mut seen: Vec<&str> = Vec::new();
        for lang in Lang::ALL {
            let label = lang.label();
            assert!(!label.trim().is_empty(), "{lang:?}: пустая подпись");
            assert!(!seen.contains(&label), "{label}: подпись повторяется");
            seen.push(label);
        }
    }

    #[test]
    fn fill_puts_values_where_the_template_asks() {
        assert_eq!(fill("{} из {}", &["5", "10"]), "5 из 10");
        assert_eq!(fill("без мест", &["5"]), "без мест");
        // Значений меньше, чем мест: остаток шаблона виден, а не съеден.
        assert_eq!(fill("{} и {}", &["раз"]), "раз и {}");
        // Лишние значения просто не нужны.
        assert_eq!(fill("{}", &["раз", "два"]), "раз");
        // Порядок слов у языков разный — на то и подстановка, а не склейка.
        assert_eq!(fill("осталось {} {}", &["5", "минут"]), "осталось 5 минут");
    }

    #[test]
    fn plural_follows_each_language_rules() {
        let ru = |n| plural(Lang::Ru, n, "день", "дня", "дней");
        assert_eq!(ru(1), "день");
        assert_eq!(ru(2), "дня");
        assert_eq!(ru(5), "дней");
        assert_eq!(ru(11), "дней", "одиннадцать — исключение");
        assert_eq!(ru(21), "день");
        assert_eq!(ru(112), "дней");

        let en = |n| plural(Lang::En, n, "day", "days", "days");
        assert_eq!(en(1), "day");
        assert_eq!(en(0), "days");
        assert_eq!(en(5), "days");

        // В армянском существительное при числительном не меняется.
        for n in [0, 1, 2, 5, 11, 21] {
            assert_eq!(plural(Lang::Am, n, "օր", "օր", "օր"), "օր");
        }
    }
}
