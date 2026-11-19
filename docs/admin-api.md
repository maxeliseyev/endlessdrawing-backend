# Admin API v1 — общий контракт backend / frontend

Статус: доверенный закрытый production HTTPS работает; реальная браузерная интеграция проверяется с PostgreSQL и виртуальными UV passkeys. **Новая политика владельца: пароль + один UV passkey, Google Authenticator (TOTP) — альтернативный резервный фактор, не дополнительный шаг после Touch ID.** Точные границы — в разделе «Готовность» ниже. Требования: [модерация](moderation.md). Этот файл — единственный источник wire-форм для двух сессий; изменения форм согласовывать здесь, не изобретать второй контракт во frontend.

## Инварианты

- Отдельный `/admin/`, без imports/ссылок/shortcut в публичном приложении. EN/RU UI.
- По явному решению владельца используется пароль **и обычный WebAuthn passkey с UV required**, включая Touch ID/platform и синхронизируемые passkeys. Явный резерв — пароль + ранее подтверждённый TOTP. Password-only сессии нет. Аттестация устройства и CA trust store не требуются. Backend по умолчанию admin-disabled до явной настройки HTTPS RP/origin.
- Только server cookie `__Host-ed-admin`, HttpOnly/Secure/SameSite=Strict/Path=/, без Domain. Pre-auth cookie `__Host-ed-admin-preauth` прав не даёт.
- Все fetch: `credentials: "same-origin"`, без CORS, секретов в Storage/VITE/URL/логах.
- Все изменяющие запросы: JSON, `X-CSRF-Token`, browser Origin должен точно соответствовать admin origin. Cookie не расширяет права публичного WS.
- CSRF возвращается API и хранится только в памяти admin entry; он привязан к cookie и её назначению, стабилен до её ротации.
- Служебные ответы: no-store, noindex, anti-framing. Ошибки не содержат входной payload.

## 1. Инициализация и вход

`GET /api/admin/session`:

- `200` — полная session response (ниже), можно показать инструменты.
- `401` — показать вход; сначала `GET /api/admin/login/options`.
- `503 admin_unavailable` — сервер не настроен для модерации; это не приглашение сделать локальный bypass.

`GET /api/admin/login/options` выставляет/reuses pre-auth cookie:

```json
{"csrf_token":"<base64url>","expires_at":"2026-11-16T12:05:00Z","authenticator_policy":"passkey_uv"}
```

`POST /api/admin/login`, header `X-CSRF-Token` из login/options:

```json
{"login":"moderator","password":"<password>"}
```

`200` после верного пароля и завершённого provisioning:

```json
{
  "status":"mfa_required",
  "webauthn":{"publicKey":{"challenge":"<base64url>","rpId":"endlessdrawing.com","timeout":120000,"allowCredentials":[],"userVerification":"required"}},
  "expires_at":"2026-11-16T12:02:00Z"
}
```

`webauthn` — полная стандартная сериализованная `RequestChallengeResponse` webauthn-rs. Реальный allowCredentials содержит id/type и, если доступны, transports; не подменять пример пустым массивом. Возможны дополнительные стандартные поля, в том числе mediation.

Frontend декодирует challenge и allowCredentials[].id в ArrayBuffer и вызывает `navigator.credentials.get({ publicKey, ... })`. В новых браузерах допустим `PublicKeyCredential.parseRequestOptionsFromJSON`, иначе нужен совместимый base64url adapter.

`POST /api/admin/login/verify`, тот же pre-auth CSRF:

```json
{
  "credential": {
    "id":"<base64url>","rawId":"<base64url>","type":"public-key",
    "response":{"authenticatorData":"<base64url>","clientDataJSON":"<base64url>","signature":"<base64url>","userHandle":null},
    "extensions":{}
  }
}
```

Buffer-поля кодировать base64url без padding. `extensions` — результат `getClientExtensionResults()`; поля уточняются по сериализации используемого proto и fixtures, не сериализовать браузерный объект через один `JSON.stringify(credential)` (он не выдаёт нужные buffers).

`200` — новая полная cookie, старая pre-auth недействительна. Session response:

```json
{
  "authenticated":true,
  "auth_method":"passkey_uv",
  "account":{"id":"94dc430b-83c1-4d87-9797-66fa696c7b28","login":"moderator"},
  "csrf_token":"<base64url>",
  "expires_at":"2026-11-16T20:00:00Z",
  "idle_expires_at":"2026-11-16T12:15:00Z"
}
```

Challenge 120 секунд, pre-auth 5 минут, полная сессия: idle 15 минут / absolute 8 часов. Каждый допущенный к проверке WebAuthn verify attempt потребляет challenge, включая неверную подпись; при ошибке/отмене/истечении заново пройти password → challenge, не повторять assertion. Отмена browser dialog не открывает права и не вызывает password-only fallback.

`POST /api/admin/logout` с полным session CSRF и `{}` body → `204`, серверный отзыв и очистка cookie. `401` также закрывает UI.

### Резерв Google Authenticator (TOTP)

Сессия `auth_method` — `passkey_uv` либо `totp`. `authenticator_policy` в options относится к **основному WebAuthn UV**; резерв выбирается явно отдельной кнопкой.

- `POST /api/admin/login/totp` `{ "login":"moderator", "password":"<password>", "code":"<6 ASCII digits>" }`: тот же pre-auth cookie/CSRF из login/options; `200` SessionReply + новая полная cookie. Password rate limits общие; backup должен быть подтверждён заранее. Wrong/missing/expired/replayed codes/password → generic 401; password-only сессии нет.
- `GET /api/admin/totp/status` → `200 {"available":true,"enabled":false,"fresh_passkey":true}`. Требует полную session, secret не возвращается.
- `POST /api/admin/totp/setup/start` `{}` → `200 {"secret":"<Base32>","otpauth_uri":"otpauth://totp/<label>?secret=<Base32>&issuer=Endless%20Drawing&algorithm=SHA1&digits=6&period=30","expires_at":"<RFC3339>"}`. Только CSRF + primary UV session, созданная менее 5 минут назад. QR генерировать локально; никогда не отправлять URI внешнему QR-сервису и не сохранять secret в Storage/логи/URL страницы. Pending secret привязан к конкретной session, expires 5 минут; active secret повторно не выдаётся.
- `POST /api/admin/totp/setup/confirm` `{"code":"<6 ASCII digits>"}` → `204`; та же fresh UV session и её pending setup. Только после верного кода backup enabled. Код подтверждения сразу consumed, для backup login дождаться нового кода.
- `POST /api/admin/totp/disable` `{}` → `204`; только fresh UV session. TOTP sessions не могут управлять факторами: 403 даже при действующей cookie/CSRF. После изменения backup все другие sessions и старые ceremonies/grants отзываются; текущая fresh primary session сохраняется.

TOTP: RFC 6238 SHA1, 6 digits/30s, +/-1 step; counter атомарно consumed с session creation под account lock, replay и concurrent duplicate отклоняются. Никаких raw codes/secrets в audit. Private server key file и encrypted backup/recovery: [операционный runbook](admin-totp.md).

## 2. Просмотр и скрытие

Просмотр мира — обычный WS v1 с отдельным случайным анонимным drawing secret. Выбор любых штрихов в admin UI не меняет их own. Публичное рисование/ластик в служебном режиме отключить; admin hide только через HTTP.

`POST /api/admin/strokes/hide`, полный session CSRF:

```json
{"operation_id":"4df8a775-7e7e-4707-a747-58d6c87d7019","stroke_ids":["87a122fa-7537-4b88-9b77-b45d24a29714"],"reason":"Moderator reason"}
```

Пределы: 1–100 **различных** UUID, причина trim/nonempty, максимум 1000 Unicode chars / 4000 UTF-8 bytes. UI: подсветка выбранного, число/UUID, причина, явное подтверждение, Escape отменяет без запроса.

`200` только после commit:

```json
{"operation_id":"4df8a775-7e7e-4707-a747-58d6c87d7019","hidden":["87a122fa-7537-4b88-9b77-b45d24a29714"],"already_hidden":[]}
```

Batch атомарный: неизвестный UUID → 404 и ни одного нового скрытия. Уже удалённые строки — already_hidden. Тот же operation_id + actor + нормализованный payload → тот же результат без второго broadcast/audit; другой payload/actor → 409. Порядок UUID в запросе не влияет на идемпотентность.

`GET /api/admin/operations/{operation_id}` → такой же результат для авторизованного автора операции; неизвестный/чужой → 404.

После потери ответа проверить operation или повторить **тот же** UUID/payload. Не объявлять успех до ответа; при новом входе не пересылать старое скрытие автоматически. В публичный WS идут обычные stroke.deleted без reason/actor. Snapshot/reconnect и undo не воскрешают скрытое.

## 2a. Жалобы (App Review 1.2)

Контракт WS-жалоб — [protocol-v5-safety.md](protocol-v5-safety.md). Те же сессия, MFA, CSRF (для POST), rate limit и аудит, что у hide.

`GET /api/admin/reports` — очередь открытых жалоб, агрегированная по штрихам, до 200 штрихов: удержанные порогом первыми, затем по последней жалобе.

```json
{"open_reports":2,"items":[{"stroke_id":"…","reports":3,"reporters":3,"reasons":["hate","spam"],"block_requests":2,
  "first_at":"2026-11-19T09:00:00Z","last_at":"2026-11-19T09:05:00Z","held":true,"visible":false,"x":12.5,"y":-40.0}]}
```

Решения (тело как у hide: `operation_id`, 1–100 различных `stroke_ids`, `reason`; тот же id + actor + нормализованный payload → тот же результат без второго broadcast; другой payload или другое действие под тем же id → 409; неизвестный UUID → 404 без изменений):

- **Скрыть** — существующий `POST /api/admin/strokes/hide`. Дополнительно закрывает открытые жалобы на эти штрихи (`actioned`) и удержание (`hidden`).
- `POST /api/admin/reports/dismiss` → `{"operation_id","restored":[…],"dismissed_reports":n}`. Закрывает жалобы (`dismissed`); удержанные порогом штрихи снова публичны (`stroke.created` со старым seq).
- `POST /api/admin/bans` → `{"operation_id","banned_authors":n,"hidden":[…]}`. Банит авторов выбранных штрихов, скрывает все их видимые штрихи (`stroke.deleted` каждому подписчику), закрывает их жалобы и удержания. Бан действует на сессию автора: create/report → `banned`.

`GET /api/admin/operations/{id}` возвращает результат любой из трёх операций в её форме.

Уведомление: VPS-монитор (`endlessdrawing-monitor reports`, таймер `endlessdrawing-reports.timer`, раз в минуту) читает loopback-only `/internal/moderation/summary` и пишет в Telegram о новых жалобах и о жалобе, ждущей больше 20 часов. Только счётчики и ссылка на `/admin/`.

## 3. Закрытое enrollment (не обязательный первый UI-этап)

Аккаунт и одноразовый enrollment token создаются CLI по SSH, никогда публичной регистрацией. Токен передаётся только HTTPS body и не даёт административную сессию. Enrollment UI — отдельный режим внутри admin entry без публичных ссылок. Если frontend пока его не реализует, сообщить явно: production provisioning останется заблокирован.

`POST /api/admin/enrollment/start`, pre-auth CSRF:

```json
{"login":"moderator","password":"<password>","enrollment_token":"<one-time token>","label":"Primary key"}
```

`200`: `{"webauthn":<CreationChallengeResponse>,"expires_at":"<RFC3339>"}`. Декодировать creation publicKey.challenge, user.id, excludeCredentials[].id; вызвать navigator.credentials.create().

`POST /api/admin/enrollment/finish`, pre-auth CSRF:

```json
{"credential":{"id":"<base64url>","rawId":"<base64url>","type":"public-key","response":{"attestationObject":"<base64url>","clientDataJSON":"<base64url>","transports":["internal"]},"extensions":{}}}
```

`200`: `{"registered_credentials":1,"required_credentials":1,"complete":true}`. Достаточно **одного** UV passkey; уже сохранённые credentials не меняются. Перейти к **новому обычному входу** с паролем+passkey — enrollment не создаёт сессию. Grant по-прежнему 15 минут/max 2 registrations (второй passkey необязателен). Затем владелец может подключить TOTP-резерв после fresh primary login. Не выполнять recovery для уже успешно зарегистрированного одного ключа только ради новой политики.

## 4. Ошибки

Единая форма: `{"error":{"code":"unauthorized","message":"Authentication required or failed"}}`.

| HTTP | code | Действие frontend |
| --- | --- | --- |
| 400 | invalid_request | Показать ошибку формы, ничего не считать выполненным |
| 401 | unauthorized | Закрыть права/показать вход; без автоповтора hide |
| 403 | forbidden | Origin/CSRF отказ; не обходить клиентом |
| 404 | stroke_not_found / operation_not_found | Batch не применён / проверить тот же operation позже при неопределённой сети |
| 409 | operation_conflict | Не генерировать новый UUID молча, показать конфликт |
| 413 | payload_too_large | Уменьшить batch/body |
| 429 | rate_limited | Respect Retry-After, без busy-loop |
| 503 | admin_unavailable / temporarily_unavailable | Не показывать ложный успех; возможен неопределённый commit hide |

Предел body 64 KiB. Неизвестные поля верхнего уровня admin-запросов отклоняются; стандартные вложенные WebAuthn структуры/extensions обрабатываются по правилам webauthn-rs. Ни один код не означает «разрешить локальные admin инструменты без сервера».

## Готовность / передача между сессиями

- Backend: 63 стандартных теста прошли с PostgreSQL, из них 16 admin-тестов. Отдельный реальный frontend/browser integration также прошёл — [доказательства и повторение](admin-integration.md). `fmt --check` и Clippy `--locked --all-targets -D warnings` проходят. Все перечисленные API-маршруты упражняются HTTP-тестами; CLI create/recover/disable проверяются через provisioning service. Реальный интерактивный provisioning на VPS пока не пройден.
- Passkey API webauthn-rs: обычное `attestation: none`, UV required, platform/roaming без принудительного attachment, синхронизируемые credentials разрешены. Программный ES256 fixture без attestation certificate проходит обычные registration/signature проверки; нулевые counters и изменения backup state проверены. Это не физический Touch ID.
- Проверены отказ без полного MFA, неверные подпись/origin/RP/challenge/UV/userHandle, истечение/replay и конкурентный verify (ровно одна сессия), текущий device-bound counter, login/logout CSRF, session expiry и восстановление. Logout/recovery блокируют hide, который прошёл HTTP-проверку, но ещё ждёт writer.
- Hide: atomic unknown UUID/audit rollback, идемпотентность, обычный WS с admin-cookie не получает чужой delete; live deletion видна двум клиентам, snapshot исключает скрытое, авторский create/retry не восстанавливает. Deferred COMMIT failure вызывает resync_required и допускает безопасный retry после свежего snapshot. Потеря TCP-ответа после фактически успешного COMMIT отдельно не инжектировалась.
- Native ARM64 combined Docker build/smoke и 11 Linux deployment-тестов прошли с новой админкой: оба HTML/assets, CLI без TTY запрещён, admin API выключен по умолчанию, probes/non-root/read-only и backup проверены. `/admin/` HTML может объяснить отключённый API, но не даёт прав. AMD64 проходит тот же gate в Actions.
- Frontend `623c5a6`: отдельный entry, typed API, buffer adapters, enrollment/login и selection/confirmation реализованы. Совместная Chromium HTTPS/WebAuthn/API/WS/DB интеграция прошла с виртуальными аутентификаторами, без HTTP/WS mocks. Вход обоими credentials, secure cookie, hide, аудит, два независимых WS clients, snapshot и logout проверены. Криптографические fixtures находятся в `tests/admin.rs` и `tests/support/passkey.rs`; примеры wire-форм выше содержат placeholders, это не реальные credentials.
- Реальные Touch ID/iCloud, независимый резервный доступ и cross-browser enrollment/login на HTTPS origin пока не проверены. Сервер проверяет UV, не биометрию; ОС вправе предложить другую системную проверку. [Provisioning runbook](admin-operations.md).
- `cargo audit` **не чистый**: три транзитивные записи, применимость и дальнейшие действия в [security-dependencies.md](security-dependencies.md). Не объявлять публичную security-готовность по успешным функциональным тестам. TLS, сетевой gate и внешние backup/restore остаются обязательными.
