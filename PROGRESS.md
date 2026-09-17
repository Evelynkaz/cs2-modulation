# Прогресс

## Этап 0 — подготовка (в работе)

### Сделано

Референсы склонированы вне репозитория в `D:\porject\refs\`
(cs2-smoke-solver @9c9ba1a, ValveResourceFormat @8a322d7, ValvePak,
ValveKeyValue); изучены документы и исходники референса; написаны
docs/ARCHITECTURE.md и docs/FORMATS.md; скелет Cargo workspace из 8 крейтов;
CLI `cs2mod` с заглушками 11 команд; .gitignore/.gitattributes; CI (fmt,
clippy, test на Ubuntu и Windows); scripts/cargo-msvc.cmd; README, NOTICE.

### Проверено

Локально на Windows: `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo test --workspace` (тестов пока нет),
`cs2mod --help`. CI на GitHub — после первого push.

### Осталось

Согласовать план с пользователем; путь к установке CS2; первый push; этап 1
(VPK + ресурсы + KV3).

### Решения

(1) добавлен крейт `extract`: политика CS2 отделена от парсера `s2fmt`;
(2) BVH + сравнение с однородной сеткой референса бенчмарком на этапе 3;
(3) `f32` в симуляции как в движке;
(4) калибровка без плагинов на сервере: константы референса + офлайн replay
его корпуса валидации (~10 тыс. бросков, вне репозитория) + ручные getpos;
(5) `sim::player` — потиковый хулл игрока для прыжков у наклонов/углов/
потолков (у референса нет);
(6) стабильность jump-бросков включает тайминг выпуска;
(7) лицензия cs2-smoke-solver — PolyForm Noncommercial, а не MIT (см.
NOTICE.md);
(8) кэш инвалидируется по хэшам содержимого, детерминированный порядок
результатов;
(9) zstd/lz4 — чистый Rust (`ruzstd`, `lz4_flex`).
