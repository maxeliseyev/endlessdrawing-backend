# Admin API v1 — общий контракт backend / frontend

Статус: **backend реализован и проверен локальными HTTP/WebAuthn/PostgreSQL-тестами; на VPS не активирован, browser E2E не пройден**. Точные границы — в разделе «Готовность» ниже. Требования: [модерация](moderation.md). Этот файл — единственный источник wire-форм для двух сессий; изменения форм согласовывать здесь, не изобретать второй контракт во frontend.

## Инварианты

- Отдельный `/admin/`, без imports/ссылок/shortcut в публичном приложении. EN/RU UI.
- По явному решению владельца используется пароль **и обычный WebAuthn passkey с UV required**, включая Touch ID/platform и синхронизируемые passkeys. Password-only сессии нет. Аттестация устройства и CA trust store не требуются. Backend по умолчанию admin-disabled до явной настройки HTTPS RP/origin.
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
  "account":{"id":"94dc430b-83c1-4d87-9797-66fa696c7b28","login":"moderator"},
  "csrf_token":"<base64url>",
  "expires_at":"2026-11-16T20:00:00Z",
  "idle_expires_at":"2026-11-16T12:15:00Z"
}
```

Challenge 120 секунд, pre-auth 5 минут, полная сессия: idle 15 минут / absolute 8 часов. Каждый допущенный к проверке WebAuthn verify attempt потребляет challenge, включая неверную подпись; при ошибке/отмене/истечении заново пройти password → challenge, не повторять assertion. Отмена browser dialog не открывает права и не вызывает password-only fallback.

`POST /api/admin/logout` с полным session CSRF и `{}` body → `204`, серверный отзыв и очистка cookie. `401` также закрывает UI.

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

`200`: `{"registered_credentials":1,"required_credentials":2,"complete":false}`. Повторить start/finish с запасным ключом; при complete:true перейти к **новому обычному входу**, не считать enrollment административной сессией. Token ограничен двумя успешными регистрациями и 15 минутами. Основной и резервный passkeys должны иметь разные credential ID. Mac и iPhone в одном iCloud могут синхронизировать один credential: это не две регистрации и не независимые резервные провайдеры. Независимость резервного доступа проверяет оператор; возможны другой passkey provider либо отдельный security key с UV.

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

- Backend: 62 теста прошли с PostgreSQL, из них 15 admin-тестов. `fmt --check` и Clippy `--locked --all-targets -D warnings` проходят. Все перечисленные API-маршруты упражняются HTTP-тестами; CLI create/recover/disable проверяются через provisioning service. Реальный интерактивный provisioning на VPS пока не пройден.
- Passkey API webauthn-rs: обычное `attestation: none`, UV required, platform/roaming без принудительного attachment, синхронизируемые credentials разрешены. Программный ES256 fixture без attestation certificate проходит обычные registration/signature проверки; нулевые counters и изменения backup state проверены. Это не физический Touch ID.
- Проверены отказ без полного MFA, неверные подпись/origin/RP/challenge/UV/userHandle, истечение/replay и конкурентный verify (ровно одна сессия), текущий device-bound counter, login/logout CSRF, session expiry и восстановление. Logout/recovery блокируют hide, который прошёл HTTP-проверку, но ещё ждёт writer.
- Hide: atomic unknown UUID/audit rollback, идемпотентность, обычный WS с admin-cookie не получает чужой delete; live deletion видна двум клиентам, snapshot исключает скрытое, авторский create/retry не восстанавливает. Deferred COMMIT failure вызывает resync_required и допускает безопасный retry после свежего snapshot. Потеря TCP-ответа после фактически успешного COMMIT отдельно не инжектировалась.
- Native ARM64 Docker build/smoke и 11 Linux deployment-тестов прошли: CLI присутствует и не работает без TTY, admin выключен по умолчанию, probes/static/non-root/read-only и backup проверены. AMD64 образ должен пройти тот же gate в Actions. Smoke использовал имеющуюся локальную frontend-сборку, не новую админку.
- Frontend: следующий этап — отдельный entry, typed API client, buffer/base64url adapters, enrollment/login и selection/confirmation; браузерная интеграция пока не реализована. Криптографические fixtures находятся в `tests/admin.rs` и `tests/support/passkey.rs`; примеры wire-форм выше содержат placeholders, это не реальные credentials.
- Реальные Touch ID/iCloud, независимый резервный доступ и cross-browser enrollment/login на HTTPS origin пока не проверены. Сервер проверяет UV, не биометрию; ОС вправе предложить другую системную проверку. [Provisioning runbook](admin-operations.md).
- `cargo audit` **не чистый**: три транзитивные записи, применимость и дальнейшие действия в [security-dependencies.md](security-dependencies.md). Не объявлять публичную security-готовность по успешным функциональным тестам. TLS, сетевой gate и внешние backup/restore остаются обязательными.
