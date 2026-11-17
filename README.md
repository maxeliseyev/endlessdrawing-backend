# Endless Drawing Backend

Backend общего бесконечного холста на **Rust + Axum + Tokio + SQLx + PostgreSQL**.

Требования: [backend](docs/backend.md), [JSON-протокол v1](docs/protocol.md), [frontend](docs/frontend.md).

Эксплуатация: [GitHub Actions → закрытый VPS-стенд](docs/deployment.md), [зашифрованные внешние бэкапы](docs/backups.md), [Telegram monitoring](docs/monitoring.md), [закрытый HTTPS/Caddy](docs/https.md), [gates публичного запуска](docs/publication.md). Модерация: [Admin API](docs/admin-api.md), [закрытый provisioning / Touch ID](docs/admin-operations.md), [совместная интеграция frontend/backend](docs/admin-integration.md).

## Текущий этап

Реализованы основа и realtime-ядро, **не весь MVP**:

- HTTP-сервер, конфигурация окружения, корректное завершение по Ctrl+C/SIGTERM;
- пул PostgreSQL с таймаутами, автоматические embedded-миграции перед стартом HTTP;
- `GET /healthz` и `GET /readyz`, опциональная раздача сборки frontend;
- таблица векторных штрихов, soft-delete поле и частичные GiST/owner индексы;
- типизированная валидация точек и кисти, нормализация цвета, серверный bbox;
- SHA-256 от декодированного 256-битного секрета сессии, без публичной сериализации digest;
- WS `/ws`: hello, create/delete, серверные ack/error и отдельный `own` для каждого получателя;
- идемпотентность UUID, проверка владельца и soft delete; повторы не создают новые строки/события;
- один последовательный обработчик мутаций, публикация только после commit;
- многочастный snapshot области из REPEATABLE READ + последующие события без окна потерь;
- ограниченные очереди/соединения/пакеты, Origin allowlist, sliding-window rate limits, heartbeat и таймауты;
- opt-in Admin API: пароль + UV-required passkeys (Touch ID/platform/synced), отзываемые server-cookie sessions, закрытый provisioning CLI и атомарное аудируемое скрытие через тот же writer;
- unit/HTTP/WebAuthn-тесты, integration-тесты PostgreSQL и тесты двух реальных WebSocket-клиентов.

Ручной Actions workflow выполняет backend CI, checkout/build отдельного frontend, реальные public/admin browser gates, combined Docker image, backup/Caddy/container smoke, SSH-доставку, pre-deploy dump и health-checked активацию/попытку image rollback (БД автоматически не восстанавливается). [Закрытый release 44074630461](https://github.com/maxeliseyev/endlessdrawing-backend/actions/runs/44074630461): backend `87fa802`, frontend `b631892`, admin API включён на доверенном HTTPS. По новому решению владельца основной вход — password + один UV passkey, Google Authenticator — альтернативный password+TOTP резерв вместо обязательного второго passkey. Caddy допускает только operator `/32` и loopback; backend/БД напрямую не опубликованы. Настоящий аккаунт/Touch ID создаёт и принимает владелец, не агент.

Frontend `/admin/` реализован и прошёл совместный Chromium → HTTPS → backend/PostgreSQL тест с виртуальными UV passkeys; перед выкладкой это обязательный Actions gate. **Следующий этап:** настоящий вход существующим Touch ID, настройка/проверка Google Authenticator и live moderation на закрытом HTTPS-стенде, review [замечаний зависимостей](docs/security-dependencies.md) и ограничения по IP. Внешние age-бэкапы в отдельный Storage Box subaccount уже активированы после настоящего operator decrypt/restore; мониторинг алертов и provider deletion protection остаются отдельными gates. Публичный frontend теперь подключён к `/ws` (create/delete/undo/reconnect) и прошёл совместный тест с этим backend и PostgreSQL в Chromium/Firefox/WebKit (`npm run test:integration` во frontend); тот же public suite теперь также passed в трёх браузерах по настоящему закрытому staging HTTPS. Это не проверка нагрузки или физических iPhone/iPad. До завершения этих этапов сервис не предназначен для публичного запуска. Отдельная HTTP-ручка выборки/изменения штрихов не реализуется: основной путь — WebSocket.

Rust-пакет оставлен в корне отдельного backend-репозитория, без дополнительной папки `backend/`.

## Локальный запуск

Нужны актуальный stable Rust и Docker с Compose. Frontend для запуска backend не требуется.

```bash
cp .env.example .env
docker compose up -d --wait postgres
cargo run
```

Перед запуском HTTP применяются встроенные миграции из `migrations/`, включая таблицы модерации. Никакая SQLx CLI для этого не нужна. Миграции встроены в бинарник при компиляции; уже применённые файлы не редактировать — добавлять новые.

```bash
curl -i http://127.0.0.1:3000/healthz
# 200 {"status":"ok"}

curl -i http://127.0.0.1:3000/readyz
# 200 {"status":"ready"}
```

`/healthz` проверяет только процесс. `/readyz` проверяет доступ к БД и таблице `strokes`, возвращает `503 {"status":"not_ready"}` при ошибке/таймауте. На пустой таблице readiness успешна. При ошибке подключения или миграции на старте процесс завершается, не начиная слушать HTTP.

Без `STATIC_DIR` путь `/` возвращает 404 — это нормально до подключения frontend.

### Конфигурация

`.env` загружается при запуске бинарника, но не перезаписывает уже заданные переменные окружения. Его отсутствие допустимо, если окружение настроено другим способом.

| Переменная | Значение по умолчанию / назначение |
| --- | --- |
| `DATABASE_URL` | Обязательный URL `postgres://...` или `postgresql://...` |
| `BIND_ADDR` | `127.0.0.1:3000` |
| `DATABASE_MAX_CONNECTIONS` | `10` |
| `DATABASE_ACQUIRE_TIMEOUT_MS` | `5000`, ожидание соединения |
| `DATABASE_QUERY_TIMEOUT_MS` | `5000`, PostgreSQL statement timeout и HTTP readiness timeout |
| `STATIC_DIR` | Не задана; путь к сборке Vite с `index.html` |
| `STROKE_MAX_POINTS` | `2000`, максимум точек в штрихе |
| `STROKE_MAX_SPAN` | `100000`, максимум размаха точек по каждой оси до расширения bbox |
| `RUST_LOG` | `info` |
| `ADMIN_ENABLED` | `false`; admin API по умолчанию выключен |
| `ADMIN_ORIGIN` | При включении обязателен точный HTTPS origin без `/` на конце |
| `ADMIN_RP_ID` | При включении обязателен соответствующий RP ID, например `endlessdrawing.com` |

`STROKE_MAX_SPAN=100000` — выбранный начальный защитный предел, не фиксированное число исходного ТЗ. Перед интеграцией согласовать его с frontend. Ширина `[1,64]`, opacity `(0,1]`, диапазон координат `[-1e9,1e9]` фиксированы контрактом.

Кисти: `round`, `square`, `chisel`, `spray` (миграция `0003_chisel_spray_brushes.sql`). Для квадратных концов и косого пера реализован консервативный запас bbox `width / sqrt(2) + 1`, для круглых и спрея — `width / 2 + 1`, согласно предлагаемым геометрическим договорённостям в `docs/protocol.md`. Настройки renderer обе команды ещё должны принять совместно.

`POSTGRES_DB`, `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_PORT` используются Compose. В `.env.example` они согласованы с `DATABASE_URL`; при изменении логина/пароля/порта обновить URL тоже. Переменные инициализации PostgreSQL действуют при создании **пустого** volume, а не меняют учётные данные существующей БД.

Не логировать `Config`, DATABASE_URL, секреты сессии или сырые WS-пакеты. SQLx statement logging отключён при создании application pool. `.env` исключён из Git, `.env.example` содержит только публичные локальные dev-значения.

### Подключение frontend-сборки

```bash
# После npm run build в frontend-репозитории:
STATIC_DIR=../endless-drawing-frontend/dist cargo run
```

Backend раздаёт только содержимое указанного каталога; не указывать корень репозитория или каталог с секретами. Отсутствующий `index.html` приводит к ошибке старта. Неизвестные страницы не подменяются index.html. Для frontend dev-сервера `/ws` можно проксировать на backend с того же origin; этот browser origin добавить в `ALLOWED_ORIGINS`.

## WebSocket: подключение frontend

Адрес локально: `ws://127.0.0.1:3000/ws`, production — `wss://<домен>/ws`. Формы сообщений приведены в [контракте](docs/protocol.md#10-реализованный-профиль-backend).

1. Генерировать 32 случайных байта через Web Crypto, хранить base64url-секрет без padding в `sessionStorage`.
2. Отправить `hello`, дождаться `hello.ack`.
3. Отправить `viewport.subscribe` с новым `subscription_id` и prefetch bounds.
4. Получить begin → chunks → end, затем `stroke.created` / `stroke.deleted`.
5. Для create/delete использовать `request_id`, сопоставлять ack/error. Повтор create после неопределённого результата — с тем же UUID и payload.
6. После reconnect или `resync_required` загрузить новый snapshot, не восстанавливать поток только по `seq`.

Браузер сам посылает Origin; запросы без Origin, с несколькими Origin или вне allowlist отклоняются с HTTP 403. Node/CLI-клиенту нужно явно передать разрешённый Origin. Указание Origin не заменяет rate limiting и не является авторизацией владельца.

### Realtime-конфигурация

| Переменная | По умолчанию |
| --- | --- |
| `ALLOWED_ORIGINS` | `http://localhost:5173,http://127.0.0.1:5173,http://localhost:3000,http://127.0.0.1:3000` |
| `WS_MAX_CONNECTIONS` | `256` на процесс, включая ожидающие hello |
| `WS_MAX_MESSAGE_BYTES` | `131072` (128 KiB), также предел входящего frame |
| `MUTATION_QUEUE_CAPACITY` | `128` команд, включая snapshot barriers |
| `EVENT_BUFFER_CAPACITY` | `256` событий общего broadcast-ring |
| `SNAPSHOT_PAGE_SIZE` | `100` штрихов на страницу БД, максимум `1000` |
| `SNAPSHOT_CHUNK_BYTES` | `262144` (256 KiB) на сериализованный chunk |
| `VIEWPORT_MAX_SPAN` | `30000` мировых единиц по каждой оси |
| `WS_CREATES_PER_SECOND` | `5` |
| `WS_DELETES_PER_SECOND` | `20` |
| `WS_SUBSCRIBES_PER_SECOND` | `5` |
| `WS_MESSAGES_PER_SECOND` | `100`, включая control frames после hello |
| `WS_HELLO_TIMEOUT_MS` | `5000` |
| `WS_SEND_TIMEOUT_MS` | `5000` на отправку |
| `WS_OPERATION_TIMEOUT_MS` | `10000` на ожидание результата команды |
| `SNAPSHOT_TIMEOUT_MS` | `30000` на всю загрузку снимка |
| `WS_HEARTBEAT_INTERVAL_MS` | `20000` |
| `WS_IDLE_TIMEOUT_MS` | `60000` без входящих frames |

Origin задавать целиком (`https://canvas.example.com`), без пути, `*`, завершающего `/` и credentials. Все таймауты положительные, не более часа; heartbeat должен быть короче idle. Настройки проходят проверку перед стартом. Соединения, очереди и rate limits ограничены сверху 65536; входящие сообщения — 1 MiB, chunk — 4 MiB.

Rate limits используют скользящее окно в одну секунду, без удвоенного burst на границе секунд. Превышение лимита операции возвращает `rate_limited`; превышение общего frame rate закрывает соединение. При исчерпании числа соединений Upgrade возвращает 503. Слишком большой пакет/повреждённый транспорт приводит к закрытию WS, JSON-ошибка не гарантируется.

События удерживаются в общем ограниченном ring, пока идёт snapshot; его overflow даёт `resync_required`, а не тихий пропуск. Большой трафик в других областях тоже может вызвать resync. Snapshot-worker держит не более одной страницы БД и двух ожидающих сообщений; смена подписки/закрытие соединения отменяет старую задачу и освобождает read-транзакцию. Размер chunk должен вмещать хотя бы один целый максимальный штрих и envelope: не уменьшать его ниже размеров уже сохранённых объектов.

Timeout/разрыв до ack не означает, что запись не закоммичена. При shutdown выполняющаяся мутация может завершиться; ещё ожидающие команды отменяются. UUID обеспечивает безопасный повтор. Продолжать работу с несколькими backend-инстанциями или писать штрихи напрямую в БД нельзя: это обходит единый порядок публикации.

## Тесты

Без PostgreSQL:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
for script in deploy/*.sh; do bash -n "$script"; done
python3 -m unittest discover -s tests/deploy -v
```

По умолчанию тесты, требующие PostgreSQL (включая WebSocket и проверки writer), помечены `ignored`, чтобы остальные unit-тесты запускались без БД. Для полного набора:

```bash
docker compose up -d --wait postgres
# Rust integration tests не загружают .env автоматически.
# Если локальные реквизиты изменены, подставить актуальный URL.
DATABASE_URL=postgres://canvas:canvas_dev_only@127.0.0.1:55432/canvas \
  cargo test --all-targets -- --include-ignored
```

SQLx создаёт отдельную временную БД для каждого DB-теста и удаляет её при успехе. Нужен пользователь с правом `CREATEDB` (локальный Compose-пользователь это умеет). Не использовать production DSN. Некоторые тесты напрямую вставляют fixture-данные — это не production API записи.

Проверяются валидация и bbox, строгий входной payload, секрет сессии, конфигурация без утечек, probes/статика, миграции и индексы, JSONB/box, выборка со страницами, soft delete и ограничения БД. Realtime-тесты проверяют два независимых клиента, reload/ownership, чужое удаление, повтор UUID/конфликт/tombstone, offscreen ack, byte-bounded chunks, смену подписки, snapshot при create/delete, overflow буфера, отсутствие событий при rollback, последовательность параллельных мутаций, Origin/hello/payload/connection/rate limits.

Проверены 66 стандартных backend-тестов, включая 16 admin-тестов и WS round-trip/retry/conflict/snapshot для `chisel`/`spray`, плюс отдельный совместный browser integration test: реальные ES256-подписи программных аутентификаторов, UV/RP/challenge/replay/expiry, синхронизируемые passkeys с нулевым counter, конкурентный verify, logout/recovery перед queued hide, атомарность аудита и COMMIT failure → resync/retry. Это не проверка настоящего Touch ID или браузерной интеграции. `cargo audit` пока сообщает транзитивные advisories: [оценка и ограничения](docs/security-dependencies.md).

CI выполняет тот же полный Rust-набор с PostgreSQL 17 и тесты deployment helper (mock Docker, не реальная БД). Deploy workflow дополнительно проверяет frontend check/unit/build и запускает собранный amd64-образ с одноразовым PostgreSQL через production Compose.

## PostgreSQL и эксплуатационные ограничения

Compose предназначен **только для локальной разработки**: PostgreSQL опубликован на `127.0.0.1:55432`, данные сохраняются в named volume. Не выставлять этот порт в Интернет. HTTP также по умолчанию слушает только loopback.

```bash
docker compose stop postgres
# Для повторного старта:
docker compose up -d --wait postgres
```

Обычная остановка сохраняет данные. Не удалять volume, если нужны сохранённые штрихи.

Целевая инфраструктура — один VPS для frontend, backend и PostgreSQL, **одна** backend-инстанция. Для публичного развёртывания ещё предстоит настроить TLS/reverse proxy, отдельные production-реквизиты, browser E2E закрытого административного скрытия, review зависимостей, защиту подключений по IP, внешние резервные копии и проверку восстановления. Пароли/ключи/сессионные токены администратора никогда не передаются frontend через `VITE_*`; предусмотрена отдельная серверная MFA-сессия, не универсальный admin-secret.

## Структура

```text
src/main.rs      # env/logging, bootstrap, signals
src/app.rs       # HTTP router, probes, static assets
src/admin/       # password + WebAuthn passkeys, sessions, audited moderation
src/bin/canvas-admin.rs # interactive trusted-terminal create/recover/disable
src/config.rs    # validated environment settings
src/db.rs        # pool setup, embedded migrator
src/domain.rs    # stroke validation, normalized payload, bounds
src/session.rs   # strict session-secret decoding and digest
src/protocol.rs  # JSON v1 message types, safe public serialization
src/storage.rs   # parameterized typed storage operations
src/realtime.rs  # sequential writer, post-commit events, snapshot barrier
src/snapshot.rs  # bounded/cancellable snapshot streaming
src/ws.rs        # Origin, hello, commands, subscriptions, socket lifecycle
src/realtime_settings.rs # defaults for configurable realtime limits
migrations/      # PostgreSQL schema
tests/           # PostgreSQL, WebSocket and deployment tests
deploy/          # private staging Compose, explicit bootstrap, activation/rollback
Dockerfile       # non-root Rust runtime + separately built Vite dist
.github/workflows/deploy.yml # manual verified GitHub Actions deployment
```
