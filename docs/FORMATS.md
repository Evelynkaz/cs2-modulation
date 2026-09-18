# Форматы файлов Source 2 / CS2 — конспект

Конспект для реализации `crates/s2fmt` и `crates/extract`. Источник —
код референсов (MIT): [ValvePak](https://github.com/ValveResourceFormat/ValvePak),
[ValveResourceFormat](https://github.com/ValveResourceFormat/ValveResourceFormat)
(снимок `8a322d7`, 2026-09-16), [ValveKeyValue](https://github.com/ValveResourceFormat/ValveKeyValue),
а также потребитель — `cs2-smoke-solver` (`src/Extraction/MapExtractor.cs`,
`src/Sim/CollisionMesh.cs`). Пути вида `VRF/...` — относительно
`ValveResourceFormat/ValveResourceFormat/`, `VRF-R/...` —
`ValveResourceFormat/Renderer/Renderer/`, `SS/...` — `cs2-smoke-solver/`.

Всё little-endian, если не сказано иное. Пункты с пометкой **(!)** —
места, где парсеры обычно ломаются.

---

## 1. VPK (`*_dir.vpk`, `*_NNN.vpk`)

Код: `ValvePak/Package.Read.cs`, `Package.cs`, `PackageEntry.cs`,
`Package.Verify.cs`; шаблон 010 Editor — `ValvePak/Misc/VPK.bt`.

### 1.1 Заголовок

| Смещ. | Тип | Поле |
|---|---|---|
| 0 | u32 | magic `0x55AA1234` (байты `34 12 AA 55`) |
| 4 | u32 | версия: 1 или 2 |
| 8 | u32 | TreeSize |
| 12 | u32 | FileDataSectionSize (v2) |
| 16 | u32 | ArchiveMD5SectionSize (v2) |
| 20 | u32 | OtherMD5SectionSize (v2, 48 если есть) |
| 24 | u32 | SignatureSectionSize (v2) |

`HeaderSize` = 12 (v1) / 28 (v2). Версия `0x00030002` — формат Respawn,
отвергаем. Раскладка v2: заголовок, дерево, секция данных, archive MD5,
other MD5, подпись.

### 1.2 Дерево директории

Три вложенных цикла строк, завершённых `\0`; пустая строка закрывает уровень:

```
loop ext  = cstr; if "" break
  loop dir  = cstr; if "" break
    loop name = cstr; if "" break
      u32 CRC32
      u16 PreloadBytes
      u16 ArchiveIndex
      u32 EntryOffset
      u32 EntryLength
      u16 Terminator == 0xFFFF      (!) иначе ошибка
      u8[PreloadBytes] preload      сразу после 18-байтной структуры
```

- Пустые расширение/директория хранятся как `" "` (один пробел).
- Полный путь: `dir + "/" + name + "." + ext`, с пропуском частей `" "`.
  Разделитель всегда `/`, в паках Valve пути в нижнем регистре.
- **(!)** После чтения `TreeSize` заменяется фактически прочитанным
  числом байт — от него считаются смещения данных в `_dir`.

### 1.3 Где лежат данные

- `ArchiveIndex == 0x7FFF` — внутри `_dir.vpk`, абсолютное смещение
  `HeaderSize + TreeSize + EntryOffset`.
- Иначе — файл `"{base}_{index:03}.vpk"` (минимум 3 цифры), смещение
  `EntryOffset` от начала файла. `base` — путь без `.vpk` и без `_dir`.
- Содержимое файла = `preload ‖ archive[EntryOffset .. +EntryLength]`.
- CRC-32 (IEEE) считается по всему восстановленному файлу.

### 1.4 Хвостовые секции v2 (нам нужны только для `verify`)

- Archive MD5: записи по 28 байт: `u16 archive, u16 hashType (0 MD5, 1 Blake3→16 байт), u32 offset, u32 length, u8[16]`. **(!)** `archive==0 && hashType==0x8000` означает архив `0x7FFF`.
- Other MD5 (48 байт): MD5 дерева, MD5 секции archive MD5, MD5 файла до этого поля.
- Подпись: новый формат (размер 20, первый i32 == magic) или старый
  (`i32 pubKeySize, key, i32 sigSize, sig`). **(!)** Данные могут выходить
  за заявленный размер секции.

Для кэша по версии игры мы хэшируем сами `.vpk` (SHA-256), а не эти поля.

---

## 2. Контейнер ресурсов (`*_c`)

Код: `VRF/Resource/Resource.cs`, `Resource/Enums/BlockType.cs`,
`Resource/Enums/ResourceType.cs`, `Utils/ResourceTypeExtensions.cs`,
`Resource/ResourceTypes/KeyValuesOrNTRO.cs`; шаблон `Misc/010_vrf_resource.bt`.

### 2.1 Заголовок (header version 12)

| Смещ. | Тип | Поле |
|---|---|---|
| 0 | u32 | FileSize |
| 4 | u16 | HeaderVersion == 12 |
| 6 | u16 | Version (своя у типа) |
| 8 | u32 | BlockOffset — относительно позиции 8 (обычно 8 → таблица с 16) |
| 12 | u32 | BlockCount |

Отказ сразу: первый u32 `0x55AA1234` (это VPK) или `"vcs2"` (шейдер).

### 2.2 Таблица блоков (12 байт на запись)

```
u32 FourCC            'D','A','T','A' в файле
u32 RelOffset         абсолютное = позиция ЭТОГО поля + RelOffset
u32 Size
```

**(!)** VRF выбрасывает записи с `Size == 0` из списка блоков (`Resource.cs:225-228`) и индексирует `phys_data_block` / `m_nBlockIndex` из CTRL по этому **отфильтрованному** списку (`Model.cs:363-364`, `VBIB.cs:386-387`, `Resource.cs:466-471`). Считает ли движок пустые записи — не проверено. Храним оба индекса (сырой и отфильтрованный), разрешаем через отфильтрованный, как VRF. TODO: проверить на реальных `vmdl_c` с пустыми блоками.

Данные блоков выровнены по 16, паддинг может быть `"S2V"`, не нули.

### 2.3 Типы блоков

| 4CC | Что | Нам нужно |
|---|---|---|
| DATA | основной payload (KV3 или NTRO) | да |
| CTRL | KV3: `embedded_meshes`, `embedded_physics`, … | да (vmdl_c) |
| PHYS | `PhysAggregateData` (KV3) | да |
| RERL | внешние ссылки `{u64 id, offstr name}` | для NTRO |
| REDI / RED2 | edit info (binary / KV3), тип ресурса по компилятору | для определения типа |
| NTRO | схема для бинарного DATA (устарело) | фолбэк |
| MDAT, VBIB, MBUF, MVTX, MIDX, … | рендер-меши | нет (коллизии в PHYS) |
| INSG, SrMa, LaCo, STAT, FLCI, DSTF | KV3 | нет |

Неизвестные 4CC храним как сырые байты (VRF падает — мы не должны).

### 2.4 Тип ресурса и формат DATA

- Тип — по расширению без `_c`: `vmdl` Model, `vphys` PhysicsCollisionMesh,
  `vwrld` World, `vwnod` WorldNode, `vents` EntityLump, `vrman`
  ResourceManifest, `vdata` VData и т.д. (`ResourceType.cs`). Без имени —
  по `CompilerIdentifier` из REDI/RED2 (`VPhysXData` → PhysicsCollisionMesh).
- Простое правило: первые 4 байта DATA — KV3 magic → KV3; иначе есть
  блок NTRO → NTRO. Современные файлы CS2 фактически KV3.
- Сжатия всего файла нет: сжатие внутри блоков (KV3 — LZ4/zstd).

### 2.5 `vrman_c` (манифест ресурсов, путь к world_physics в VRF)

Без NTRO: `i32 version == 8` (`0,0` — пусто), `i32 count`,
`count × {i32 relOff, i32 n}` — `n` offset-строк по `entry + relOff`.

---

## 3. KV3 binary

Код: `VRF/Resource/ResourceTypes/BinaryKV3.cs` (v1–v5),
`BinaryKV3.Legacy.cs` (v0), `BinaryKV3.NodeType.cs`,
`BinaryKV3.Serialization.cs` (писатель v4/v5 — зеркало раскладки),
`VRF/Serialization/KeyValues/KV3IDLookup.cs`, `VRF/Compression/BlockCompress.cs`.
Фикстуры для сверки: `ValveResourceFormat/Tests/Files/*kv3_v*`
(+ текстовые дампы в `Tests/Files/ValidOutput`) — используем только локально
для проверки, в репозиторий не копируем.

### 3.1 Magic

| Версия | u32 | Байты в файле |
|---|---|---|
| v0 (legacy) | `0x03564B56` | `56 4B 56 03` = `VKV\x03` |
| v1..v5 | `0x4B5633NN` | `NN 33 56 4B` |

**(!)** Порядок байт: версия идёт первым байтом. Проверка:
`magic & 0xFFFFFF00 == 0x4B563300`, `version = magic & 0xFF`. Новее v5 в
VRF нет.

### 3.2 GUID'ы

16 сырых байт на диске, сравниваем побайтно (текстовая форма — mixed-endian).
Кодировки (только v0): `binary_bc` `46 1A 79 95 BC 95 6C 4F A7 0B 05 BC A1 B7 DF D2`,
`binary_lz4` `8A 34 47 68 A1 63 5C 4F A1 97 53 80 6F D9 B1 19`,
`binary` `00 05 86 1B D8 F7 C1 40 AD 82 75 A4 82 67 E7 14`.
Формат `generic` `7C 16 12 74 E9 06 98 46 AF F2 E6 3E B5 90 37 E7`.
Неизвестный формат-GUID допустим. В v1–v5 есть только формат-GUID.

### 3.3 Типы узлов

| Id | Тип | Payload |
|---|---|---|
| 1 | NULL | — |
| 2 | BOOLEAN | u8 из Bytes1 |
| 3 / 4 / 5 | INT64 / UINT64 / DOUBLE | Bytes8 |
| 6 | STRING | i32 id строки из Bytes4; −1 = "" |
| 7 | BINARY_BLOB | v1: i32 длина (Bytes4) + байты из Bytes1; v2+: длина из таблицы длин, байты из потока блобов |
| 8 | ARRAY | i32 n (Bytes4), n элементов со своими типами |
| 9 | OBJECT | n (v<5: Bytes4; v5: поток ObjectLengths), n членов |
| 10 | ARRAY_TYPED | i32 n (Bytes4), один подтип, n значений без типов |
| 11 / 12 | INT32 / UINT32 | Bytes4 |
| 13 / 14 | TRUE / FALSE | — |
| 15 / 16 | INT64 0 / 1 | — |
| 17 / 18 | DOUBLE 0 / 1 | — |
| 19 | FLOAT | f32, Bytes4 (v4+) |
| 20 / 21 | INT16 / UINT16 | Bytes2 (v4+) |
| 22 | неизвестен | VRF падает |
| 23 | INT32_AS_BYTE | u8 из Bytes1 (v4+, смысл не до конца ясен) |
| 24 | ARRAY_TYPE_BYTE_LENGTH | как 10, но n — u8 из Bytes1 |
| 25 | ARRAY_TYPE_AUXILIARY_BUFFER | v5: n u8 и подтип из текущего набора, **значения — из другого набора буферов** |

### 3.4 Флаги

- v3+: если `type & 0x80` → `type &= 0x3F`, следующий байт из потока типов —
  enum: 0 None, 1 Resource, 2 ResourceName, 3 Panorama, 4 SoundEvent,
  5 SubClass, 6 EntityName.
- v0–v2: `type &= 0x7F`, следующий байт — битовая маска: значение `0x04`
  (multiline) сбросить; остаток должен точно равняться одному из
  0, 1, 2, 8, 16, 32 (иначе ошибка — комбинации VRF не принимает):
  0 None, 1 Resource, 2 ResourceName, 8 Panorama, 16 SoundEvent, 32 SubClass.

### 3.5 v0 (`VKV\x03`)

`u32 magic, guid encoding, guid format`, дальше по кодировке:
`binary_bc` — BlockCompress; `binary_lz4` — `i32 uncompressedSize` + сырой
LZ4 block; `binary` — как есть. Распакованный поток последовательный, без
«дорожек» и выравниваний: `u32 stringCount`, строки cstr, корень
`u8 type [u8 flag]` + значение, в конце `u32 0xFFFFFFFF`.
**(!)** В v0 у члена объекта `i32 nameId` идёт **до** байта типа (в v1+ — после).

### 3.6 Заголовок v1–v5

| Смещ. | Тип | Поле | Версии |
|---|---|---|---|
| 0 | u32 | magic | все |
| 4 | u8[16] | format GUID | все |
| 20 | u32 | сжатие: 0 нет, 1 LZ4, 2 ZSTD | все |
| 24 | i32 ×3, i32 | countBytes1, countBytes4, countBytes8, sizeUncompressedTotal | **только v1** (заголовок 40; сжатый размер = размер блока − 40) |
| 24 | u16 | compressionDictionaryId (== 0) | v2+ |
| 26 | u16 | compressionFrameSize (LZ4: 16384) | v2+ |
| 28 | i32 | countBytes1 | v2+ |
| 32 | i32 | countBytes4 (включая i32 числа строк) | v2+ |
| 36 | i32 | countBytes8 | v2+ |
| 40 | i32 | countTypes: **v2–v4 байты строк + типов; v5 — только типы** (!) | v2+ |
| 44 | u16, u16 | countObjects, countArrays (не нужны) | v2+ |
| 48 | i32 | sizeUncompressedTotal | v2+ |
| 52 | i32 | sizeCompressedTotal | v2+ |
| 56 | i32 | countBlocks (число блобов) | v2+ |
| 60 | i32 | sizeBinaryBlobsBytes | v2+ |
| 64 | i32 | countBytes2 | v4+ |
| 68 | i32 | sizeBlockCompressedSizesBytes | v4+ |
| 72..84 | i32 ×4 | uncompressed/compressed buffer1, uncompressed/compressed buffer2 (0 = без сжатия) | v5 |
| 88..100 | i32 ×4 | countBytes1/2/4/8 для buffer2 | v5 |
| 104 | i32 | неизвестно | v5 |
| 108, 112 | i32, i32 | countObjects_buffer2, countArrays_buffer2 | v5 |
| 116 | i32 | неизвестно | v5 |

Размер заголовка: v1 40, v2/v3 64, v4 72, v5 120.

### 3.7 Payload

- v1–v4: `[buffer1] [блобы, если countBlocks>0] [u32 0xFFEEDD00, если countBlocks>0]`.
- v5: `[buffer1] [buffer2] [блобы] [u32 0xFFEEDD00, если countBlocks>0]`.
- LZ4 — **сырой block** (не frame), размер результата известен заранее.
- ZSTD — один frame. **(!)** В v2–v4 buffer1 и блобы сжаты **одним**
  фреймом: распаковать `sizeUncompressed + sizeBinaryBlobsBytes`, блобы — хвост.
- **(!)** Блок ресурса может содержать байты после KV3-документа (пример: pak01
  `viewmodel_inspects.vnmgraph_c` DATA — документ записан дважды); VRF их не проверяет, мы тоже.

### 3.8 Раскладка buffer1 в v1–v4

```
off = 0
Bytes1 = countBytes1 байт
если countBytes2>0: align(2); Bytes2           (v4)
если countBytes4>0: align(4); Bytes4
если countBytes8>0: align(8); Bytes8  иначе align(8)   (!) v<5 выравнивает и при пустом Bytes8
stringCount = первый i32 из Bytes4 (Bytes4 продолжается после него)
строки: stringCount × cstr от off
типы: v1 — до (конец − 4); v2–v4 — countTypes − (байты строк)
если countBlocks == 0: u32 0xFFEEDD00 и конец буфера
иначе: countBlocks × i32 длины блобов, u32 0xFFEEDD00, (LZ4) u16 размеры фреймов до конца
```

Выравнивания — относительно начала распакованного буфера.

### 3.9 Раскладка v5

- buffer1 («aux»): Bytes1 (сначала байты строк, потом aux u8),
  align2 Bytes2, align4 Bytes4 (первый i32 — stringCount), align8 Bytes8;
  **без** align(8) при пустом Bytes8.
- buffer2 («main»): `countObjects_buffer2 × i32` ObjectLengths с нуля,
  затем Bytes1, align2 Bytes2, align4 Bytes4, align8 Bytes8 (каждый — если
  count>0; выравнивание учитывает префикс ObjectLengths), затем
  countTypes байт типов без выравнивания, затем как в v1–v4 (маркер,
  длины блобов, u16 размеры LZ4-фреймов).

### 3.10 Блобы (v2+)

- без сжатия: `sizeBinaryBlobsBytes` сырых байт;
- LZ4: **цепочка** блоков по ≤16384 байт, словарь — ранее распакованный
  выход (до 64 KiB), цепочка сквозная через все блобы; размеры сжатых
  фреймов — список u16 из основного буфера (читается до конца буфера,
  а не по `sizeBlockCompressedSizesBytes`);
- ZSTD v5: отдельный фрейм размером
  `sizeCompressedTotal − compressedBuffer1 − compressedBuffer2`;
- в конце `u32 0xFFEEDD00` из файла.

### 3.11 Обход

```
ctx.Buffer = v5 ? buffer2 : buffer1;  ctx.Aux = buffer1
(t, flag) = ReadType()                       // из потока типов
root = ReadValue(t)

OBJECT: n = v5 ? pop ObjectLengths : pop i32 Bytes4
        n × { (t,f)=ReadType(); keyId = pop i32 Bytes4; ReadValue(t) }
ARRAY:  n = pop i32 Bytes4; n × { ReadType(); ReadValue }
ARRAY_TYPED (10): n = i32 Bytes4; один ReadType(); n × ReadValue
BYTE_LENGTH (24): n = u8 Bytes1;  один ReadType(); n × ReadValue
AUX (25): n = u8 Bytes1 текущего; ReadType(); swap(Buffer, Aux); n × ReadValue; swap назад
```

- **(!)** Ключ читается после типа, но из другого потока; внутри Bytes4
  id ключа идёт раньше Bytes4-данных значения.
- Каждая «дорожка» читается с начала до конца; после разбора все
  дорожки должны быть исчерпаны — это главная проверка корректности.
- ObjectLengths при AUX не переключается.

---

## 4. KV3 text

Код: `ValveKeyValue/Deserialization/KeyValues3/KV3TokenReader.cs`,
`KV3TextReader.cs`, `Serialization/KeyValues3/KV3TextSerializer.cs`;
тестовые данные `ValveKeyValue.Test/Test Data/TextKV3/*.kv3`.

```
<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->
document := header value
value    := [flag (':' | '|')]* ( object | array | blob | string | literal )
object   := '{' ( key ['='] value )* '}'
array    := '[' ( value [','] )* ']'
blob     := '#[' hex-пары через пробелы ']'
comment  := '//…' | '/* … */'
```

- Голый токен заканчивается на пробельном или `{}[]=, \t\n\r'":|;`.
  Идентификатор `[A-Za-z0-9_:.]`, за которым сразу `:` или `|`, — флаг
  (`resource`, `resource_name`, `panorama`, `soundevent`, `subclass`,
  `entity_name`); при нескольких флагах побеждает последний.
- Строки `"…"`/`'…'`, экранирование только `\n`, `\t`, остальное `\x`→`x`.
  Многострочные `"""\n … """`: последний перевод строки удаляется,
  экранирования нет.
- Литералы: `true/false/null` (с учётом регистра), `nan/inf/+inf/-inf`,
  число (i64 → u64 → double); всё прочее, например `123abc`, — строка.
  Hex `0x…` не поддерживается.

Нужен нам для: `m_modelInfo.m_keyValueText` моделей (`prop_data.base`
для стекла) и для отладочного вывода/тестов.

---

## 5. Сжатие

| Где | Алгоритм | Rust |
|---|---|---|
| KV3 буферы, KV3 v0 lz4 | сырой LZ4 block | `lz4_flex::block` |
| KV3 блобы v2+ | цепочка LZ4 block со словарём | `lz4_flex::block::decompress_into_with_dict` |
| KV3 | zstd frame | `ruzstd` |
| KV3 v0 `binary_bc`, SNAP | BlockCompress (ниже) | своё |
| VBIB/MVTX | zstd + meshoptimizer | не нужно (рендер) |

BlockCompress: `u32 hdr`; если `hdr & 0x80000000` — хранится как есть
(`size = hdr & 0x7FFFFFFF`); иначе `size = hdr`, в цикле: при исчерпании
битов читать `u16 mask`; бит 1 → `u16 tok`, `off = (tok>>4)+1`,
`len = (tok&0xF)+3`, побайтное копирование назад (перекрытие допустимо);
бит 0 → литерал `u8`. **(!)** В VRF нет проверки границ — у нас она обязательна.

---

## 6. Физика: `PhysAggregateData`

Код: `VRF/Resource/ResourceTypes/PhysAggregateData.cs`,
`RubikonPhysics/{Part,Shape,ShapeDescriptor}.cs`,
`RubikonPhysics/Shapes/{Hull,Mesh,Sphere,Capsule}.cs`,
`VRF/Serialization/KeyValues/KVObjectExtensions.cs`, `Model.cs`.

### 6.1 Где лежит коллизия карты

- `maps/<map>.vpk` → `maps/<map>/world_physics.vmdl_c` с блоком PHYS.
  Фолбэк: `maps/<map>/world_physics.vphys_c` (DATA = PhysAggregateData).
  VRF ещё умеет через `world_physics.vrman_c`.
  **(!)** В билде 2000908 `pak01` не содержит отдельных `*.vphys_c` — физика
  пропов встроена в `*.vmdl_c`; отдельные `vphys_c` встречаются только в
  `maps/cs_italy.vpk` (`world_physics`, `phys_level_water`) и
  `maps/lobby_mapveto.vpk`.
- `Model.GetEmbeddedPhys()`: блок CTRL (KV3) → `embedded_physics.phys_data_block`
  (индекс в таблице блоков) → этот блок PHYS.
- Модели пропов чаще используют отдельный файл: DATA модели →
  `m_refPhysicsData[0]` + `_c` (нижний регистр) → `vphys_c`.

### 6.2 Верхний уровень

| Ключ | Тип | Примечание |
|---|---|---|
| `m_nFlags` | int | |
| `m_bindPose` | array матриц | по одной на part; может быть пуст (см. 6.7) |
| `m_parts` | array Part | |
| `m_boneNames`, `m_boneParents` | | не нужны |
| `m_surfacePropertyHashes` | array int | MurmurHash2(seed `0x31415926`) имени surfaceprop |
| `m_collisionAttributes` | array | индекс — `m_nCollisionAttributeIndex` формы |

Part: `m_nFlags`, `m_flMass`, `m_rnShape`, `m_nCollisionAttributeIndex`, …
Shape (`m_rnShape`): массивы `m_spheres`, `m_capsules`, `m_hulls`, `m_meshes`.
Дескриптор формы: `m_nCollisionAttributeIndex` (**используем его, а не
part'а**), `m_nSurfacePropertyIndex`, `m_UserFriendlyName`, payload в
`m_Sphere` / `m_Capsule` / `m_Hull` / `m_Mesh`.

### 6.3 Hull (`RnHull_t`)

Скаляры: `m_vCentroid`, `m_flMaxAngularRadius`, `m_Bounds{m_vMinBounds,m_vMaxBounds}`,
`m_nFlags`, `m_flVolume`, … **(!)** Каждый массив ниже может быть
**KV3 binary blob** (упакованные структуры LE) **или KV3 array объектов** —
поддерживаем оба.

| Ключ | Элемент блоба | Форма объекта |
|---|---|---|
| `m_Vertices` (старые, до 2023-11-04) | vec3 f32, 12 байт | array vec3 |
| `m_Vertices` (новые) | **u8 исходящий полуребро-индекс вершины** (`RnVertex_t::m_nEdge`) | — |
| `m_VertexPositions` (новые) | vec3 f32, 12 байт | — |
| `m_Edges` | `{u8 next, u8 twin, u8 origin, u8 face}`, 4 байта | объекты с `m_nNext`, `m_nTwin`, `m_nOrigin`, `m_nFace` |
| `m_Faces` | `{u8 edge}`, 1 байт | `{m_nEdge}` |
| `m_Planes` | `{vec3 normal, f32 offset}`, 16 байт | объекты |

Правило: есть `m_VertexPositions` → это позиции, а `m_Vertices` — u8-индексы;
иначе позиции в `m_Vertices`. Рёбра идут парами (e, twin). Индексы u8 →
не больше 255 вершин/рёбер в hull. В новом формате `m_Vertices[v]` — индекс
**исходящего** из вершины `v` полуребра (`edges[m_Vertices[v]].origin == v`
выполняется на всех реальных hull'ах), а `HalfEdge.origin` индексирует
`m_VertexPositions` напрямую — проверено на 1933 hull'ах de_mirage:
0 треугольников с нормалью внутрь, 0 вершин вне плоскостей/bounds.

**Триангуляция** (веером по грани, `Hull.GetFaceTriangles`):
```
start = face.edge; e = edges[start].next
loop: if e == start break; n = edges[e].next; if n == start break
      emit(edges[start].origin, edges[e].origin, edges[n].origin); e = n
```
Обход CCW → нормали наружу. Флаги hull'а никто не фильтрует.

### 6.4 Mesh (`RnMesh_t`)

| Ключ | Элемент блоба | Форма объекта |
|---|---|---|
| `m_Vertices` | vec3 f32 | array vec3 |
| `m_Triangles` | `{i32 a, i32 b, i32 c}`, 12 байт | `{m_nIndex: [3]}` |
| `m_Materials` | u8 на треугольник (или array int; пусто — один материал) | |
| `m_Nodes` (встроенный BVH) | `{vec3 min, u32 packed, vec3 max, u32 triOffset}`, 32 байта; `packed>>30` тип (3 — лист), `packed & 0x3FFFFFFF` — смещение второго ребёнка / число треугольников | |

Также `m_vMin`, `m_vMax`, `m_nFlags`. Свой BVH строим сами, `m_Nodes` не нужен.

Sphere: `m_vCenter`, `m_flRadius`. Capsule: `m_vCenter` (2 × vec3), `m_flRadius`.

### 6.5 Атрибуты коллизий (`m_collisionAttributes[i]`)

- `m_CollisionGroupString` (null → `"Default"`);
- `m_InteractAsStrings` (**у старых ассетов — `m_PhysicsTagStrings`**);
- `m_InteractWithStrings`, `m_InteractExcludeStrings`;
- числовые `m_CollisionGroup`/`m_InteractAs`/… не использует никто.
- Отсутствующий ключ = пустой список.

**(!)** Имена групп не несут смысла (`ConditionallySolid` бывает и
playerclip, и grenadeclip) — решения принимаем по слоям interact-as/exclude.

### 6.6 Матрицы

- Плоская форма (`m_bindPose[i]`): 12 (или 16) float, row-major 3×4,
  строка i = `[R_i0 R_i1 R_i2 t_i]` → `p' = R·p + t`.
- Вложенная форма (`m_vTransform`): массив 3 (или 4) vec4-строк, та же математика.

### 6.7 Чего не делает референс (решаем явно)

- `m_bindPose` не применяется (для world_physics одна part — безвредно;
  для многочастных моделей пропов — нужно применять, как VRF);
- сферы и капсулы пропускаются;
- `point_template` и трансформы дочерних entity lump'ов не применяются;
- индекс атрибута — `u8` (падение после 255) — у нас `u16`.

---

## 7. Entity lump (`*.vents_c`)

Код: `VRF/Resource/ResourceTypes/EntityLump.cs`, `EntityLumpTraversal.cs`,
`EntityLumpKnownKeys.cs`, `VRF/Resource/Enums/EntityFieldType.cs`,
`VRF/Utils/StringToken.cs`, `VRF/ThirdParty/MurmurHash2.cs`,
`EntityTransformHelper.cs`.

- `vwrld_c` DATA: `m_entityLumps` (пути), `m_worldNodes[].m_worldNodePrefix`.
  Референс проще: перебирает все `vents_c` в VPK карты.
- `vents_c` DATA: `m_name`, `m_childLumps`, `m_entityKeyValues[]`.
  Элемент: `m_connections[]` (`m_outputName`, `m_targetName`, `m_inputName`,
  `m_overrideParam`, `m_flDelay`, `m_nTimesToFire`, `m_targetType`) и одно из:
  - **новое**: `keyValues3Data { version == 1, values{}, attributes{} }` —
    объединить оба, ключи в нижний регистр;
  - **legacy**: блоб `m_keyValuesData`:
    ```
    u32 version == 1, u32 hashedCount, u32 stringCount
    hashedCount × { u32 keyHash; value }
    stringCount × { u32 keyHash; cstr keyName; value }
    value: u32 type, затем:
      0x06 bool u8 | 0x01 f32 | 0x22 f64 | 0x09 color 4×u8 | 0x05 i32 | 0x25 u32
      0x1a i64 | 0x21 u64 | 0x03 vector / 0x27 qangle 3×f32 | 0x1e cstr | иначе ошибка
    ```
- Хэш ключей: MurmurHash2 (`m = 0x5bd1e995`, `r = 24`, seed `0x31415926`)
  по ASCII-lowercase ключу; имена известных хэшей — таблица
  `EntityLumpKnownKeys.cs`.
- Энтити без `classname` отбрасывается. `origin`/`angles` — массив или
  строка `"x y z"`; `scales` по умолчанию (1,1,1). Targetname может иметь
  префикс `[PR#]`.
- Трансформ энтити: `v' = R(angles)·(v * scales) + origin`, где
  `R = Rz(yaw)·Ry(pitch)·Rx(roll)` (column-vector; сначала roll, потом
  pitch, потом yaw; положительный pitch смотрит вниз, `forward.z = −sin(pitch)`).
- `point_template`: ключ `entitylumpname` указывает дочерний lump, его
  энтити наследуют `rigid(template) * parent`.

---

## 8. World nodes и статические пропы (`*.vwnod_c`)

Код: `VRF/Resource/ResourceTypes/World.cs`, `WorldNode.cs`,
`VRF-R/World/WorldNodeLoader.cs`, `SS/src/Extraction/MapExtractor.cs`
(`AppendStaticProps`).

- `world.vwrld_c` → `m_worldNodes[].m_worldNodePrefix` + `.vwnod_c`
  (**(!)** в префиксах обратные слэши — заменить на `/`, нижний регистр).
- `vwnod_c` DATA: `m_sceneObjects[]`, `m_aggregateSceneObjects[]`,
  `m_clutterSceneObjects[]`, `m_sceneObjectLayerIndices`, `m_layerNames`.
- SceneObject: `m_renderableModel` (путь vmdl), `m_vTransform` (3 vec4-строки —
  полная матрица размещения, со scale), `m_nObjectTypeFlags`, `m_skin`, …
- Физика пропа: embedded PHYS модели, иначе `m_refPhysicsData[0]` → `vphys_c`;
  кэшировать по пути модели (включая «нет физики»).
- `AggregateSceneObjects` — объединённые визуальные меши без связи с
  исходной моделью; коллизию из них не восстановить (у референса пропущены
  сознательно; на dust2/mirage/anubis все реальные пропы — агрегаты,
  предполагается, что их коллизия уже запечена в world_physics). На
  de_mirage worldnode n0: 335 scene objects (объединённые render-батчи
  worldnode'а, без физики) + 260 агрегатов. На de_cache физика есть у 762
  из 1330 scene objects — там важны именно статические пропы. Сферы и
  капсулы в физике на некоторых картах не редкость (de_fachwerk — 2992
  капсулы, de_boulder — 2662, de_overpass — 524, cs_shelter — 175 сфер /
  123 капсулы) и пока не триангулируются (как у референса).
- Поиск моделей: VPK карты → `csgo/pak01_dir.vpk` →
  `csgo_community_addons/<map>/<map>_dir.vpk` (cs_shelter, de_boulder, de_fachwerk).

---

## 9. Навигационная сетка CS2 (`maps/<map>.nav`)

Код: `VRF/NavMesh/NavMeshFile.cs`, `NavMeshArea.cs`, `NavMeshLadder.cs`,
`NavMeshGenerationParams.cs`, `NavMeshGenerationHullParams.cs`,
`NavMeshTransformedBounds.cs`, `NavAttributeFlags.cs`.

```
u32 magic = 0xFEEDFACE
u32 version            (VRF: 30..36)
u32 subVersion
u32 flags              (бит 0 — analyzed)
v>=36: KV3 блок (unknown1)
v>=31: таблица полигонов
v>=32: u32 (== 0)
v>=35: movable meshes
v>=36: KV3 блок (unknown2)
areas
ladders
transformed bounds
generation params
v>=36: KV3 блок (unknown3)
subVersion>0: KV3 custom data
```

- Встроенный KV3: выровнять позицию потока до кратного 8, затем полный
  binary KV3.
- Полигоны (v31+): `u32 cornerCount`, `cornerCount × vec3`; `u32 polygonCount`,
  полигон = `u8 n, n × u32 индекс угла`, v35+: `u32 movableMeshId`
  (`0xFFFFFFFF` — статичный мир).
- Movable meshes (v35+): `u32 count`, каждый — cstr id + 48 байт (вероятно 3×4 матрица).
- Area:
  ```
  u32 id; i64 attributeFlags; u8 hullIndex
  v>=31: u32 polygonIndex   иначе: u32 cornerCount, cornerCount × vec3
  f32 unknown
  для каждого угла (ребра): u32 n, n × {u32 areaId, u32 edgeId}
  u8 hidingSpots (== 0); u32 encounters (== 0)
  u32 laddersAbove, × u32; u32 laddersBelow, × u32
  ```
  Флаги: Jump 0x2, NoJump 0x8, Stop 0x10, Run 0x20, Walk 0x40, Avoid 0x80,
  Transient 0x100, DontHide 0x200, Stand 0x400, NoHostages 0x800, Stairs 0x1000,
  NoMerge 0x2000, ObstacleTop 0x4000, NonZUp 0x8000, CrouchHeight 0x10000,
  NonZUpTransition 0x20000, CrawlHeight 0x40000.
  Хулл 0 — стоящий игрок (его и берёт референс).
- Ladder: `u32 id, f32 width, vec3 top, vec3 bottom, f32 length, u32 dir,
  u32 × 5 (topForward, topLeft, topRight, topBehind, bottom)`, v35+: ещё `u32 × 2`.
- Transformed bounds: `i32 count`, каждый `vec3 min, vec3 max, 12 × f32` (3×4).
- Generation params: `i32 navGenVersion, u32 useProjectDefaults, f32 tileSize,
  f32 cellSize, f32 cellHeight, i32 minRegionSize, i32 mergedRegionSize,
  f32 meshSampleDistance, f32 maxSampleError, i32 maxEdgeLength, f32 maxEdgeError,
  i32 vertsPerPoly`; gen>=7 `f32 smallAreaOnEdgeRemoval`; gen>=12
  `cstr hullPresetName, cstr hullDefinitionsFile`; `i32 hullCount` × hull
  params; gen<=11 — добивка до 3 записей; gen>=12 `u8 gravityFollowsRotation`.
- Hull params: gen>=9 `u8 enabled`; `f32 radius, f32 height`; gen>=9
  `u8 shortHeightEnabled, f32 shortHeight`; gen>=13 `u8 crawlEnabled, f32 crawlHeight`;
  `f32 maxClimb, i32 maxSlope, f32 maxJumpDownDist, f32 maxJumpHorizDistBase,
  f32 maxJumpUpDist`; gen>=11 `i32 borderErosion`.

**(!)** Высота nav-области — среднее углов полигона, до ~5u ниже реального
пола; для `setpos` ноги ставим по коллизии.

Реальные параметры генерации de_mirage (билд 2000908): `nav_gen_version 13`,
один hull: `radius 16, height 71, max_climb 16, max_slope 50,
max_jump_down_dist 157, max_jump_horiz_dist_base 64, max_jump_up_dist 68`.
Это параметры генерации навигационной сетки, а не константы движения
игрока — не путать. В билде 2000908 в игре 27 файлов `.nav`, версии 35–36,
все парсятся.

---

## 10. Политика твёрдости для гранат и игрока (референс, `CollisionMesh.cs`)

```
grenade_solid[i] = !exclude[i].contains_ci("csgo_thrown_grenade")
                && !interactAs[i].any_ci({"playerclip","npcclip","sky"})

player_solid[i]  = !(interactAs.any_ci("npcclip") && !interactAs.any_ci("playerclip"))
                && !exclude[i].contains_ci("player")
                && !interactAs[i].any_ci("csgo_grenadeclip")
                && name[i] != "EntityPhysicsClip"
```

- Для гранаты твёрдые в том числе `csgo_grenadeclip` и `passbullets`
  (если exclude не содержит `csgo_thrown_grenade`).
- Синтетические атрибуты из энтити (пустые слои → твёрдые для гранаты):
  `func_clip_vphysics` → `EntityPhysicsClip` (не твёрд для игрока/видимости),
  `func_door*`, `prop_door_rotating` → `EntityDoor`, `func_breakable` →
  `EntityBreakable`, `prop_dynamic`-стекло → `EntityBreakable`,
  прочее из allowlist и статические пропы → `EntitySolid`.
- Allowlist классов: `func_brush`, `func_clip_vphysics`, `func_door`,
  `func_door_rotating`, `func_breakable`, `prop_door_rotating`, `prop_dynamic`
  (последний — только если разрушаемый: `break_list` без `break_command_list`).
- Стекло: `prop_data.base` начинается с `Glass` (например `Glass.Window`);
  без base — путь модели содержит `window`/`glass`. `m_modelInfo.m_keyValueText`
  парсится только если начинается с `<!-- kv3 ` и длина ≥ 140 (`Model.cs:581-585`;
  референс наследует это через VRF, повторяем ради совпадения классификации стекла).
- Пропускать: `func_brush` с `retake` в targetname или `/retake_` в пути
  модели; энтити с `startdisabled` = `1`/`true`.
- Для сравнения: трассировщик рендерера VRF (`VRF-R/Rubikon.cs`) использует
  другую, включающую модель тегов — **не** наша модель; правила выше
  откалиброваны референсом по реальным броскам.

---

## 11. Промежуточный формат референса `.s2geo` (для справки)

.NET `BinaryWriter` (строки с 7-битной длиной): magic `S2SSGEO3`, mapName,
gameBuildId, атрибуты (имена, interact-as, exclude), `f32` вершины, `i32`
индексы, `u8` атрибут на треугольник. Наш `.cgeo` — свой версионированный
формат (см. `ARCHITECTURE.md`), совместимость не требуется.
