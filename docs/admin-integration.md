# Совместная проверка и интегрированный staging-релиз

Frontend: `623c5a6b3bbbf4cfc549f1329af0aa64c9fc3771` (`feat: add isolated admin frontend with passkey authentication and moderation`). Backend собирает его целый `dist/` в один образ; копировать исходники приложений между репозиториями или сливать их Git history не нужно.

## Что проверено локально

- Frontend: TypeScript, 97 unit-тестов, production build двух HTML entry.
- Chromium / Firefox / WebKit: полный повторный прогон — 127 passed, 2 CDP-only skip. В первом прогоне один Firefox mock-test 429 потерял выбор до открытия dialog; отдельные 5 повторов и затем весь suite прошли. Это отмеченная нестабильность теста, не основание отключать проверки безопасности.
- Backend: 63 стандартных теста, fmt/Clippy, 11 Linux deployment-тестов.
- Отдельный **реальный совместный** Chromium-тест: production frontend assets → HTTPS reverse proxy → Axum → PostgreSQL. Ни HTTP, ни WS, ни `navigator.credentials` не замоканы. Только аутентификаторы — виртуальные CDP UV devices.
- Две регистрации passkeys, отсутствие сессии после enrollment, вход обоими credentials, HttpOnly/Secure/SameSite cookie, read-only admin WS snapshot, выбор чужого штриха, причина/подтверждение и атомарное скрытие с аудитом.
- Два независимых браузерных **WS protocol clients** получают deletion; новый snapshot не возвращает скрытое; чужой public delete отклоняется. Это не подключение публичного drawing UI к backend: `/` остаётся локальным прототипом.
- Logout обоих сеансов подтверждён HTTP 401; test DB содержит два credentials, одну hide operation/аудит, один tombstone, ноль неотозванных sessions.
- Native ARM64 combined-image smoke: оба HTML/JS entry, no-store/anti-framing для `/admin/`, отключённый API возвращает 503, CLI закрыт без TTY, приложение non-root/read-only, БД без published port, backup читается.

## Повторить реальную интеграцию

Нужны Node >=22.12, OpenSSL, PostgreSQL, собранный frontend и Playwright Chromium:

```bash
# Из frontend (предпочтительно отдельный checkout зафиксированного SHA):
npm ci
npm run check && npm test && npm run build
npx playwright install chromium

# Из backend, только локальная/CI тестовая БД, НИКОГДА production DSN:
docker compose --env-file .env.example up -d --wait postgres
FRONTEND_DIR="$PWD/../endless-drawing-frontend" \
DATABASE_URL=postgres://canvas:canvas_dev_only@127.0.0.1:55432/canvas \
  cargo test --locked --features browser-integration --test admin_browser -- --ignored
```

`tests/admin_browser.rs` создаёт отдельную sqlx test DB и эфемерный account/grant. `tests/browser/admin.mjs` генерирует однодневный self-signed localhost TLS certificate, запускает только loopback proxy и CDP authenticator. Только этот тестовый browser context игнорирует недоверенный тестовый TLS certificate; production Origin, cookies и WebAuthn проверки не ослабляются. Fixture password/token передаются дочернему процессу через stdin, не argv/URL/логи; приватные виртуальные credentials живут только в памяти теста. Успех не означает настоящий Touch ID, доверенный production TLS или независимость реальных резервных провайдеров.

## Deployment gate

Manual Actions `Deploy staging` теперь до image build/SSH выполняет этот тест с зафиксированным `frontend_ref`, PostgreSQL 17 и Chromium. Затем собирает linux/amd64 образ, делает container smoke и штатный backup/readiness deploy. Release manifest и image labels фиксируют оба SHA. Автоматического публичного deploy с push main нет.

Актуальные Compose/helper установлены на VPS штатным bootstrap; существующие runtime.env, DB credentials и volume сохранены. Вход администратора остаётся выключенным (`ADMIN_ENABLED=false` по умолчанию). HTML `/admin/` можно открыть для проверки загрузки/сообщения «администрирование не настроено», но все admin API, включая enrollment/login/session/hide, при выключенном admin возвращают 503.

## Ещё не пройдено

- Настоящий Touch ID / резервный passkey provider на доверенном окончательном HTTPS origin; закрытый пользовательский SSH provisioning без захвата capability агентом.
- Публичный read/write frontend realtime (сейчас только admin имеет WS transport), server-side chisel/spray и их end-to-end persistence.
- Production HTTPS/network gate, замечания dependency audit, DB roles и внешние backup/restore. См. [admin-operations.md](admin-operations.md), [security-dependencies.md](security-dependencies.md).

Закрытая выкладка новой сборки не разрешает публичный запуск продукта и не доказывает сохранение штрихов локального drawing-прототипа.
