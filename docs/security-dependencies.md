# Проверка зависимостей перед публичным запуском

`cargo audit` выполнен с обновлённой RustSec DB. **Чистого результата пока нет:** после исключения устаревшей тестовой crypto-зависимости остаются три записи по двум advisories. Игноры не добавлены; этот документ не является принятием риска владельцем и не заменяет security review.

| Dependency / advisory | Наблюдение |
| --- | --- |
| `libsqlite3-sys 0.30.1`, RUSTSEC-2026-0459 | Optional SQLite graph в Cargo.lock через SQLx 0.8.6. SQLite не используется: `cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i libsqlite3-sys` не находит активного пути. |
| `rsa 0.9.10`, RUSTSEC-2023-0071 | Optional MySQL graph SQLx в Cargo.lock. MySQL отключён; та же проверка `cargo tree ... -i rsa@0.9.10` не находит активного пути. |
| `rsa 0.10.0-rc.19`, RUSTSEC-2023-0071 | Активная транзитивная зависимость `webauthn-rs-core → crypto-glue`. Advisory Marvin касается извлечения приватного RSA ключа по времени приватных операций. Backend проверяет подписи публичными ключами и не хранит RSA private keys/не подписывает и не расшифровывает ими. Это оценка применимости, а не утверждение, что библиотека исправлена. |

Программный ES256-аутентификатор в `tests/support/passkey.rs` использует тот же актуальный `crypto-glue`, только как dev-dependency. Старый прямой тестовый `p256 0.13` и его `crypto-bigint 0.5` удалены. Приватные тестовые ключи эфемерны, не используются сервером для bypass и не сохраняются/логируются.

Попытка `cargo update -p rsa@0.10.0-rc.19` не дала более новой совместимой версии в доступном registry. Не патчить криптографию самостоятельно и не подавлять advisory глобальным ignore. Перед публичным запуском повторить проверку, оценить совместимое обновление WebAuthn/crypto-glue и SQLx либо оформить отдельное явное решение по неприменимым путям. Не включать SQLite/MySQL, RSA signing/decryption или другие новые crypto-функции без пересмотра оценки.

Повторение:

```bash
cargo audit
cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i libsqlite3-sys
cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i rsa@0.9.10
cargo tree --locked --target x86_64-unknown-linux-gnu -e normal,build -i rsa@0.10.0-rc.19
```

Не выдавать 63 успешных backend-теста, виртуальную browser integration или container smoke за успешный dependency audit, настоящий Touch ID E2E либо разрешение публичного запуска.
