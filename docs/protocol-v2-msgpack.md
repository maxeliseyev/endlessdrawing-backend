# WebSocket v2: MessagePack

M2 реализован отдельным [manifest v3](protocol-v3-manifest.md), с откатом на этот v2. Логические операции v1/v2 сохранены.

Статус: M1 реализован локально 18.11.2026: backend и frontend поддерживают JSON v1 и MessagePack v2. Production-выкладка и приёмка HTTPS/iPhone/iPad этим не выполнены. Этот файл — единственный нормативный источник бинарного профиля. Семантика операций v1 — [protocol.md](protocol.md); HTTP Admin API — только [admin-api.md](admin-api.md).

Архитектура и следующие этапы — [frontend transport-v2-design.md](../../endless-drawing-frontend/docs/transport-v2-design.md). M2 (manifest/missing geometry) и M3 (приоритет видимого) остаются проектом и не входят в реализованный M1.

## 1. Задача и измеренная исходная точка

Сократить число байтов в завершённых штрихах без изменения геометрии, ownership, undo, порядка наложения и правил синхронизации. MessagePack сам по себе не устраняет полные snapshot при повторной подписке. Эта проблема выделена в следующий этап проекта, а не считается решённой бинарным форматом.

Диагностическое чтение настоящего `wss://endlessdrawing.com/ws` 18.11.2026: центр `(4800, -2100)`, окно 1512×850 CSS px, zoom 1, подписка 4536×2550 world units. Получено 94 штриха / 21 430 точек / 4 JSON chunks. Измерение не воспроизводит неизвестные камеру, zoom, сеть и длительность сессии со скриншота владельца.

| Представление тех же сообщений | Байты application payload |
| --- | ---: |
| JSON v1, как получено | 858 499 |
| MessagePack maps, координаты float64 | 419 193 |
| JSON + DEFLATE | 386 259 |
| MessagePack + DEFLATE | 323 460 |

MessagePack вычислен диагностическим энкодером по спецификации, с именованными полями и строковыми UUID. DEFLATE посчитан офлайн: level 6, raw DEFLATE, независимый контекст на сообщение, `Z_SYNC_FLUSH` без завершающих четырёх байтов. Это ориентир для размера, не проверка производительности production-библиотек и не измерение TLS/TCP/WebSocket overhead. Даже при предложенном `permessage-deflate` сервер не вернул согласованное расширение.

Первый chunk пришёл через 788 мс после subscribe, end — через 1027 мс; handshake отдельно занял 1147 мс. Эти числа не отделяют время БД, сериализации и сети и не измеряют Safari decode/paint. Повтор той же подписки и сдвиг на 55% ширины экрана каждый снова передали 94 тех же штриха и 858 499 байтов. 57,2 МБ на исходном скриншоте нельзя интерпретировать как размер одного snapshot без границ begin/end.

## 2. Выбор профиля до первого сообщения

Один `/ws`, один WebSocket на вкладку. Секрет не участвует в выборе профиля и не попадает в URL или HTTP headers.

Новый клиент предлагает `Sec-WebSocket-Protocol` в порядке:

```text
endless-drawing.msgpack.v2, endless-drawing.json.v1
```

Сервер выбирает ровно один предложенный профиль и возвращает его в ответе upgrade. Поддержка старых клиентов без заголовка сохраняется:

| Запрос | Выбор сервера | Данные после upgrade | `hello.ack.protocol_version` |
| --- | --- | --- | ---: |
| Предложен v2, v2 включён | `endless-drawing.msgpack.v2` | MessagePack Binary | 2 |
| Предложены v2 и JSON v1, v2 выключен | `endless-drawing.json.v1` | JSON Text | 1 |
| Нет `Sec-WebSocket-Protocol` | Без выбранного профиля | Legacy JSON Text | 1 |
| Предложены только неподдерживаемые профили | HTTP 400 до upgrade | Соединение не открывается | — |

Origin allowlist, лимит соединений и shutdown gate проверяются для всех профилей. Наличие неизвестных вариантов рядом с поддерживаемым не мешает выбору поддерживаемого.

Новый клиент проверяет `socket.protocol` перед hello. Пустое или неожиданное значение при предложенных профилях — ошибка соединения; не угадывать кодек по первому байту и не открывать параллельный сокет. Автоматического повторного соединения без subprotocol для обхода ошибки нет. Совместимость со старым сервером обеспечивается порядком выкладки, а не неоднозначным handshake. Обычный reconnect повторяет тот же список предложений.

## 3. Границы сообщений

В профиле v2 каждое application-сообщение, включая hello, ack и error, — ровно один MessagePack map в одном WebSocket Binary message. Фрагментацию WebSocket собирает его библиотека; один message не обязательно равен одному сетевому frame.

Не добавлять Base64, JSON-оболочку, length prefix, массив сообщений или конкатенацию MessagePack objects. Пустое сообщение, второй объект/хвост после первого, Text message в v2 и Binary message в JSON-профиле недопустимы. Ping/Pong/Close остаются стандартными управляющими frames WebSocket.

В браузере `binaryType = 'arraybuffer'` устанавливается сразу после создания сокета. Decode синхронный для одного ограниченного сообщения, затем runtime guards и существующая state machine. Асинхронное преобразование отдельных Blob через независимые promises не использовать: оно усложняет порядок ack/snapshot/events и обработку старого сокета.

## 4. Профиль MessagePack и точность

Используются maps со строковыми именами полей, arrays, UTF-8 strings, booleans, integers и float64. Порядок ключей map не значим. Позиционные tuples для целого stroke/message, числовые opcodes, `bin`, extension/timestamp types и пользовательские typed-array extensions в v2 не используются. Единственный позиционный массив геометрии — существующая пара `[x, y]` внутри `points`.

| Данные | Представление / проверка |
| --- | --- |
| Координаты, width, opacity, spawn, bounds | Конечные JS number / Rust f64. Дробные значения кодировать float64; целые допускают точное integer-представление. Не включать float32/округление/квантизацию |
| `seq` | Integer `1..9_007_199_254_740_991`; проверять диапазон и на серверном выходе, и после decode |
| `protocol_version` | Integer, 2 в бинарном профиле |
| UUID stroke | Строка 36 символов в стандартной форме; нормализация lowercase, как в v1 |
| `request_id`, `subscription_id` | Непустые строки до 128 UTF-8 bytes; приложение продолжает генерировать UUID |
| `color`, `brush`, `type`, `code` | Те же строки и enum, что в v1; сервер нормализует цвет в uppercase |
| `own`, `already_deleted` | Только boolean; отсутствуют в create draft |
| Необязательное поле | Отсутствует, когда значения нет; не заменять на nil/undefined |

Float64 сохраняет точность существующих векторов, включая большие мировые координаты. `-0` и `+0` считаются одной координатой. NaN, ±Infinity, значения вне world limit, нецелый/небезопасный seq отвергаются. Декодированный `bigint` не передаётся в модель `Stroke`; unsafe integers отбрасываются до её обновления.

Rust-кодек явно использует `with_struct_map().with_human_readable()`: UUID выдаются строками, что проверяется typed server fixture/test. Вход сначала ограниченно декодируется в semantic map, затем проходит domain guards. DB/domain UUID остаются своим типом. Не требовать побайтовой каноничности MessagePack: порядок map и ширина integer-кодирования могут отличаться. Идемпотентность определяется UUID, владельцем и нормализованным содержимым штриха, как в v1; хеш новых transport bytes вместо существующего механизма недопустим.

## 5. Логические сообщения

Все формы ниже — структурная запись декодированного объекта, не текст внутри Binary message. За исключением версии hello.ack, поля и обязательность соответствуют v1. Конкретный каталог ошибок и правила нормализации берутся из backend `protocol.md`/`src/protocol.rs`; сырые `message` не показываются в UI.

| Направление | `type` | Поля кроме `type` |
| --- | --- | --- |
| C → S | `hello` | `session_secret` |
| S → C | `hello.ack` | `protocol_version: 2`, необязательный `spawn: {x, y}` |
| C → S | `viewport.subscribe` | `subscription_id`, `bounds: {min_x, min_y, max_x, max_y}` |
| S → C | `viewport.snapshot.begin` | `subscription_id` |
| S → C | `viewport.snapshot.chunk` | `subscription_id`, `strokes: Stroke[]` |
| S → C | `viewport.snapshot.end` | `subscription_id` |
| C → S | `stroke.create` | `request_id`, `stroke: StrokeDraft` |
| C → S | `stroke.delete` | `request_id`, `stroke_id` |
| S → C | `ack`, operation `stroke.create` | `request_id`, `operation`, `stroke: Stroke` |
| S → C | `ack`, operation `stroke.delete` | `request_id`, `operation`, `stroke_id`, `seq`, `already_deleted` |
| S → C | `stroke.created` | `subscription_id`, `stroke: Stroke` |
| S → C | `stroke.deleted` | `subscription_id`, `stroke_id`, `seq` |
| S → C | `error` | `code`, `message`; необязательные `request_id` / `subscription_id` по корреляции v1 |

`StrokeDraft` = `{id, points, brush, width, color, opacity}`. `Stroke` добавляет только серверные `{seq, own}`. Bounds вычисляет сервер; не добавлять их в клиентский create или изобретать новое поле bounds в публичном stroke. `round`, `square`, `chisel`, `spray` и алгоритмы кистей остаются прежними.

Секрет — существующая строка base64url от 32 bytes Web Crypto, только Storage/RAM страницы и hello по WSS (localhost WS допустим). Новое binary-представление не изменяет право удаления и не даёт администраторских полномочий. Read-only admin использует тот же публичный WS-профиль, с собственным RAM-секретом и без create/delete. Логин, WebAuthn, TOTP, CSRF и hide продолжают использовать существующий HTTP Admin API.

## 6. Лимиты и ошибки декодирования

Лимиты задаются по размеру готового application payload в байтах, до возможного transport compression. Количество символов JSON не является byte limit. Таблица — профиль совместимости v2; конфигурация не должна незаметно расширить его для одного клиента.

| Ограничение | Значение v2 |
| --- | ---: |
| Client → server, одно сообщение | 128 KiB |
| Server → client, одно сообщение | 256 KiB |
| Целевой размер snapshot chunk | 64 KiB |
| Штрихов в snapshot chunk | ≤1000 |
| Точек одного штриха | 1..2000 |
| Точек суммарно в snapshot chunk | ≤8192 |
| Вложенность MessagePack containers | ≤8 |
| Элементов array при структурном decode | ≤2000, затем field-specific guards |
| Пар в одном map | ≤16 |
| Одной строки при структурном decode | ≤256 UTF-8 bytes, затем field-specific guards |
| Узлов декодируемого дерева | ≤65 536 |
| Клиентская scene | 20 000 strokes / 1 000 000 points, pending ≤64 |
| World coordinates / subscription span | ±1e9 / ≤30 000 по оси, как v1 |

64 KiB — цель packing, а не таймер ожидания заполнения. Первую готовую страницу/порцию отправлять сразу. Единственный допустимый stroke, превышающий целевой размер, можно отправить отдельно в пределах hard limit. Chunk заканчивается по первому из byte/count/point limits. DB page и WS chunk — разные понятия; длинную DB page делить, маленькую не задерживать ради накопления. Границы chunks могут различаться между JSON и MessagePack.

До выделения массивов/строк проверить длины и доступные байты, затем глубину/общий бюджет. После decode применить существующие guards, в том числе размеры `points`/пар/строк и конечность чисел. Только post-decode `parseStroke` недостаточен против объявленного `array32` огромной длины, глубокой вложенности и unsafe integers. Дубли ключей map, нестроковые ключи, `__proto__`/`prototype`/`constructor`, неизвестные client-поля, extensions/bin и недопустимые типы отклонять. Некорректный объект не должен частично менять scene/outbox.

Oversize message: Close 1009. Неверный Text/Binary тип выбранного профиля: Close 1003. Malformed MessagePack, лишние bytes и нарушение структурного бюджета: Close 1007. Нельзя извлекать request_id повторным неограниченным decode. После успешного безопасного decode обычная domain-ошибка возвращается существующим `error` в выбранном профиле. Браузер не разрешает приложению отправлять Close 1003/1007/1009: публичный клиент использует соответствующие application-коды 4003/4007/4009, существующее локализованное сообщение и reconnect; сервер отправляет стандартные коды выше; admin — существующий protocol-error flow. Секреты, тела сообщений и сырые ошибки декодера не логировать.

Rate limits 5 creates/s, 20 deletes/s, 5 subscribes/s, клиентские интервалы с запасом, ack timeout 12 с и существующие handshake/snapshot deadlines сохраняются. Замена формата не является поводом увеличить число точек или размеры сцены.

## 7. Snapshot, события и рендер

Сохраняются hello → ack → subscribe → begin/chunks/end, новый subscription_id, игнорирование старой подписки, staging, tombstones и согласованная граница БД/live events. Chunks рисуются по мере прихода; удаление отсутствующих объектов выполняется только на end. Буферизированные изменения сервера следуют за end. После reconnect — новый hello и полный snapshot, не replay по seq.

Ack и broadcast дедуплицируются по id. Отмена pending до/после отправки и повтор create с тем же UUID/содержимым не меняются. MessagePack не сериализует preview и не добавляет streaming незавершённых штрихов.

Decoder возвращает обычные объекты для `src/wire.ts`. Типы модели, WeakMap/renderer caches, float64 world coordinates и тайловая отрисовка не зависят от транспортного кодека. Переделка renderer вместе с кодеком не требуется. Более быстрый decode не обещается до browser benchmark.

## 8. Сжатие

В v2 нет application gzip/deflate/zstd, magic bytes или альтернативного сжатого тела. Сравнение размеров не является решением включить сжатие. Отдельно проверить поддержку `permessage-deflate` в используемом WS-стеке, CPU/память, задержки, размер распакованных сообщений и поведение секрет-bearing hello. Сжатие согласуется WebSocket extension, не HTTP gzip для статических assets и не флагом внутри hello. При его будущем включении hard limits действуют также на распакованные bytes.

## 9. Реализация и rollout gate

Кодеки: backend `src/codec.rs`, frontend `src/transport/{codec,msgpack}.ts`. Зависимости: `rmp-serde 1.3.1`, `@msgpack/msgpack 3.1.3`, зафиксированные lockfiles. Бинарный preflight с лимитами выполняется до библиотечного decode на обеих сторонах. Строковые UUID и точность проверены в общем корпусе `tests/fixtures/ws-v2/` и real-Postgres WS tests. Fixture-копия frontend хранит тот же SHA-256 manifest.

`WS_MSGPACK_ENABLED=true` по умолчанию разрешает выбор v2, `false` оставляет explicit/legacy JSON. `deploy/compose.yml` передаёт флаг в приложение. Для применения на VPS понадобится обновить серверный Compose обычным deployment-процессом; эта задача его не обновляет. Уже открытый сокет сохраняет выбранный профиль.

## 10. Выкладка и откат

1. Реализовать и проверить backend, понимающий legacy v1 без subprotocol, явный JSON v1 и MessagePack v2. Сначала старый frontend продолжает работать без изменений.
2. Доказать паритет v1/v2 общими fixtures и real-backend тестами, включая одновременных JSON/MessagePack посетителей.
3. Выпустить frontend с двумя предложенными профилями для public и read-only admin, общим codec boundary и проверкой версии ack.
4. Замерить трафик/время первого видимого штриха/полного snapshot и ошибки на staging HTTPS в трёх браузерах; отдельно на iPhone/iPad Safari.
5. При проблеме с v2 сервер выбирает предложенный JSON v1 на новых соединениях; уже открытые v2 соединения либо завершаются корректно, либо продолжают прежний профиль до reconnect. Кодек внутри открытого сокета не менять.

Откат backend на сборку до поддержки subprotocol возможен только после отката frontend и учёта открытых/кешированных новых вкладок. Поэтому обычный rollback транспорта — выключение выбора v2 с сохранением dual-profile backend. Удаление legacy v1 — отдельная миграция после проверки активных клиентов, включая будущий нативный клиент.

## 11. Критерии готовности

- Обе стороны читают golden binary fixtures другой стороны; отдельно проверены строковые UUID и safe integer seq. Побайтовое совпадение разных энкодеров не требуется.
- Геометрия всех кистей, дробные значения возле ±1e9, отрицательные координаты, 1/2000 точек, opacity и uppercase normalization проходят semantic round-trip без float32.
- Некорректные/усечённые данные, хвост второго объекта, unknown fields, duplicate keys, запрещённые типы, unsafe integers, огромные length headers и глубина отвергаются ограниченно, без изменения scene и без payload в логах.
- Legacy JSON, explicit JSON и binary профили проходят create/delete/undo/reconnect/resync, multipart snapshot и ownership; смешанные клиенты видят один мир. Admin остаётся read-only, HTTP-факторы/hide не меняются.
- Chromium/Firefox/WebKit подтверждают Binary frames и порядок сообщений; mock и real-backend suites декодируют фактический frame type, а не ищут JSON substring внутри binary.
- На фиксированном geometry-heavy корпусе ≤60% JSON application bytes при той же геометрии и сопоставимых лимитах; это цель, а не гарантия для коротких control messages. Decoder/paint CPU и память не должны ухудшать UX; бюджет зафиксировать по baseline перед включением по умолчанию.
- Измерения отделяют handshake, subscribe→first chunk, decode/validate, подготовку геометрии, first visible paint, snapshot.end и total bytes. Пустые области и cache overlap измеряются отдельно.

## Источники

- [MessagePack specification](https://github.com/msgpack/msgpack/blob/master/spec.md) — типы и бинарное представление.
- [@msgpack/msgpack](https://github.com/msgpack/msgpack-javascript) — кандидат TS-кодека, single-object decode и настройки лимитов.
- [rmp-serde: to_vec_named](https://docs.rs/rmp-serde/latest/rmp_serde/encode/fn.to_vec_named.html) — кандидат сериализации именованных maps на сервере.
- [WHATWG WebSockets](https://websockets.spec.whatwg.org/) — согласование subprotocol и Binary messages.
- [RFC 7692](https://www.rfc-editor.org/rfc/rfc7692.html) — отдельное согласование WebSocket compression.
