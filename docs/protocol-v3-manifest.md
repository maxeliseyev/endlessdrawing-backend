# WebSocket v3: manifest и недостающая геометрия

Статус: реализация M2 от 18.11.2026; production ещё не обновлялся. Этот документ — нормативное расширение [MessagePack v2](protocol-v2-msgpack.md). Формат чисел, byte/structural limits, UUID, ownership, create/delete/ack, HTTP Admin API и геометрия кистей прежние. Этап M3 с приоритетом видимой области не входит в этот контракт.

## Назначение и согласование

Повторная подписка передаёт авторитетное членство области без повторных points. Клиент предлагает `endless-drawing.msgpack.v3`, затем `endless-drawing.msgpack.v2`, затем `endless-drawing.json.v1`. Сервер выбирает v3 только при `WS_MSGPACK_ENABLED=true` и `WS_MANIFEST_ENABLED=true`. В выбранном v3 hello.ack содержит `protocol_version: 3`, все application messages — Binary MessagePack maps. Нет угадывания версии по payload.

V2/JSON v1 работают по прежнему контракту. На них cached subscribe и fetch отвергаются. При `WS_MANIFEST_ENABLED=false` новый клиент выбирает v2, при `WS_MSGPACK_ENABLED=false` — предложенный JSON v1. Открытый сокет не меняет профиль.

Первый subscribe нового соединения, reload/reconnect и восстановление после resync — обычный `viewport.subscribe` с полным snapshot. Это сохраняет cold-cache latency M1: дополнительного manifest→fetch RTT нет. Cached subscribe применяется только внутри того же соединения после завершённого snapshot; геометрия не сохраняется в Storage.

## Дополнительные сообщения

Ниже — логическая запись maps после decode, не текст внутри Binary message. Поля обязательны, лишние клиентские поля отвергаются.

| Направление | type | Поля кроме type |
| --- | --- | --- |
| C→S | `viewport.subscribe.cached` | `subscription_id`, `bounds` по прежним правилам |
| S→C | `viewport.manifest.begin` | `subscription_id`, `snapshot_token`, `stroke_count`, `point_count` |
| S→C | `viewport.manifest.chunk` | `subscription_id`, `snapshot_token`, `stroke_ids: UUID[]` |
| S→C | `viewport.manifest.end` | `subscription_id`, `snapshot_token` |
| C→S | `viewport.fetch` | `subscription_id`, `snapshot_token`, `batch`, `stroke_ids: UUID[]`, `final: boolean` |
| S→C | `viewport.fetch.end` | `subscription_id`, `snapshot_token`, `batch` |
| S→C | `viewport.snapshot.fallback` | `subscription_id`, `reason: busy | scene_limit` |

`snapshot_token` — сгенерированная сервером строка UUID, привязанная к текущему connection/subscription. Он не является секретом или mutation authority. Batch — integer, начинается с 0, увеличивается на 1. UUID stroke остаются строками; arrays по ≤1000 UUID. Manifest counts — integers: 0..20 000 strokes и 0..1 000 000 points; каждый stroke содержит 1..2000 points. `point_count` — сумма исходных точек всех членов manifest.

## Последовательность cached snapshot

1. Сервер отменяет прежний snapshot, проверяет bounds/rate limit и получает согласованную DB-границу вместе с event receiver через существующий writer barrier.
2. В read transaction читает только `id`, `seq`, `jsonb_array_length(points)` живых штрихов, пересекающих bounds, по возрастанию seq. Membership строится целиком в пределах лимитов; при переполнении не выдаётся частичный manifest.
3. Сервер отправляет `viewport.snapshot.begin` перед чтением metadata; транзакция закрывается **до ожидания клиента** и перед manifest begin/chunks/end. Token уникален для этой загрузки, stroke_ids идут по seq. Пустой manifest имеет count=0, begin→manifest.begin→manifest.end.
4. Клиент держит старую сцену видимой, собирает bounded membership отдельно. После полного manifest переиспользует имеющиеся immutable Stroke objects напрямую в staging, фиксируя их сильными ссылками. Совпадение определяется по UUID в фактическом кеше этой сессии, не по предположению сервера «мы это отправляли».
5. Отсутствующие UUID клиент запрашивает batches по ≤1000. Один batch в полёте; его geometry приходит обычными `viewport.snapshot.chunk` с текущим subscription_id. Сервер не отправляет geometry для переиспользованных UUID. Каждый ответ должен соответствовать ещё не полученным UUID текущего batch; лишние/повторные записи недопустимы.
6. Для non-final batch сервер после всех его chunks отправляет `viewport.fetch.end`, клиент проверяет batch/token/полноту и отправляет следующий. Для final batch сервер сразу завершает `viewport.snapshot.end`. При полностью тёплой области клиент отправляет batch=0, stroke_ids=[], final=true: geometry bytes равны нулю.
7. Клиент проверяет полноту geometry и суммарное point_count, затем end атомарно заменяет сцену. Отсутствующие в manifest прежние штрихи удаляются только здесь. Own acks после DB-границы сохраняются отдельно в staging по прежней семантике; pending/undo не теряются.
8. Post-snapshot create/delete/hide events следуют после end. Рисование всегда по seq независимо от поступления cached/fetched записей. Тёплая область всё ещё передаёт manifest/control bytes, а не «ноль трафика».

## Геометрия после закрытия транзакции

В штатной модели stroke UUID/seq/points/brush/width/color/opacity/owner неизменяемы; delete/hide меняет только deleted_at. Поэтому geometry запрос может прочитать члена frozen manifest по PK, включая мягко удалённую запись. Сервер проверяет membership и исходный seq перед выдачей; own вычисляет по сессионному owner, никогда не принимает его от клиента.

Удаление после DB-границы не меняет принадлежность самому снимку; соответствующий buffered event после end снимает stroke. Если клиент уже знает delete/hide (например, собственный delete ack или admin HTTP hide), локальный tombstone запрещает повторный показ даже при поздней snapshot geometry.

Физическое удаление/несовпадение seq/повреждение строки при чтении geometry не подменяется пустым stroke: загрузка отменяется с resync_required, новый полный snapshot строится по новой границе. Новые серверные операции, меняющие сохранённую geometry, должны пересмотреть этот контракт и инвалидировать snapshots, а не молча редактировать запись.

## Ресурсы, отмена и fallback

- До 16 cached snapshots на процесс, общий semaphore: метаданные занимают ограниченную память. При нехватке слота `snapshot.fallback: busy` до чтения DB.
- В памяти snapshot до 20k UUID/seq и bounded множества уже запрошенных UUID, без полного массива world points. Поиск metadata идёт pages текущего SNAPSHOT_PAGE_SIZE (≤1000), geometry читается pages по 100 записей.
- Вне SQL чтения geometry пул не занят этим snapshot. Пока клиент думает или данные отправляются, read transaction не удерживается.
- Две исходящие порции и две входящие fetch-команды максимум; try_send не блокирует WebSocket writer/heartbeat/mutations. Клиентский stop-and-wait не делает очередь бесконечной.
- Общий snapshot timeout, send timeout, event broadcast ring и frame rate limit прежние; fetch входит в общий лимит сообщений, но не считается новым subscribe. Проверяются batch order, token, UUID membership, отсутствие повторов, ≤1000 ids/batch и ≤20 batch index.
- Snapshot chunks сохраняют target 64 KiB, hard cap 256 KiB, ≤8192 points и ≤1000 strokes. Manifest chunk из 1000 строковых UUID с envelope меньше target 64 KiB.
- Новая подписка/close/shutdown отменяет task и освобождает слот. Старый fetch с другим subscription_id игнорируется и не отменяет заменившую его загрузку. Неверный token/batch/UUID активной подписки приводит к resync_required, а не чтению произвольной geometry.
- Lagged event receiver после snapshot требует полного resync, не replay по seq. Контракт не обещает пережить переполнение event buffer без повторного snapshot.
- `scene_limit` выдаётся вместо неполного manifest. Публичный клиент показывает существующее краткое dense-сообщение и загружает полный snapshot с прежним ограниченным отображением. Admin завершает загрузку существующим scene_limit flow. `busy` вызывает полный snapshot без нового UI.
- Resync/fallback сбрасывает право следующей cached подписки; клиент начинает full с новым subscription_id с throttle. После обычного полного snapshot публичный truncated cache снова не объявляется полным.

## Приёмка и публикация

Проверки: полностью тёплая область → 0 повторных geometry bytes; частичное перекрытие → только отсутствующие UUID; удаление/HTTP hide в перекрытии; delete во время fetch; own ack после границы; отмена/старые сообщения на каждой фазе; invalid token, id и batch; cache eviction; превышение points/strokes; busy slot; slow client/освобождение DB pool; event buffer overflow; JSON/v2/v3 mixed clients; cold entry/reconnect остаются full.

Release план: завершить локальную приёмку M1+M2, подготовить соответствующую пару frontend/backend и Compose gate, затем HTTPS staging с сохранением JSON/v2 отката. Production-публикация — после staging-проверки, отдельным действием; этот этап реализации ничего не публикует. M3 может выпускаться отдельно.
