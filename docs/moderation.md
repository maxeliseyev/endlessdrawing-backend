# Модерация: план реализации backend и контракт для согласования

## Статус

Основание — документ владельца в соседнем frontend-репозитории: `../endless-drawing-frontend/docs/moderation.md` (из этого `docs/` — `../../endless-drawing-frontend/docs/moderation.md`). Продуктовые требования и handoff синхронизируются с frontend-сессией; wire-формы определяются только в [Admin API](admin-api.md).

**Backend API реализуется и тестируется локально; готовой публичной админки пока нет.** Актуальные wire-формы и статус проверок — в [Admin API](admin-api.md). Владелец выбрал обычные passkeys через Touch ID: аппаратная аттестация и CA trust store не требуются, platform/synced credentials разрешены, UV required и пароль остаются обязательными. Публичный запуск до его приёмки запрещён; CI/CD пока выкладывает только закрытый SSH-стенд.

Исходный MVP допускал служебный секрет/команду, но требование владельца уточнено: нужен браузерный `/admin/`, **логин + сложный пароль + обязательный WebAuthn/FIDO2**, основной и запасной ключ. Однофакторный временный вход, shared secret в JS, `?admin=1`, WS `admin:true` и password-only fallback не делать.

## 1. Подтверждённые инварианты

- Публичный `/` — только общий холст, без входа/ссылок/скрытых DOM-элементов/shortcut администратора.
- Admin UI — отдельный frontend entry, не импортируемый публичной сборкой; EN/RU, noindex/nofollow, no-store. Доступность URL или JS сама по себе не даёт прав.
- Только backend проверяет пароль, WebAuthn assertion, срок/отзыв сессии и каждое административное действие.
- Cookie host-only: `__Host-ed-admin; HttpOnly; Secure; SameSite=Strict; Path=/`, без Domain. Никаких admin-токенов в Storage/URL/VITE/WS hello.
- Обычный `/ws` не получает административных прав даже при наличии admin-cookie: `own`/`stroke.delete` продолжают работать только по секрету рисования.
- Администратор скрывает выбранные целые штрихи, не «рисует белым». Нужны выделение, список/число UUID, причина, явное подтверждение; массовый bbox без preview недопустим.
- Скрытие, аудит и результат идемпотентной операции коммитятся атомарно. Только после commit — обычные публичные `stroke.deleted` без причин и идентификатора модератора.
- Undo/create старого UUID не восстанавливают скрытые объекты. Snapshot/reconnect не возвращают их.
- Решение о незаконности принимает уполномоченный человек по утверждённой политике; автоматического списка запрещённых мнений или обещания юридического соответствия нет.

## 2. Предлагаемая архитектура

Для просмотра того же публичного мира admin entry может использовать обычный WS v1: read snapshot не требует новых административных полномочий. В служебном UI активен отдельный режим выбора, не публичного рисования. Скрытие идёт только через аутентифицированный HTTP admin API; cookie не расширяет права WS.

### Таблицы (новые миграции, не редактировать уже применённую 0001)

| Таблица | Содержимое |
| --- | --- |
| `admin_accounts` | UUID, нормализованный login, Argon2id hash, disabled/auth-version, timestamps |
| `admin_credentials` | Account ID, credential ID, сериализованный поддерживаемой библиотекой WebAuthn credential, имя ключа, revoked_at |
| `admin_login_attempts` | Digest pre-auth cookie, одноразовое challenge state, account/purpose, expiry/consumed_at, CSRF binding |
| `admin_sessions` | Digest случайного cookie token, account/auth-version, CSRF digest, idle/absolute expiry, revoked_at |
| `moderation_operations` | Operation UUID, actor, нормализованный payload hash, reason, timestamps и сохранённый результат |
| `moderation_operation_strokes` | Operation UUID, stroke UUID, результат `hidden` / `already_hidden`; без копии геометрии |
| `admin_security_audit` | События provisioning/recovery/отзыва и входа без паролей/challenge/assertion/cookie |

Сессионные и bootstrap bearer tokens в БД — только digest. WebAuthn challenge state/публичные credentials хранить по формату выбранной библиотеки и не отдавать в публичные ответы. Signature counters и доступные признаки backup/UV обновлять по правилам библиотеки, не из собственного «упрощённого» валидатора.

### Пароль и FIDO2

- Пароль сгенерирован менеджером паролей, минимум 20 символов, вставка/autofill разрешены. Argon2id с индивидуальной солью; начальный ориентир 64 MiB / 3 итерации / parallelism 1, затем калибровка на VPS и нагрузочный тест. Ограничить максимальный размер password input и число параллельных hash-проверок.
- Использовать поддерживаемую серверную библиотеку, например `webauthn-rs`, с проверкой её актуального API/модели хранения перед реализацией. Не писать проверку подписей/COSE/origin самостоятельно.
- Production RP ID: `endlessdrawing.com`, точный origin `https://endlessdrawing.com`; localhost — отдельный RP/config и тестовые credentials. Нельзя зарегистрировать ключ только на localhost и обещать, что эта регистрация заработает в production.
- Challenge криптографически случайный, одноразовый, с account/purpose/origin binding. После успешной проверки атомарно пометить consumed; конкурентный replay не создаёт второй сеанс.
- Проверять ожидаемые RP ID/origin, подпись, challenge, user presence, `userVerification: required` и применимые flags/counters.
- Пароль выдаёт только pending-MFA состояние, **не** `admin_sessions`. Отмена/ошибка/timeout WebAuthn не имеют password-only обхода.
- Основной и резервный passkeys регистрируются отдельно. По решению владельца разрешены Touch ID/platform и синхронизируемые passkeys; hardware-only attestation policy не используется. Один iCloud credential на Mac/iPhone не считать двумя независимыми регистрациями. Сервер проверяет UV и подпись, не получает биометрию и не требует исключительно отпечаток.

### Предлагаемые сроки и ограничения — согласовать

| Параметр | Начальное предложение |
| --- | --- |
| WebAuthn challenge TTL | 120 секунд |
| Pending/pre-auth cookie | 5 минут, только для login flow, без прав модерации |
| Admin idle expiry | 15 минут |
| Admin absolute expiry | 8 часов |
| CSRF и сессионный token | По 256 случайных бит, ротация при полном входе |
| Login attempts | Ограничения по IP и account, общая ограниченная очередь Argon2; значения подобрать тестом |
| Hide batch | 1–100 уникальных UUID |
| Reason | Обязательная непустая строка, до 1000 символов/4000 UTF-8 байт |
| Admin JSON body | Начальный максимум 64 KiB |

Logout, смена пароля, disabled-account и отзыв факторов немедленно отзывают соответствующие серверные сеансы. Idle/absolute expiry проверяются на каждом запросе, cookie Max-Age не является единственной проверкой. `401` в UI снимает права, не повторяет скрытие автоматически после нового входа.

## 3. Login/CSRF и API: проект контракта

Нужна отдельная pre-auth CSRF-инициализация: после logout или до первого входа ещё нет полной admin-сессии. SameSite-cookie и скрытый URL не решают login CSRF сами по себе.

| Endpoint | Предложение |
| --- | --- |
| `GET /admin/` | Отдельный admin entry с формой, no-store и noindex |
| `GET /api/admin/login/options` | Короткоживущая pre-auth cookie и CSRF token только для формы входа |
| `POST /api/admin/login` | Login/password + Origin + pre-auth CSRF; при успехе только pending-cookie и WebAuthn request options |
| `POST /api/admin/login/verify` | Assertion + Origin + pending CSRF; при успехе consume challenge, rotate/create полноценную cookie-сессию |
| `GET /api/admin/session` | Только полная сессия: минимальные account/expiry сведения и CSRF token; иначе 401 |
| `POST /api/admin/logout` | Origin + CSRF, серверный отзыв и истечение cookie |
| `POST /api/admin/strokes/hide` | Полная сессия + Origin + `X-CSRF-Token`, атомарное скрытие batch |
| `GET /api/admin/operations/{operation_id}` | Закрытая проверка результата после потери ответа |

Имена login/options и operation-status endpoint — дополнение к frontend-предложению, согласовать до реализации UI/fixtures. WebAuthn payload передавать в проверенном сериализуемом формате выбранной библиотеки; не изобретать несовместимую base64-перекодировку ArrayBuffer-полей.

Все admin/API ответы и входная страница: `Cache-Control: no-store`, без кросс-origin credential CORS. Проверять Origin на всех изменяющих запросах, включая login/verify/logout; session CSRF обязателен. Для admin entry — `frame-ancestors 'none'`, строгая CSP без inline script/eval, `Referrer-Policy: no-referrer`, nosniff. Защита от XSS важна: HttpOnly не мешает вредоносному same-origin JS действовать от имени вошедшего администратора.

Ошибки login нейтральные, без подтверждения наличия login или подробных криптографических причин. Внутренняя диагностика — только безопасные типы/коды, без исходного body, assertion, секретов или cookie.

## 4. Hide: транзакция и конкурентность

Предлагаемый запрос:

```json
{
  "operation_id": "4df8a775-7e7e-4707-a747-58d6c87d7019",
  "stroke_ids": ["87a122fa-7537-4b88-9b77-b45d24a29714"],
  "reason": "Reason selected by the authorized moderator"
}
```

Предлагаемый ответ, только после commit:

```json
{
  "operation_id": "4df8a775-7e7e-4707-a747-58d6c87d7019",
  "hidden": ["87a122fa-7537-4b88-9b77-b45d24a29714"],
  "already_hidden": []
}
```

Предлагаемая семантика **атомарная**, без частичного скрытия при неизвестном/невалидном UUID:

1. Проверить полную сессию/Origin/CSRF, лимиты и payload. Нормализовать reason/UUID, определить стабильный payload hash.
2. Поставить `AdminHide` в существующий единственный `realtime` actor. Не делать прямой UPDATE из HTTP handler/CLI.
3. В actor/транзакции повторно проверить актуальную сессию/account expiry/отзыв, чтобы ожидание очереди не продлевало права. Согласовать блокировки account/session с logout/recovery: уже начатое действие может завершиться до отзыва, но после успешного ответа logout новая операция не проходит. Порядок блокировок единый, без гонки «проверили до logout, записали после него».
4. Для уже исполненного operation_id того же actor/payload вернуть сохранённый результат; при другом actor/payload — конфликт, не повторять действия.
5. Загрузить все строки, включая tombstones. Если хотя бы одной нет — откатить весь batch. Уже удалённые строки — `already_hidden`, без нового публичного события.
6. Поставить `deleted_at` только активным строкам, сохранить operation/result/audit в той же транзакции. При ошибке аудита скрытие не коммитится.
7. После commit опубликовать по каждому реально скрытому объекту обычный `stroke.deleted` с исходными id/seq/bounds, например в порядке seq. Snapshot barrier/broadcast ring уже обеспечивают правильную границу; при overflow клиент получает resync.
8. Потеря HTTP-ответа не меняет идемпотентность: клиент проверяет operation или повторяет тот же operation_id/payload, не генерирует новое действие вслепую.

Public delete и AdminHide сериализованы тем же actor. Если автор удалил первым, модератор получает `already_hidden`; если первым скрыл модератор, повторный public delete не воскрешает объект. Пользовательская авторизация не ослабляется, исходный сессионный секрет не раскрывается модератору.

Отклонённые попытки и auth failures аудировать отдельно безопасными кодами; они не могут использовать успешно закоммиченный moderation operation как доказательство состоявшегося скрытия.

## 5. Provisioning и восстановление: только закрытый процесс

Нельзя выдавать регистрацию нового FIDO2 credential после одного правильного пароля.

Предлагается отдельная операционная CLI через доверенный SSH:

1. Создать account и password hash без пароля в argv/логах; передавать пароль защищённым prompt/stdin.
2. Выдать краткоживущее одноразовое enrollment разрешение, account/purpose-bound, с digest в БД и аудитом. Оно разрешает только регистрацию ключей, не выдаёт сеанс модератора. Передача в HTTPS body из формы, не URL/Storage.
3. На production origin под закрытым сетевым доступом выполнить WebAuthn регистрации основного/запасного ключа. Конкретный закрытый enrollment flow и TTL согласовать отдельно; staging-gate не заменяет прикладной MFA.
4. Разрешить normal login только после завершения политики регистрации. Проверить вход с обоими ключами и отказ без второго фактора.
5. Потеря всех ключей: закрытая проверка личности владельца, SSH recovery с отзывом старых credentials/сессий и новым enrollment. Нет универсального recovery password или скрытого API обхода MFA.

SSH private key не загружается в браузер. Ключ CI-деплоя не является WebAuthn credential. Защита GitHub/deployment доступа критична: тот, кто может заменить серверный код, может обойти прикладную авторизацию; защитить ветки, environment и операторские ключи отдельно.

## 6. Приёмка следующего этапа

- Пароль без FIDO2, просроченный/replayed challenge, неверный origin/RP ID/подпись/UV и отмена диалога не создают full session.
- Проверены оба ключа, закрытое enrollment/recovery и отзыв сеансов; auth rate limits не дают исчерпать 8 ГБ RAM параллельными Argon2 hash.
- CSRF/login-CSRF/logout-CSRF, framing, session expiry/revocation и прямой неавторизованный API отклоняются.
- Публичный WS с admin-cookie всё равно не удаляет чужое; payload/Storage/UI не повышают права.
- Batch скрывает только выбранные штрихи, атомарен при неизвестном UUID, safely retries operation_id, сохраняет аудит и не публикует события до commit.
- Скрытие во время snapshot и reconnect не теряется; два независимых публичных клиента видят удаление, соседние штрихи сохраняются.
- Авторские undo/retry/create старого UUID не восстанавливают скрытое. Повтор операции не создаёт ложных новых скрытий.
- Нет admin entry в публичных imports и ссылках; EN/RU UI честно обрабатывает pending/failure/401 и не повторяет hide после нового входа.
- Runtime credentials, password, cookies и assertions не попадают в artifacts/JS/логи/GitHub secrets frontend; публичные события не содержат причин и аудитора.
- Владелец утвердил политику authenticator/passkey, сроки сеансов/challenges, юрисдикцию и retention геометрии/tombstones/аудита/внешних backup. Soft delete не равен физическому удалению запрещённого материала.

Перед публичным включением нужно проверить настоящие Touch ID/passkeys, отдельный резервный доступ и итоговый HTTPS production origin. Допуск platform/synced passkeys уже выбран владельцем; пароль, UV и одноразовые challenges не ослабляются.
