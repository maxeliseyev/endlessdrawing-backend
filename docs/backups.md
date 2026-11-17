# Внешние бэкапы: Hetzner Storage Box + age

## Статус

Подготовлены и установлены на VPS export/restore helper, age uploader и проверки. 17.11.2026 настоящий Storage Box subaccount `u651188-sub1` принял отдельный SSH-ключ: подтверждены daily + weekly upload, независимые SHA-256 и полный запуск systemd service (`Result=success`, exit 0). Download ciphertext обратно на машину оператора совпал по SHA с доверенным VPS state. Настоящий удалённый snapshot успешно расшифрован операторским age identity **на Mac** и восстановлен в отдельный network-none/read-only PostgreSQL 17 контейнер; исходная БД не менялась. Контейнер и временный plaintext удалены, ключ на VPS не копировался. После этого ежедневный timer **включён**, следующее срабатывание при проверке — 18.11.2026 04:13:28 UTC.

Пройден реальный **локальный тест** PostgreSQL 17 → pg_dump → отдельная restore DB → age → decrypt одноразовым тестовым ключом → независимый readback restore. Также проверен VPS local-only pipeline в systemd sandbox (32 MiB peak). Проверка настоящего операторского identity теперь также пройдена отдельно; его приватный ключ на VPS не устанавливался.

Подход согласован с работающим `construct-server/ops/backup.sh`: шифрование **до** отправки, приватный age identity вне VPS, SSH port 23, SHA-256 и retention 7 daily / 4 weekly. Сам construct-скрипт, его каталог, credentials и cron не изменяются. Redis у Endless Drawing нет.

После закрытой выкладки backend `ff832e5` / frontend `5cc6de7` выполнен ещё один полный настоящий off-site round-trip: snapshot **`20261117T094648Z-a1354607`**, operator age decrypt на Mac, PostgreSQL 17 restore, migrations **1/2/3**. В нём 9 soft-deleted test strokes после трёхбраузерной staging-проверки, 0 accounts; контейнер/plaintext удалены, nonsensitive proof обновлён на VPS и у оператора. Это также проверка backup новой schema 0003, не только исходной пустой БД.

## TOTP key (новая политика владельца)

Google Authenticator — резерв вместо обязательного второго passkey; см. [admin-totp](admin-totp.md). При наличии private server KEK `/opt/endlessdrawing/admin-secrets/totp.key` export формирует **format 2**, добавляет `totp-key.bin` (32 bytes) в fixed checksums и **внутрь age archive**. В Box никогда не отправляется raw KEK. Это не операторский age identity; последний по-прежнему вне VPS. Format 1 (три файла, без KEK) остаётся поддержан. При restore нужна соответствующая key file; не генерировать новую вместо потерянной. Verifier дополнительно очищает pending TOTP и сдвигает last-used step за текущий +1, чтобы restore не воскресил replay window.

## Изоляция от construct

В Hetzner создать НОВЫЙ Storage Box subaccount:

- реальный корневой каталог **`endlessdrawing`**, не `construct` и не корень всего Box;
- SSH support и read/write, отдельный публичный SSH-ключ Endless Drawing;
- username вида `uNNNNNN-subN`, host этого subaccount `uNNNNNN-subN.your-storagebox.de` либо его родительского Box `uNNNNNN.your-storagebox.de`, port **23**. Host другого Box/subaccount отвергается;
- приватный SSH-ключ остаётся root-only на VPS; это НЕ age identity и НЕ операторский `~/.ssh/stokholm`.

На VPS уже создан `/etc/endlessdrawing/backup/id_ed25519`; его публичную часть можно получить в доверенном терминале:

```bash
sudo ssh-keygen -y -f /etc/endlessdrawing/backup/id_ed25519
```

Логическая структура внутри chroot subaccount:

```text
backups/incoming/<UTC_TIMESTAMP>-<random>/        # ещё не опубликованные копии
backups/daily/<UTC_TIMESTAMP>-<random>/pg-....tar.age
backups/weekly/<UTC_TIMESTAMP>-<random>/pg-....tar.age
```

То есть физически это `endlessdrawing/backups/...`. Код отвергает master username и любой `BACKUP_REMOTE_DIR`, кроме `backups`. Rotation удаляет только строго валидные snapshot names внутри `backups/daily` / `backups/weekly`, без wildcard и обхода `..`; формат construct без random suffix также не подходит фильтру. Это дополнение к chroot, не замена правильной настройки subaccount в панели Hetzner.

Приватный age identity должен быть на машине оператора и в менеджере паролей. Допустимо шифровать на уже используемый публичный age recipient; для независимого криптографического домена можно создать отдельный identity **на машине оператора**, не на VPS. Публичный recipient не является секретом.

## Установка без активации

После review файлов `deploy/backup*`, `deploy/install-operations.sh`, `deploy/systemd/` выполнить установщик root на VPS из полного проверенного каталога `deploy/`. Он ставит недостающие host tools, root-owned helpers и units, но **не включает timer**, не подключается к Box, не меняет runtime.env/БД и не запускает Caddy. Существующие backup.env сохраняются.

Файлы:

- `/usr/local/sbin/endlessdrawing-backup-export` ← `deploy/backup.sh`;
- `/usr/local/sbin/endlessdrawing-backup` ← `deploy/backup-storagebox.py`;
- `/usr/local/sbin/endlessdrawing-backup-health` ← `deploy/backup-health.py`;
- `/etc/endlessdrawing/backup.env` — `root:root 0600`, пример `deploy/backup.env.example`;
- `/etc/endlessdrawing/backup/{id_ed25519,known_hosts}` — отдельные SSH credentials/pin;
- `/var/lib/endlessdrawing-backup/` — root-only spool/state/logs.

В `backup.env` вписать публичный recipient, реальный host/subaccount и пути. Файл парсится как данные, не `source`/shell. Private keys или пароли не вставлять в env/argv/Actions. `--local-only` требует только валидный публичный age recipient; внешние SSH credentials ему не нужны, успех off-site он не записывает.

### Host key

`StrictHostKeyChecking=yes`, `IdentitiesOnly=yes`, отдельный known_hosts и `-F /dev/null` обязательны. Кандидат `ssh-keyscan` **не доказывает подлинность**. Сверить fingerprint с Hetzner/доверенным known_hosts работающего construct, после чего установить только подтверждённую строку для `[host]:23` в выделенный known_hosts. Не копировать приватный SSH-ключ construct и не отключать host checking.

## Конвейер и ограничения

1. Общий deploy lock исключает экспорт/restore одновременно с миграцией/активацией. Проверяется запас диска; полная проверка восстановления требует места под дополнительную БД и WAL.
2. `pg_dump --format=custom --no-owner --no-privileges`: векторы, ownership digests, tombstones, accounts/credentials, audit, migrations. Custom format уже сжимает данные — второй gzip не нужен. Нет искусственного порога 1 MiB: новая пустая БД может давать маленький валидный архив.
3. Archive list + SHA-256 фиксированного набора файлов и manifest с image/frontend SHA/PG major/размером БД. Это ещё не off-site backup.
4. Реальный `pg_restore` в случайную **новую** `ed_verify_*` БД, проверка схемы/migrations, удаление только собственной временной БД. Ошибка create не даёт права drop существующего имени. Рабочая БД не восстанавливается. Дамп/SQL ошибки остаются в root-only logs, не в journal/body ответа.
5. `tar(database.dump, manifest.json, SHA256SUMS, optional totp-key.bin)` → `age -r PUBLIC_RECIPIENT`. Приватного age identity нет на VPS. После успешного шифрования plaintext bundle удаляется; при сетевом сбое ciphertext сохраняется для диагностики/повтора. Failed export может оставить root-only incomplete bundle без READY: контролировать диск и убирать вручную после расследования.
6. Upload в incoming, SSH SHA-256, отдельный `mv` в daily и повторная SHA-проверка. По воскресеньям (либо `--weekly`) отдельная weekly copy с проверками до/после публикации. Каждая remote операция — ОДНА команда, без `&&`, pipe или redirect: ограниченный shell Storage Box может молча отбросить их. Если `mv/cp` возвращает ложный успех, обязательная финальная SHA-проверка не даёт записать успех/запустить rotation.
7. Только после всех проверок — retention 7 daily / 4 weekly. Полный подтверждённый успех атомарно записывается в `last-success.json`. Неподдерживаемая команда, невалидный hash, failed upload/restore или rotation дают ненулевой exit. Старые хорошие копии до upload verification не удаляются.

Дамп содержит приватные модерационные данные и password hashes. Не скачивать plaintext в общий каталог, не прикладывать SQL/logs к Actions или чату. Консистентный логический dump не включает WAL/PITR: при исправном расписании катастрофа может потерять до суток последних записей. Один и тот же Box с другим проектом — общий риск quota/provider/account; настроить мониторинг квоты.

## Первый запуск и расписание

```bash
# На VPS, после заполнения config и проверки host key:
sudo /usr/local/sbin/endlessdrawing-backup --local-only
sudo /usr/local/sbin/endlessdrawing-backup --weekly
sudo /usr/local/sbin/endlessdrawing-backup-health
```

Local-only не обновляет last-success и не считается внешней копией. После настоящего upload оператор должен скачать один `.tar.age`, расшифровать **своим** identity на своей машине и восстановить в отдельный PostgreSQL 17. Нельзя временно копировать этот identity на VPS ради автоматизации проверки.

Только после успешного реального upload + операторского decrypt/restore:

```bash
sudo systemctl enable --now endlessdrawing-backup.timer
sudo systemctl list-timers endlessdrawing-backup.timer
```

Расписание: ежедневно **04:10 UTC + случайная задержка до 10 минут**, после construct cron 03:30 UTC. По воскресеньям weekly. Persistent timer догоняет пропущенный запуск после включения/перезагрузки. Это не меняет cron другого проекта.

`endlessdrawing-backup-health` возвращает ошибку, если нет подтверждённого off-site + local restore drill успеха за последние 36 часов, дата в будущем/невалидна или local-only. Подключить к внешнему мониторингу **и проверить доставку алерта**, а также отслеживать `systemctl --failed`/статус backup service. Сам journal/CLI не является уведомлением владельца. Не ставить галочку «monitoring готов» без внешнего канала.

SSH upload key с write/delete правами может удалить собственные бэкапы при компрометации VPS. Настроить Storage Box provider snapshots/отдельную защиту retention, недоступную этому subaccount; age защищает конфиденциальность, но не предотвращает удаление ciphertext.

## Восстановление после аварии

Автоматического restore в production **нет**. Действия оператора:

1. Закрыть сеть/запись, зафиксировать инцидент и оценить допустимую потерю данных; не откатывать БД автоматически при image rollback.
2. Скачать подходящий snapshot в приватный каталог на машине оператора, проверить ciphertext SHA по доверенным записям; `age -d -i <identity-file> -o recovered.tar ...` без вывода plaintext в терминал. Проверить состав tar перед извлечением, извлечь только ожидаемые относительные файлы и сверить SHA256SUMS.
3. Восстановить custom dump через `pg_restore --exit-on-error --no-owner --no-privileges` в **новую изолированную** PostgreSQL 17 DB. Сопоставить manifest/migrations и совместимый backend image; вернуть роль/новые DB credentials закрытым операционным путём.
4. Перед подключением приложения к восстановленной БД отозвать **все** admin sessions, preauth и enrollment grants, повысить auth_version, чтобы старый backup не воскресил отозванные cookie/права. Эти действия — только на restore candidate, после остановки записи и отдельного review. Факторные counters тоже могут быть старее устройства; пройти новый WebAuthn login/recovery по ситуации.
5. Проверить ownership/tombstones/audit и реальную браузерную интеграцию, затем отдельно разрешить переключение. Старую рабочую БД и архив сохранить до принятия результата.

## Тесты

```bash
# Mock safety tests, Linux; без настоящего Box:
docker run --rm -v "$PWD:/workspace:ro" -w /workspace python:3.14-slim \
  python3 -m unittest discover -s tests/deploy -p 'test_*.py' -v
# Real local dump/restore + age round-trip, только disposable Docker project:
python3 tests/deploy/smoke_backup.py endlessdrawing-local:integrated-admin
```

35 deployment/backup safety tests прошли. Дополнительно проверено, что helper не получает stdin оператора/SSH script (Docker exec не должен съедать последующие команды). CI real drill использует одноразовый тестовый age identity и отдельные Docker volume/DB, а не реальные credentials Storage Box или операторский identity. Отдельная настоящая operator-проверка выполнена `deploy/verify-backup.py`: он сверяет независимо полученный ciphertext SHA, передаёт identity только native age CLI, извлекает только три ожидаемых regular tar entry, проверяет payload checksums и поднимает ограниченный local-only PostgreSQL контейнер без сети/host mounts. В restore candidate отзываются sessions/ceremonies/enrollment; production не трогается. Проверка ограничена маленькими backup DB до 256 MiB — для больших данных нужен отдельно спланированный test host/resources.

Несекретный proof JSON сохранён у оператора и в `/var/lib/endlessdrawing-backup/last-operator-restore.json` на VPS. Проверенный snapshot `20261117T075339Z-86a8a777`: migrations 1/2 валидны, 0 strokes/accounts/moderation operations. Это соответствует старому deployed frontend `623c5a6` с локальным drawing-прототипом; не означает, что новые public WS/brush изменения уже опубликованы. Проверка не доказывает доставку внешних alerts или provider snapshot protection — эти gates остаются отдельно.
