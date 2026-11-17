# Gates публичного запуска

Цель — настоящий общий сохраняемый холст с модерацией и восстановлением, а не только доступный HTML. Подготовка отдельных компонентов не является разрешением открыть приложение всему Интернету.

## Подтверждено на VPS

- [x] Закрытый combined release через [Actions run 43971072004](https://github.com/maxeliseyev/endlessdrawing-backend/actions/runs/43971072004).
- [x] Backend `13f3ab1260f42b3cb934ff9c1f9a6f3af14fec38`, frontend `623c5a6b3bbbf4cfc549f1329af0aa64c9fc3771` подтверждены по image labels после выкладки.
- [x] Readiness 200, admin HTML 200; admin API 503 = намеренно disabled, не успешная авторизация.
- [x] App только `127.0.0.1:8080`, БД без public port; SSH сохранён. Caddy пока не установлен, 80/443 на сервере не слушают.
- [x] Для Storage Box создан отдельный SSH key в `/etc/endlessdrawing/backup/id_ed25519`, без вывода приватного ключа. Это не age identity. Никаких изменений каталога/cron/credentials construct не делалось.

## Реализовано и проверено (активация указана отдельно)

- [x] Общий Chromium HTTPS frontend/WebAuthn/API/WS/PostgreSQL тест с виртуальными UV credentials, без HTTP/WS mocks. Настоящий Touch ID он не доказывает.
- [x] [Private Caddy configuration](https.md), deny-by-default, тест spoofed forwarded headers не даёт доступ.
- [x] [Backup tools](backups.md): custom pg_dump, отдельная restore DB, age public-key encryption, SSH host pin, dedicated subaccount, verified upload, atomic publish, independent weekly SHA и constrained rotation.
- [x] 35 deployment/backup mock safety tests. Настоящий локальный dump → restore → age encrypt/decrypt → independent readback drill прошёл на PostgreSQL 17; используется одноразовый test identity, не операторский private key.
- [x] Backup helpers/units установлены на VPS, настоящий local-only dump/restore/encrypt прошёл в systemd sandbox; peak 32 MiB. Рабочая БД не восстанавливалась, off-site success не записан.
- [x] Daily timer включён после настоящего operator restore: 04:10 UTC + до 10 минут, retention 7 daily / 4 weekly. Следующее срабатывание при проверке — 18.11.2026 04:13:28 UTC; health helper подтверждает recent off-site success.

## Обязательно закрыть до снятия private gate

- [x] Отдельный subaccount `u651188-sub1` создан и принимает ключ Endless Drawing по SSH 23; master account construct не используется.
- [ ] Финально сверить в панели Hetzner, что реальный root subaccount — **новая** папка `endlessdrawing`, не `construct` и не весь Box. SSH metadata/readback не доказывают физический chroot path.
- [x] Host key `u651188-sub1.your-storagebox.de:23` сверён с существующим доверенным parent Box в операторском known_hosts; ED25519 совпал, отдельный pin установлен. Root-only backup.env настроен на `u651188-sub1`; hostname subaccount поддержан и проверен тестами.
- [x] 17.11.2026 новый SSH-ключ принят subaccount. Настоящие daily/weekly upload, проверки ciphertext SHA до/после публикации, systemd service (Result=success / exit 0) и download readback на операторскую машину прошли. Ciphertext readback SHA совпал с доверенным VPS state. Расписание включено после следующего operator restore-check.
- [x] Настоящий readback `20261117T075339Z-86a8a777` (SHA-256 `4b071bee5acf83677c374d539eda87c5f814d16080c0bcca53fd6935918abcc6`) расшифрован операторским identity на Mac и восстановлен в изолированный PostgreSQL 17 без сети и host mounts. Migrations 1/2 валидны; в этом старом deployed snapshot 0 strokes/accounts/moderation operations. Временный plaintext/container удалены; identity не читался в вывод и не переносился на VPS. Несекретный proof JSON сохранён у оператора и на VPS.
- [ ] Подтвердить отдельную копию private age identity в менеджере паролей; файл на Mac уже проверен. Выбрать/проверить Storage Box snapshots или другую защиту от удаления upload key.
- [ ] Подключить мониторинг квоты Box/диска/WAL и проверить доставку внешнего failure/stale-backup alert. Daily timer и recent off-site success уже проверены, но journal и health CLI сами никого не уведомляют.
- [ ] Подтвердить DNS origin/AAAA/CAA, выбрать DNS-only/private access или корректный Cloudflare Access вариант. Установить Caddy без default welcome window, получить доверенный cert; закрытый доступ остаётся.
- [ ] Включить admin API только на конечном HTTPS origin. Оператор закрыто создаёт аккаунт, регистрирует настоящие основной/резервный passkeys и проверяет Touch ID/login/logout/recovery/отмену/неверный пароль. Два iCloud-синхронизированных устройства не означают два credentials.
- [ ] Выбрать reviewed commit нового frontend с public WS / brushes; backend migration 0003 уже закоммичена отдельно (`1473251`). Текущий VPS release всё ещё frontend `623c5a6` с локальным drawing-прототипом; локальные integration tests не означают выкладку. Не терять работу параллельной frontend-сессии.
- [ ] Единый новый reviewed SHA pair прошёл backend CI, public/admin real-browser integration, owner-only erase/undo, все brush round-trip, live moderation у двух реальных UI clients, reconnect/reload и truthful pending/error UX. Опубликовать через Actions, не копировать грязный dist на VPS.
- [ ] Разделить maintenance/migration/backup DB role и runtime app role; приложение не должно ходить superuser. Согласовать embedded migrations с этим режимом до переключения.
- [ ] Trusted proxy/client-IP/rate/connection limits проверены; не превращать X-Forwarded-For в источник прав и не считать shared proxy IP отдельным посетителем.
- [ ] Dependency audit review по [security-dependencies.md](security-dependencies.md), server/SSH/GitHub hardening, monitoring, backup restore и достаточная ёмкость/профилирование сцены.
- [ ] Владелец утвердил правила модерации, юрисдикцию и retention tombstones/audit/backup: soft delete скрывает публично, но не удаляет физически данные из archive.
- [ ] Отдельное явное разрешение снять private gate. Rollback image не восстанавливает БД автоматически.

Не ставить галочку по одному успешному `readyz`, HTML `/admin/`, сертификату, mock test или сообщению `--local-only`.
