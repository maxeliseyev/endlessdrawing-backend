# Endless Drawing Backend

Backend общего бесконечного холста на **Rust + Axum + Tokio + SQLx + PostgreSQL**.

Требования: [backend](docs/backend.md), [JSON-протокол v1](docs/protocol.md), [frontend](docs/frontend.md).

## Текущий этап

Реализована основа, **не весь MVP**:

- HTTP-сервер, конфигурация окружения, корректное завершение по Ctrl+C/SIGTERM;
- пул PostgreSQL с таймаутами, автоматические embedded-миграции перед стартом HTTP;
- `GET /healthz` и `GET /readyz`, опциональная раздача сборки frontend;
- таблица векторных штрихов, soft-delete поле и частичные GiST/owner индексы;
- типизированная валидация точек и кисти, нормализация цвета, серверный bbox;
- SHA-256 от декодированного 256-битного секрета сессии, без публичной сериализации digest;
- unit/HTTP-тесты и integration-тесты на настоящем PostgreSQL.

**Следующий этап:** репозиторий и последовательный обработчик мутаций, WS `/ws`, hello, create/delete с проверкой владельца и идемпотентностью, согласованный snapshot + live-события. Пока `/ws` и API изменения штрихов **не реализованы и возвращают 404**. Лимиты WS, rate limiting, административное скрытие и production-инфраструктура также впереди. Сервис пока не предназначен для публичного запуска.

Rust-пакет оставлен в корне отдельного backend-репозитория, без дополнительной папки `backend/`.

## Локальный запуск

Нужны актуальный stable Rust и Docker с Compose. Frontend для запуска backend не требуется.

```bash
cp .env.example .env
docker compose up -d --wait postgres
cargo run
```

При первом запуске применится `migrations/0001_create_strokes.sql`. Никакая SQLx CLI для этого не нужна. Миграции встроены в бинарник при компиляции; уже применённые файлы не редактировать — добавлять новые.

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

`STROKE_MAX_SPAN=100000` — выбранный начальный защитный предел, не фиксированное число исходного ТЗ. Перед интеграцией согласовать его с frontend. Ширина `[1,64]`, opacity `(0,1]`, диапазон координат `[-1e9,1e9]` фиксированы контрактом.

Для квадратных концов реализован консервативный запас bbox `width / sqrt(2) + 1`, для круглых — `width / 2 + 1`, согласно предлагаемым геометрическим договорённостям в `docs/protocol.md`. Настройки renderer обе команды ещё должны принять совместно.

`POSTGRES_DB`, `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_PORT` используются Compose. В `.env.example` они согласованы с `DATABASE_URL`; при изменении логина/пароля/порта обновить URL тоже. Переменные инициализации PostgreSQL действуют при создании **пустого** volume, а не меняют учётные данные существующей БД.

Не логировать `Config`, DATABASE_URL, секреты сессии или сырые WS-пакеты. SQLx statement logging отключён при создании application pool. `.env` исключён из Git, `.env.example` содержит только публичные локальные dev-значения.

### Подключение frontend-сборки

```bash
# После npm run build в frontend-репозитории:
STATIC_DIR=../endless-drawing-frontend/dist cargo run
```

Backend раздаёт только содержимое указанного каталога; не указывать корень репозитория или каталог с секретами. Отсутствующий `index.html` приводит к ошибке старта. Неизвестные страницы не подменяются index.html. Для frontend dev-сервера будущий `/ws` следует проксировать на backend с того же origin.

## Тесты

Без PostgreSQL:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

По умолчанию 6 PostgreSQL-тестов помечены `ignored`, чтобы unit-тесты запускались без БД. Для полного набора:

```bash
docker compose up -d --wait postgres
# Rust integration tests не загружают .env автоматически.
# Если локальные реквизиты изменены, подставить актуальный URL.
DATABASE_URL=postgres://canvas:canvas_dev_only@127.0.0.1:55432/canvas \
  cargo test --all-targets -- --include-ignored
```

SQLx создаёт отдельную временную БД для каждого integration-теста и удаляет её при успехе. Нужен пользователь с правом `CREATEDB` (локальный Compose-пользователь это умеет). Не использовать production DSN. Тесты напрямую вставляют fixture-данные — это не production API записи.

Проверяются валидация и bbox, строгий входной payload, декодирование/хеширование секрета, конфигурация без утечек, health/ready/статика, повтор миграций, индексы, round-trip JSONB/box, выборка области со страницами, soft delete и ограничения БД. Авторизация пользовательского удаления и гонки snapshot будут тестироваться вместе с соответствующей реализацией.

CI выполняет тот же полный набор с PostgreSQL 17.

## PostgreSQL и эксплуатационные ограничения

Compose предназначен **только для локальной разработки**: PostgreSQL опубликован на `127.0.0.1:55432`, данные сохраняются в named volume. Не выставлять этот порт в Интернет. HTTP также по умолчанию слушает только loopback.

```bash
docker compose stop postgres
# Для повторного старта:
docker compose up -d --wait postgres
```

Обычная остановка сохраняет данные. Не удалять volume, если нужны сохранённые штрихи.

Для публичного развёртывания ещё предстоит настроить TLS/reverse proxy, отдельные production-реквизиты, закрытое административное скрытие, ограничения подключений и операций, резервное копирование и проверку восстановления. Административный секрет никогда не должен передаваться frontend через `VITE_*`.

## Структура

```text
src/main.rs      # env/logging, bootstrap, signals
src/app.rs       # HTTP router, probes, static assets
src/config.rs    # validated environment settings
src/db.rs        # pool setup, embedded migrator
src/domain.rs    # stroke validation, normalized payload, bounds
src/session.rs   # strict session-secret decoding and digest
migrations/     # PostgreSQL schema
tests/          # PostgreSQL integration tests
```
