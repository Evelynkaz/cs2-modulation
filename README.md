# cs2-modulation

Калькулятор раскидок (lineups) для CS2 на Rust: по точке на карте находит
все броски smoke/flash/HE/molotov/incendiary/decoy, выдаёт
`setpos …; setang …`, тип броска (stand, walk, run, crouch, jump, run-jump),
клик, траекторию, точку приземления и стабильность. Веб-вьювер даёт
2D-радар и 3D-сцену коллизий.

## Статус

Этапы 0–3 сделаны: скелет workspace, документы, CI, VPK, контейнер
ресурсов и KV3 (`s2fmt`), физика, entity lump'ы, world node'ы, `.nav` и
извлечение геометрии карты (`extract`), а также геометрические запросы —
`UniformGrid`, `Bvh`, `VoxelGrid` (`geom`). Для разработки доступны команды
`cs2mod vpk
ls|cat|verify` (например, `cs2mod vpk ls pak01_dir.vpk`), `cs2mod res
[--vpk] [--block] [--kv3]` (например, `cs2mod res world_physics.vmdl_c
--kv3`), а также:

```
cs2mod extract de_mirage
cs2mod info de_mirage
cs2mod export-obj de_mirage --filter grenade --out cache/obj/de_mirage_grenade.obj
```

Этап 4b добавил симуляцию броска/дыма/линии видимости и офлайн-инструменты
калибровки:

```
cs2mod throw de_mirage --pos 500,-800,-160 --ang -10,45 --type jump
cs2mod smoke de_mirage --rest -500,-1400,250 --params full
cs2mod sightline de_mirage --from ... --to ... --rest ...
cs2mod replay --all                       # весь корпус validation референса
cs2mod calibrate de_mirage --throws data/throws.json --unfreeze speed,jumpv
```

Константы броска (`ThrowConstants`) по умолчанию берутся из `sim`; если
рядом лежит `data/throw-constants.json` (или передан `--constants <path>`),
он подхватывается автоматически, о чём печатается строка `throw constants:
<path>` — как у референса (`MeshSetup.cs:LoadConstants`). `calibrate` по
умолчанию ничего не подбирает: все константы заморожены (`physics-sim.md`
референса называет измеренными и `ThrowSpeed`, и клики, и `JumpVelocity` —
неконтролируемый фит по броскам однажды испортил гравитацию до `0.34`);
подбор конкретной константы включается явно через `--unfreeze
speed,jumpv,...`, и `--out` по умолчанию пишет в `data/throw-constants.json`
(`config/` отклонит guard записи — это каталог не для игровых/калиброванных
данных). Коды возврата (как у референса):

| Команда | `0` | `2` | иначе |
|---|---|---|---|
| `throw` | бросок долетел | `result.lost` | — |
| `smoke` | объём дыма не пуст | объём дыма пуст | — |
| `sightline` без `--rest` | геометрия не блокирует | геометрия блокирует | — |
| `sightline` с `--rest` | `NOT blocked by smoke` | `BLOCKED by smoke` (`smoke_cells_crossed >= 3`, независимо от точной геометрии) | — |
| `replay` | — | — | `1`, если не нашлось ни одного зачётного броска |

Подробности и текущее состояние — в [`PROGRESS.md`](PROGRESS.md).

## Жёсткие правила

- Никаких файлов игры в git: `data/` и `cache/` — в `.gitignore`.
- Никакого взаимодействия с запущенной игрой — только файлы на диске.
  Проверка бросков — вручную, на локальном практис-сервере через
  `setpos`/`setang`.
- Любая физическая константа — со ссылкой на источник в комментарии или
  пометкой `TODO: calibrate`.

## Структура

| Крейт / каталог | Назначение |
|---|---|
| `crates/s2fmt` | Форматы Source 2: VPK, контейнер `*_c`, KV3 (binary/text), физика, entity lump'ы, world nodes, CS2 `.nav`. Чистый парсер без знаний о геймплее CS2. |
| `crates/geom` | Геометрия коллизий: `TriMesh` с атрибутами на треугольник, фильтры атрибутов, BVH (raycast/sweep), воксельные сетки, экспорт в OBJ. |
| `crates/extract` | Политика извлечения карты CS2: мёрж world physics, твёрдых brush-энтити, статических пропов и бьющегося стекла в один меш коллизий; кэш по версии игры. |
| `crates/sim` | Потиковая симуляция гранаты (тик 1/64с, 2 физических подшага), детонация по типу гранаты, объём дыма и линии видимости. |
| `crates/solver` | Обратный поиск раскидок: standspots, грубый перебор по вокселям, уточнение, точная проверка с шевелением прицела, оценка стабильности, ранжирование. |
| `crates/calib` | Калибровка констант броска по замерам (`throws.json`, совместим с cs2-smoke-solver) и офлайн-replay корпусов бросков. |
| `crates/server` | HTTP API (axum) и статика веб-вьювера, долгие задачи solve с прогрессом. |
| `crates/cli` | CLI `cs2mod` (clap): extract, info, throw, solve, standspots, calibrate, replay, serve, export-obj и другие команды. |
| `viewer/` | Веб-вьювер без сборщика: 2D-радар (canvas) и 3D-сцена (three.js) для результатов. |
| `config/` | Конфиги и переопределения констант. |
| `docs/` | Документация: архитектура и форматы файлов. |
| `scripts/` | Вспомогательные скрипты сборки. |

## Сборка

Требуется Rust, закреплённый в `rust-toolchain.toml` (1.98.1, `edition = "2024"`);
`rustup` подхватит его автоматически.

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Windows: если `rustc` не находит `link.exe` (например, с Visual Studio 2026 /
VS 18), собирайте через `scripts\cargo-msvc.cmd <args>`. В Git Bash
`/usr/bin/link.exe` перекрывает MSVC-линкер — собирайте из PowerShell или cmd.

## Путь к игре

Будет задаваться флагом `--game` или переменной `CS2_GAME_DIR` (появится
на этапе 2) (…\steamapps\common\Counter-Strike Global Offensive\game\csgo)
и в репозиторий не пишется.

## Документация

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — архитектура проекта.
- [`docs/FORMATS.md`](docs/FORMATS.md) — конспект форматов файлов Source 2/CS2.
- [`PROGRESS.md`](PROGRESS.md) — прогресс по этапам.
- [`NOTICE.md`](NOTICE.md) — авторство и лицензии сторонних проектов.
