# Проверка зависимостей перед публичным запуском

`cargo audit` (RustSec DB) выполнен 17.11.2026. Результат: **три vulnerability-записи и один unmaintained warning**. Игноры не добавлены; каждый пункт ниже — отдельная оценка применимости. Владелец явно принял остаточный риск (см. [publication](publication.md)).

| Dependency / advisory | Оценка |
| --- | --- |
| `libsqlite3-sys 0.30.1`, RUSTSEC-2026-0459 | **Не компилируется.** Optional SQLx SQLite graph в `Cargo.lock`; `cargo tree --locked -e normal,build -i libsqlite3-sys` не находит активного пути. SQLite/MySQL features отключены (`sqlx default-features = false`, только `postgres`). CI guard запрещает их активацию. |
| `rsa 0.9.10`, RUSTSEC-2023-0071 | **Не компилируется.** Optional SQLx MySQL graph; `cargo tree ... -i rsa@0.9.10` пуст. |
| `rsa 0.10.0-rc.19`, RUSTSEC-2023-0071 | **Активна**, транзитивно `webauthn-rs-core → crypto-glue 0.2.2`. Обновление до `rsa 0.10.0` невозможно: `crypto-glue 0.2.2` требует feature `sha2`, удалённый в `rsa 0.10.0` stable; новее crypto-glue/webauthn-rs нет (0.2.2 / 0.6.2 — последние). Advisory Marvin касается приватных RSA-операций (извлечение ключа по времени); backend проверяет подписи публичными ключами и не хранит/не использует RSA private keys, не подписывает и не расшифровывает ими. Это оценка применимости, а не утверждение, что библиотека исправлена. |
| `chrono 0.4.45`, RUSTSEC-2026-0543 | **Unmaintained warning** (не vulnerability). Транзитивно через `sqlx` (feature `chrono`) и `serde_with` (webauthn-rs-proto), плюс прямой `chrono` в admin. Замена прямой зависимости на `time` не убирает транзитивный chrono. Риск — отсутствие будущих патчей, не текущая уязвимость. |

Программный ES256-аутентификатор в `tests/support/passkey.rs` использует тот же актуальный `crypto-glue`, только как dev-dependency. Приватные тестовые ключи эфемерны, не используются сервером для bypass и не сохраняются/логируются.

Попытка `cargo update -p rsa@0.10.0-rc.19 --precise 0.10.0` отклонена resolver'ом: `crypto-glue` требует feature `sha2`, отсутствующую в `rsa 0.10.0`. Не патчить криптографию самостоятельно и не подавлять advisory глобальным ignore.

Защита от случайной активации неиспользуемых графов: CI-шаг проверяет, что `libsqlite3-sys` и `rsa@0.9.10` не имеют активного normal+build пути; включение SQLite/MySQL, RSA signing/decryption или новых crypto-функций требует пересмотра этой оценки.

Повторение:

```bash
cargo audit
cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i libsqlite3-sys
cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i rsa@0.9.10
cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i rsa@0.10.0-rc.19
```

Не выдавать 78 успешных backend-тестов, виртуальную browser integration или container smoke за успешный dependency audit, настоящий Touch ID E2E либо разрешение публичного запуска.
