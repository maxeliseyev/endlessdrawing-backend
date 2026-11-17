# Telegram: private monitoring / owner-only setup

## Граница и статус

Подготовлен `deploy/monitor.py`, systemd service/timer и backup OnFailure hook. **Реальный Telegram token агенту не передавать.** Пока владелец не прошёл configure в своём TTY и не подтвердил тестовое сообщение, delivery/monitoring gate не считается пройденным.

**VPS acceptance 17.11.2026 13:39 UTC:** владелец завершил скрытый TTY configure и подтвердил получение теста. Проверены config metadata (`root:root 0600`, без symlink), timer enabled, service Result=success / ExecMainStatus=0, `issues: []`, daily-green отправлен. Дополнительный `drill` принят Telegram: две явные TEST-симуляции failure/recovery, без изменения сервисов и notification state. Token не передавался агенту и не читался в вывод. Это подтверждение Telegram, не независимый outage monitor.

Это VPS-local watchdog, не независимая проверка полностью выключенного/отрезанного VPS. При падении хоста, сети или самого watchdog сообщения отправить некому. Внешний dead-man/uplink monitor на другом хосте или hosted service остаётся отдельным launch gate. Не обещать уведомление о полном падении машины по одному этому timer.

## Независимый off-VPS dead-man (отдельный всегда-включённый хост)

`deploy/deadman-monitor.py` + systemd units — независимая проверка `https://endlessdrawing.com/readyz` с **другого** хоста (не приложение-VPS). Alert только после N подряд неудач, одно восстановление, не спам. Это закрывает «полностью выпал VPS/его сеть»: сообщение шлёт не приложение-VPS.

### Отдельный VPS (предпочтительно: не выключается)

Скопировать на этот хост `deploy/deadman-monitor.py`, `deploy/deadman/endlessdrawing-deadman.service`, `deploy/deadman/endlessdrawing-deadman.timer`, `deploy/install-deadman.sh` (например, `scp` или shallow `git clone` backend). Затем root:

```bash
sudo bash deploy/install-deadman.sh
# Только в своём доверенном интерактивном TTY, token скрыт getpass:
sudo /usr/local/sbin/endlessdrawing-deadman configure --chat-id <numeric-chat-id>
```

`configure` проверяет @username бота, шлёт тест, требует **yes**, атомарно пишет `/etc/endlessdrawing/deadman.env` (root 0600) и включает `endlessdrawing-deadman.timer` (каждые 10 минут). Проверка: `sudo systemctl list-timers endlessdrawing-deadman.timer`, `sudo /usr/local/sbin/endlessdrawing-deadman test`, `... probe`. Token не в argv/env/history, не в chat/CI.

### macOS (вторично; Mac не всегда включён)

Тот же скрипт, конфиг `~/.config/endlessdrawing/deadman.conf` (owner 0600) вместо `/etc`. Токен пишется через тот же `configure` (не Keychain). Запускается launchd `deploy/deadman/endlessdrawing.deadman.plist` (каждые 10 минут).

State — `/var/lib/endlessdrawing-deadman/state.json` (VPS) или `~/.local/share/endlessdrawing-deadman/state.json` (Mac): счётчик неудач + timestamp, без token/chat message bodies. Чат ID не секретен; token в чат не присылать.

## Что проверяется

Каждые 15 минут (+ до 30s), и сразу по systemd OnFailure backup job:

- loopback backend readiness / доступность PostgreSQL;
- доверенный TLS hostname/certificate, срок менее 14 дней;
- диск VPS: менее 5 GiB или 15% свободно;
- off-site verified backup не старше 36h;
- systemd backup service Result;
- operator age decrypt/restore proof не старше 35 дней;
- наличие ранее созданного TOTP encryption file (байты не читаются/не логируются);
- реальная Storage Box quota через строго pinned dedicated SSH subaccount, read-only `df -Pk backups`: >=85% used или менее 5 GiB свободно. Это общий capacity risk Box с construct; чужие файлы/cron/config не меняются.

Сообщения: первое обнаружение/изменение проблемы, напоминание того же состояния раз в 6 часов, однократное восстановление. После 04:20 UTC — один daily-green при подтверждённом backup этого дня. Нет автоматических restart/delete/restore/rotate операций. При неудачной Telegram отправке успех не записывается, следующая проверка повторяет отправку. Journal не содержит токен/URL провайдера/response body/SQL/headers/рисунки/QR/коды.

## Безопасная настройка владельцем

1. Создать отдельного бота через `@BotFather` → `/newbot`, сохранить token в менеджере паролей.
2. Открыть своего бота и отправить `/start`.
3. Узнать numeric chat ID. Не присылать token в переписку/CI. Для уже известного chat ID getUpdates не нужен — configure не трогает webhook/polling других проектов.
4. Только в своём доверенном интерактивном терминале:

```bash
ssh -t -i ~/.ssh/stokholm subwhale@212.147.244.161 \
  'sudo -n /usr/local/sbin/endlessdrawing-monitor configure --chat-id <numeric-chat-id>'
```

Token вводится getpass (без echo), не argv/env/history. CLI покажет @username бота, отправит тест и попросит **yes**, только если он пришёл в нужный чат. После подтверждения атомарно сохраняется `/etc/endlessdrawing/alerts.env` (`root:root 0600`, parent 0700), включается timer и запускается первая проверка. App/Caddy/Actions token не получают. Никакого shell source, browser Storage или Telegram credentials в repository.

Владелец/агент затем проверяет только metadata:

```bash
sudo systemctl list-timers endlessdrawing-monitor.timer
sudo systemctl show endlessdrawing-monitor.service -p Result -p ExecMainStatus
sudo /usr/local/sbin/endlessdrawing-monitor probe
# Дополнительный реальный тест (фиксированный текст, без чувствительных payload):
sudo /usr/local/sbin/endlessdrawing-monitor test
# Две явные TEST-симуляции failure/recovery; реальные state/сервисы не меняются:
sudo /usr/local/sbin/endlessdrawing-monitor drill
```

State в `/var/lib/endlessdrawing-monitor/state.json` — только issues/time/test-confirmed metadata, без token/chat message bodies. Не печатать config или полный environment. На смену получателя state сбрасывается после нового подтверждённого теста.

## Установка и тесты

`deploy/install-operations.sh` устанавливает helper и units, но не включает monitor и не создаёт Telegram credentials. Существующий backup timer сохраняется. Config отсутствует → monitor service пропускается по ConditionPathExists, это не «monitoring ready».

Linux safety suite: **69 tests** (66 прежних, dead-man VPS config/validate/decision), privacy/permissions/symlink guards, token/ID parsing, read-only quota, health/expiry/staleness thresholds, failure dedup/recovery/daily policy, no redirects, redacted provider errors, no false success after failed send, TTY-only setup и обязательное ручное подтверждение доставки. Unit tests не подтверждают настоящую Telegram delivery.
