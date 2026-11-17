# Google Authenticator: резерв вместо второго passkey

## Подтверждённая выкладка

[Actions 44074630461](https://github.com/maxeliseyev/endlessdrawing-backend/actions/runs/44074630461) success: backend `87fa802849fbdc2defcab3033009c0624edce459`, frontend `b631892eb1667b9c489ec78100c8a7e50b874b28`. Migrations 1/2/3/4 applied, image labels checked; существующий единственный passkey сохранён. Позже владелец включил TOTP через fresh-UV setup (17.11.2026, audit `totp_enabled`), primary login/secret/QR агент не захватывал. Native trusted-HTTPS browser проверил новый вход/подсказки/backup button без ввода реальных credentials. Private KEK read-only mount проверен, raw key не печатался.

72 backend tests (20 admin), fmt/Clippy, 38 deployment/backup tests, 127 frontend unit, 190 browser tests / 2 CDP skips, real HTTPS UV + QR + TOTP login integration passed. Аутентификаторы и TOTP seeds в автотестах только эфемерные fixtures. Dependency audit по-прежнему имеет прежние 3 findings / 2 advisories, новых игноров нет.

Post-deploy off-site snapshot `20261117T120450Z-993e4835` скачан, age-decrypted на Mac и восстановлен в isolated PostgreSQL: migrations 1/2/3/4 и **тот же 32-byte KEK** восстановлены/проверены по checksums. Plaintext/container удалены; nonsensitive proof на VPS/у оператора, private age identity не переносился.

Решение владельца 17.11.2026: обычный вход **логин + пароль + один UV passkey / Touch ID**, резервный **логин + пароль + TOTP**. Второй passkey больше не обязателен. Password-only входа нет. TOTP фишингу подвержен сильнее, чем WebAuthn; это осознанный альтернативный резерв, а не ещё один шаг после Touch ID. Не открывать публичный gate автоматически.

## API (расширение admin-api.md)

Все endpoints наследуют HTTPS exact Origin, no-store, SameSite/HttpOnly/Secure cookies, CSRF и limits. Session response дополнительно содержит `auth_method: "passkey_uv" | "totp"`. Primary `authenticator_policy: "passkey_uv"` в options обозначает WebAuthn UV policy, а не отсутствие TOTP-резерва.

- `POST /api/admin/login/totp` `{login,password,code}`: сначала GET login/options и его cookie/CSRF, затем пароль и ровно 6 ASCII digits. Успех — обычный полный SessionReply и новый opaque session cookie. Нет enrollment через резервный код; backup должен быть ранее подтверждён. В БД должен оставаться хотя бы один active passkey. Password rate limits общие с основным входом.
- `GET /api/admin/totp/status`: требует session, возвращает `{available,enabled,fresh_passkey}`. Никаких секретов.
- `POST /api/admin/totp/setup/start` `{}`: session+CSRF, **primary passkey session не старше 5 минут**, не TOTP session. Возвращает `{secret,otpauth_uri,expires_at}` ровно для одного локально генерируемого QR. Secret только в RAM/DOM формы, без Storage, URL страницы, clipboard automation, analytics, logs или внешних QR services. UI QR генерирует qrcode локально.
- `POST /api/admin/totp/setup/confirm` `{code}`: та же fresh UV session, совпадающий pending_session_digest, deadline 5 минут, верный код. Только после этого backup active. Confirm code сразу считается использованным.
- `POST /api/admin/totp/disable` `{}`: только fresh primary UV session+CSRF. UI требует явное подтверждение. Для замены отключить и подключить заново; active secret никогда не выдаётся повторно.

После включения/отключения backup повышается auth_version, отзываются **все другие** sessions, pending ceremonies и старые enrollment grants; текущая fresh primary session сохраняется. TOTP-session никогда не может включать/менять/отключать backup. Когда fresh период закончился, войти снова с паролем+Touch ID. CLI recover/disable удаляют также active/pending TOTP.

## Криптография и replay

RFC 6238 совместимость Google Authenticator: SHA1-HMAC, 20 random bytes, Base32 без padding, 6 digits, 30 seconds, +/- 1 step. SHA1 здесь HMAC-TOTP, не подписи/коллизионная проверка. Коды сравниваются constant-time. Успех под account/row lock атомарно сохраняет highest-used counter вместе с cookie session и audit; повтор/конкурентный повтор одного кода отклоняется, включая после рестарта. Неуспешный COMMIT не даёт половинчатого результата.

AES-256-GCM envelope с random 96-bit nonce и AAD account UUID + exact origin, encrypted active и pending secrets в `admin_totp`; raw QR secret не сохраняется в БД. KEK raw 32 bytes, private file вне БД, не env value и не source. Startup fails closed при missing/wrong key, если есть encrypted rows. Secrets/keys не Debug/не логируются; key/temporary buffers и cipher zeroize.

## VPS / контейнер / восстановление

`deploy/prepare-totp-key.sh` — явная root preparation под deploy lock: создаёт `/opt/endlessdrawing/admin-secrets/totp.key` только один раз, `10001:10001 0400`, dir `0500` под закрытым root-owned app directory. Marker предотвращает тихую замену потерянного key. Docker non-root backend получает **только** этот secrets directory через read-only bind `/run/admin-secrets`; caddy/runtime credentials туда не копируются. Runtime env содержит только `ADMIN_SECRETS_DIR` и `ADMIN_TOTP_KEY_FILE` paths. Операторский age identity остаётся на Mac, это другой ключ.

Перед новой Compose activation установить/проверить secret mount source; при admin-disabled тестах использовать пустой fixture directory. Не ослаблять `/etc/endlessdrawing` 0700 и не выводить KEK в терминал/чат. Обычный bootstrap не генерирует приватный key автоматически.

Backup format **2** добавляет `totp-key.bin` **внутрь age envelope**, вместе с DB/manifest/SHA256SUMS. Format 1 без TOTP key остаётся читаемым. В Box отправляется только ciphertext, не raw KEK; PostgreSQL dump без key не раскрывает TOTP seeds. Operator verifier принимает только точный набор 3/4 regular entries, проверяет key flag/length/checksum и удаляет plaintext после isolated restore. При disaster recovery восстановить **именно соответствующий KEK** из зашифрованного snapshot, не создавать новый; затем закрыто отозвать sessions/ceremonies, очистить pending TOTP setup и поднять last_used_step минимум до текущего DB step + 1 перед открытием приложения (первые 30–60 секунд резервные коды намеренно не принимаются). Это предотвращает воскресший после restore replay-counter. Проверить key/counter/auth_version; primary passkey counters тоже могут требовать recovery по ситуации. Обладание age private identity даёт доступ ко всему восстановленному архиву; сохранить его вне VPS и отдельно в password manager.

## Ручная приёмка владельца

После выкладки войти существующим login/password + ранее зарегистрированным Touch ID (не повторять create/recover). В администрировании открыть «Резерв Google Authenticator» → «Подключить», отсканировать QR телефоном, подтвердить 6-digit code. QR/secret/password/коды не отправлять агенту или в переписку. Дождаться нового кода, выйти и проверить явный резервный login. Потом проверить обычный Touch ID ещё раз. Только пользователь подтверждает физические устройства; виртуальные тесты их не заменяют.
