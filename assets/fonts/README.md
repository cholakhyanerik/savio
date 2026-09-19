# Шрифты Savio

Шесть гарнитур, девять файлов. Все под лицензией SIL Open Font License 1.1
(текст — в [OFL.txt](OFL.txt)), поэтому их можно вшивать в исполняемый файл;
именно так они и попадают в сборку — через `include_bytes!` в
[../../src/theme.rs](../../src/theme.rs).

| Файл | Гарнитура | Где используется | Письменность |
|---|---|---|---|
| `Caprasimo-Regular.ttf` | Caprasimo | заголовки | латиница |
| `KellySlab-Regular.ttf` | Kelly Slab | заголовки | кириллица |
| `NotoSerifArmenian-Regular.ttf` | Noto Serif Armenian | заголовки | армянская |
| `Figtree-Regular.ttf` | Figtree 400 | основной текст | латиница |
| `Nunito-Regular.ttf` | Nunito 400 | основной текст | кириллица |
| `NotoSansArmenian-Regular.ttf` | Noto Sans Armenian | основной текст | армянская |
| `Figtree-Bold.ttf` | Figtree 700 | полужирный | латиница |
| `Nunito-Bold.ttf` | Nunito 700 | полужирный | кириллица |
| `NotoSansArmenian-Bold.ttf` | Noto Sans Armenian 700 | полужирный | армянская |

Тройки не для красоты: кириллицы нет ни в Caprasimo, ни в Figtree, а
армянской письменности — ни в одной из четырёх. egui подбирает шрифт на
каждый знак отдельно, идя по списку семейства сверху вниз, так что латиница
набирается первой гарнитурой, кириллица — второй, армянская — третьей. То же
делает браузер со списком `font-family` из макета.

Что каждое семейство умеет нарисовать все три алфавита, держит тест
`every_family_can_draw_all_three_alphabets` в [../../src/theme.rs](../../src/theme.rs):
отсутствующий глиф рисуется пустым прямоугольником, и ни сборка, ни `clippy`
этого не видят.

## Откуда взялись файлы

Caprasimo, Kelly Slab и оба Noto Armenian скачаны из репозитория Google Fonts
как есть — они там статические:

```
https://raw.githubusercontent.com/google/fonts/main/ofl/caprasimo/Caprasimo-Regular.ttf
https://raw.githubusercontent.com/google/fonts/main/ofl/kellyslab/KellySlab-Regular.ttf
https://raw.githubusercontent.com/google/fonts/main/ofl/notosansarmenian/static/NotoSansArmenian-Regular.ttf
https://raw.githubusercontent.com/google/fonts/main/ofl/notosansarmenian/static/NotoSansArmenian-Bold.ttf
https://raw.githubusercontent.com/google/fonts/main/ofl/notoserifarmenian/static/NotoSerifArmenian-Regular.ttf
```

Именно `static/`, а не переменные файлы рядом, и по той же причине, что
у Figtree с Nunito ниже: `ab_glyph` вариаций не применяет.

Figtree и Nunito Google Fonts выкладывает **только переменными**, и брать их
как есть нельзя. Умолчание оси `wght` у них не 400, а 300 и 200
соответственно, а `ab_glyph` (им рисует egui) вариаций не применяет и берёт
мастер по умолчанию. То есть переменный файл дал бы светлое начертание вместо
обычного — молча, без единой ошибки сборки, и заметить это можно было бы
только глазами.

Поэтому файлы здесь — статические экземпляры, снятые `fonttools`:

```
python -m fontTools.varLib.instancer "Figtree[wght].ttf" wght=400 -o Figtree-Regular.ttf
python -m fontTools.varLib.instancer "Figtree[wght].ttf" wght=700 -o Figtree-Bold.ttf
python -m fontTools.varLib.instancer "Nunito[wght].ttf"  wght=400 -o Nunito-Regular.ttf
python -m fontTools.varLib.instancer "Nunito[wght].ttf"  wght=700 -o Nunito-Bold.ttf
```

Исходники — оттуда же:

```
https://raw.githubusercontent.com/google/fonts/main/ofl/figtree/Figtree%5Bwght%5D.ttf
https://raw.githubusercontent.com/google/fonts/main/ofl/nunito/Nunito%5Bwght%5D.ttf
```

`fonttools` нужен только чтобы обновить файлы, в сборке Savio его нет.

## Чего в этих шрифтах нет

Ни в Kelly Slab, ни в Nunito нет стрелок `→ ↓ ↑`; в Figtree они есть, и
именно поэтому Figtree стоит в списке первым. У Caprasimo нет ещё и точки
`·`, которой Savio разделяет части строк, — её даёт Kelly Slab.

У шрифтов Noto Armenian нет ни кириллицы, ни стрелок: они стоят в списках
последними и добирают ровно то, чего нет у первых двух. Поэтому менять
порядок внутри семейства нельзя — поставь Noto первым, и латиница с
кириллицей набрались бы им.

Проверять покрытие надо до того, как знак попадёт в текст интерфейса:
отсутствующий глиф рисуется пустым прямоугольником, и этого не видят ни
сборка, ни `clippy`, ни тесты (Правило 4 в [CLAUDE.md](../../CLAUDE.md)).
