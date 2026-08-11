# Шрифты Savio

Четыре гарнитуры, шесть файлов. Все под лицензией SIL Open Font License 1.1
(текст — в [OFL.txt](OFL.txt)), поэтому их можно вшивать в исполняемый файл;
именно так они и попадают в сборку — через `include_bytes!` в
[../../src/theme.rs](../../src/theme.rs).

| Файл | Гарнитура | Где используется | Кириллица |
|---|---|---|---|
| `Caprasimo-Regular.ttf` | Caprasimo | заголовки, латиница | нет |
| `KellySlab-Regular.ttf` | Kelly Slab | заголовки, кириллица | да |
| `Figtree-Regular.ttf` | Figtree 400 | основной текст, латиница | нет |
| `Figtree-Bold.ttf` | Figtree 700 | полужирный, латиница | нет |
| `Nunito-Regular.ttf` | Nunito 400 | основной текст, кириллица | да |
| `Nunito-Bold.ttf` | Nunito 700 | полужирный, кириллица | да |

Пары не для красоты: кириллицы нет ни в Caprasimo, ни в Figtree. egui
подбирает шрифт на каждый знак отдельно, идя по списку семейства сверху вниз,
так что латиница набирается первой гарнитурой, а кириллица — второй. То же
делает браузер со списком `font-family` из макета.

## Откуда взялись файлы

Caprasimo и Kelly Slab скачаны из репозитория Google Fonts как есть:

```
https://raw.githubusercontent.com/google/fonts/main/ofl/caprasimo/Caprasimo-Regular.ttf
https://raw.githubusercontent.com/google/fonts/main/ofl/kellyslab/KellySlab-Regular.ttf
```

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

Проверять покрытие надо до того, как знак попадёт в текст интерфейса:
отсутствующий глиф рисуется пустым прямоугольником, и этого не видят ни
сборка, ни `clippy`, ни тесты (Правило 4 в [CLAUDE.md](../../CLAUDE.md)).
