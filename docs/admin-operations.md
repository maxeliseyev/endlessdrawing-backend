# Администратор: passkeys / Touch ID и закрытый provisioning

Статус: backend и `/admin/` прошли [совместную HTTPS/API/WS/DB проверку](admin-integration.md), повторённую с frontend `5cc6de7`, с виртуальными Chromium UV аутентификаторами. Production HTTPS уже доверенный, Caddy допускает только явно подтверждённый operator `/32` и loopback. Владелец изменил backup policy: **один UV passkey для основного входа, пароль+Google Authenticator как альтернативный резерв**. Уже зарегистрированный passkey не сбрасывать; два credentials не обязательны. Подключение TOTP и проверка обоих входов — закрытая приёмка владельца, [новый runbook](admin-totp.md). Актуальный релиз/API status — в [publication checklist](publication.md). Wire-контракт: [admin-api.md](admin-api.md).

## До включения

1. В релиз включить целый новый frontend dist с отдельным `/admin/` entry (реализован в frontend `623c5a6`): enrollment, вход, выбор штрихов и подтверждение скрытия. На публичном холсте никаких admin imports/ссылок/shortcut.
2. Настроить доверенный браузером HTTPS/WSS на окончательном origin, сохранив закрытый доступ к стенду. Обычного HTTP SSH-туннеля `http://localhost:8080` для этого сценария недостаточно: Secure cookies и политика admin требуют HTTPS. Не ослаблять MFA/Origin/cookie ради стенда.
3. В серверном `/etc/endlessdrawing/runtime.env` настроить только несекретные параметры:
   ```dotenv
   ADMIN_ENABLED=true
   ADMIN_ORIGIN=https://endlessdrawing.com
   ADMIN_RP_ID=endlessdrawing.com
   ALLOWED_ORIGINS=https://endlessdrawing.com
   ```
   Origin — без завершающего `/`, пути, query или fragment. Отдельные localhost/staging RP требуют отдельных регистраций; они не переносятся автоматически на production RP.
4. Установить актуальный root-owned `deploy/compose.yml` штатным bootstrap: старый файл с VPS не передаёт новые ADMIN_* автоматически при одном image deploy. Не перезаписывать существующие DB credentials. Перевыкладку делать через ручной Actions с backup/readiness, не прямой сборкой на VPS.
5. Проверить [замечания аудита зависимостей](security-dependencies.md). Admin API по умолчанию выключен; HTML entry доступен для проверки и показывает отключённое администрирование. Это не даёт сессии или enrollment. CA-файла/сертификата аутентификатора больше не требуется. Не помещать пароль, enrollment token или приватные ключи в env/VITE/Actions/URL.

## Создание аккаунта

Выполняет владелец в своём доверенном интерактивном терминале. Агент/CI не должен захватывать вывод enrollment capability. Сначала новый образ сервера должен применить миграции и пройти readiness.

```bash
ssh -t -i ~/.ssh/stokholm subwhale@212.147.244.161
sudo -i
cd /opt/endlessdrawing
IFS= read -r ENDLESSDRAWING_IMAGE < current-image
export ENDLESSDRAWING_IMAGE
# Без -T, tee, перенаправления, screen recording и записи терминала:
docker compose --env-file /etc/endlessdrawing/runtime.env \
  -f /opt/endlessdrawing/compose.yml exec backend /app/canvas-admin create moderator
```

CLI дважды запросит пароль без echo: сгенерированный менеджером, минимум 20 символов, максимум 1024 UTF-8 bytes. Пароль не передавать в argv или shell history. Выданный enrollment token — секретная возможность регистрации, не сессия модератора; действует 15 минут и на две успешные регистрации. Не отправлять его в переписку/логи.

На том же HTTPS origin вручную открыть `/admin/`, выбрать enrollment, ввести login/password/token и label, зарегистрировать **один основной** passkey. Если он уже сохранён на сервере, не повторять create/recover: перейти к обычному входу. Браузер/провайдер создаёт ключи; `ssh-keygen` и загрузка SSH private key не нужны. Сайт хранит credential ID и публичный ключ. Touch ID/iCloud разрешены; сервер проверяет UV, а не получает биометрию и не гарантирует исключительно отпечаток.

Второй passkey необязателен. Владелец выбрал Google Authenticator вместо него: после password+Touch ID login открыть резервную настройку, отсканировать QR локально и подтвердить код. Через резервный password+TOTP login менять факторы нельзя; для этого нужен свежий основной вход не старше 5 минут. QR/secret/code не передавать агенту.

`complete:true` не авторизует модерацию. Перейти к новому обычному входу: login/password → WebAuthn get → server session. Проверить вход с каждым credential. После регистрации очистить enrollment token и пароль из памяти формы.

## Восстановление / отключение

В том же доверенном SSH/root терминале и с тем же Compose/image окружением:

```bash
docker compose --env-file /etc/endlessdrawing/runtime.env \
  -f /opt/endlessdrawing/compose.yml exec backend /app/canvas-admin recover moderator
# Либо немедленно отключить аккаунт:
docker compose --env-file /etc/endlessdrawing/runtime.env \
  -f /opt/endlessdrawing/compose.yml exec backend /app/canvas-admin disable moderator
```

CLI потребует явное подтверждение. Recovery — только после проверки личности владельца: новый пароль, отзыв всех прежних credentials, sessions, TOTP и enrollment/ceremonies, затем новый primary passkey и повторная настройка TOTP. Disable отзывает sessions/enrollment и блокирует аккаунт; recovery может его включить обратно. Нет email/password-only/универсального recovery-secret обхода. Если CLI завершился с неопределённой ошибкой commit, не считать token выданным или account несуществующим: закрыто проверить состояние и при необходимости выполнить recovery.

Logout отзывает текущую сессию, а не все устройства. Idle — 15 минут, absolute — 8 часов. Ожидающее в writer скрытие перепроверяет сессию внутри транзакции: после успешного logout/recovery оно не получает старые права.

## Перед публичным запуском

- Настоящие Touch ID/passkeys, резервный вход, отмена диалога, неверный пароль, logout/recovery и cross-browser проверены на выбранном HTTPS origin.
- Скрытие видно двум независимым публичным клиентам без reload; новый snapshot не возвращает скрытое; авторские retry/undo не восстанавливают его.
- После потери ответа используется тот же operation_id/payload либо GET operation. 503 не доказывает rollback. При неизвестном исходе commit backend инвалидирует подписки через resync_required; frontend должен запросить свежий snapshot.
- Root/SSH/GitHub deployment доступ защищён отдельно: возможность заменить серверный код сильнее прикладного MFA.
- На reverse proxy настроены ограничения по настоящему peer IP. Backend намеренно не доверяет произвольному X-Forwarded-For: без доверенной proxy-настройки его IP-лимиты могут быть общими для всех посетителей за прокси.
- TLS, сетевой gate, DB roles, внешние зашифрованные backup/restore и юридическая/retention политика приняты отдельно. Успешные локальные тесты не снимают эти блокировки публичного запуска.
