# Деплой через GitHub Actions: один VPS

## Статус и границы

Workflow подготовлен; первая попытка Actions остановилась до активации из-за отсутствующего helper. **Серверный bootstrap теперь выполнен**, Docker CE восстановлен/запущен, `/usr/local/sbin/endlessdrawing-activate check` проходит. После этого владелец подтвердил доступ к приложению через SSH-туннель. Это не проверка shared persistence или новой модерации; admin-изменения ещё не выкладывались. SSH/firewall и публичный reverse proxy не менялись.

VPS: `subwhale@212.147.244.161`, Linux amd64, Ubuntu 26.04.1, 2 vCPU, 8 ГБ RAM, диск 40 ГБ (на момент проверки свободно около 35 ГБ). Для начального закрытого стенда достаточно; диск и размер БД/бэкапов контролировать.

Frontend: `https://github.com/maxeliseyev/endlessdrawing-frontend`, локально `../endless-drawing-frontend`. **Сейчас это локальный прототип без WS и admin UI**: сборка/выкладка не делает его общим сохраняемым холстом. Интеграция с backend и модерация — отдельные обязательные этапы.

Начальный стенд намеренно закрыт: приложение слушает только `127.0.0.1:8080` на VPS, PostgreSQL вообще без published ports. Публичный proxy/80/443 пока не настраивается. Backend подключён к bridge `edge` для loopback-publishing и к внутренней `private` для БД; PostgreSQL подключён только к `private`. Требования до публичного запуска — [модерация](moderation.md), TLS и внешние проверенные бэкапы.

## 1. Путь релиза

Workflow `.github/workflows/deploy.yml` в backend-репозитории, Actions → **Deploy staging** → Run workflow:

1. Переиспользовать Backend CI: shell deployment tests, Rust fmt/clippy, все PostgreSQL/WS-тесты.
2. Checkout backend выбранного запуска и отдельного frontend на `frontend_ref`; предпочтительно проверенный commit SHA, `main` допустим для прототипа.
3. Node 22: `npm ci`, check, unit tests и build. Browser E2E здесь не запускаются.
4. Зафиксировать оба SHA в `release.json`, выбрать тег `endlessdrawing-app:<backend_sha>-<run_id>-<run_attempt>`, не изменяемый `latest`.
5. Собрать **один linux/amd64 Docker-образ** с revision labels: Rust binary + frontend `dist/`. Rust компилируется на Actions runner, не на VPS.
6. Загрузить образ на runner и проверить с одноразовой PostgreSQL через production Compose: HTML/assets, probes, non-root/read-only и отсутствие опубликованного DB-порта. Это smoke-test, не браузерная E2E-интеграция общего мира.
7. Сохранить gzip Docker archive как Actions artifact на 7 дней. Runtime credentials в image/artifact не включаются.
8. После approval environment `staging` загрузить artifact и передать image через SSH в root-owned deployment helper.
9. Helper берёт host lock, проверяет диск, делает локальный pre-deploy DB dump, активирует контейнер и ждёт readiness. При неудаче пытается вернуть прежний образ, но job остаётся failed.

GHCR и registry-token на сервере не нужны: используется `docker image load` по проверенному SSH. Доступ к приватному frontend-репозиторию настраивается отдельно read-only token.

Сейчас workflow **только ручной** (`workflow_dispatch`). Ни PR, ни push, ни frontend commit сами не выкладываются. После стабилизации можно включить `push main` и отдельный frontend dispatch с GitHub App; не добавлять широкий cross-repo write PAT только ради первого стенда.

## 2. Однократный bootstrap VPS

Сначала прочитать `deploy/bootstrap.sh`, `deploy/compose.yml`, `deploy/activate.sh`. Затем с доверенного компьютера, из backend-репозитория:

```bash
# Сначала вручную проверить SSH host key, не отключать host checking:
ssh -i ~/.ssh/stokholm subwhale@212.147.244.161
exit

# Явно разрешённая установка на VPS:
SSH_IDENTITY="$HOME/.ssh/stokholm" bash deploy/bootstrap-remote.sh
```

Скрипт требует `sudo -n` на VPS. На чистом сервере устанавливает Docker/Compose из Ubuntu repositories; существующую согласованную установку Docker CE или Ubuntu **сохраняет**, не смешивает providers. При незавершённой настройке пакетов останавливается с диагностикой. Включает Docker и размещает:

| Путь на VPS | Содержимое |
| --- | --- |
| `/opt/endlessdrawing/compose.yml` | Root-owned Compose; один backend, PostgreSQL, внутренняя сеть |
| `/etc/endlessdrawing/runtime.env` | Сгенерированные случайные реквизиты БД, `root:root`, `0600` |
| `/usr/local/sbin/endlessdrawing-activate` | Root-owned helper для check/deploy/rollback |
| `/opt/endlessdrawing/backups/` | Закрытые локальные pre-deploy dumps, `0700` |
| Docker volume `endlessdrawing_postgres_data` | Постоянные данные PostgreSQL |

Bootstrap **не** запускает приложение, не открывает порты и не меняет SSH/firewall. Повтор сохраняет существующий runtime.env, но обновляет helper/Compose из проверенного checkout; это нужно после изменений `deploy/`. Изменение Compose само по себе применяется при следующей активации, поэтому такие изменения требуют отдельного review.

Пароль генерируется на сервере и не печатается/не отправляется в GitHub. Изменение env не меняет пароль уже инициализированной БД — ротация требует отдельной процедуры. Runtime env не надо копировать в локальный `.env`, frontend или artifacts.

### Если Actions пишет `endlessdrawing-activate: command not found`

Это отсутствующий/незавершённый bootstrap, не проблема Docker image. Проверить по SSH:

```bash
sudo test -x /usr/local/sbin/endlessdrawing-activate
sudo dpkg --audit
sudo /usr/local/sbin/endlessdrawing-activate check
```

Если helper отсутствует, сначала устранить незавершённую установку пакетов из **одного** источника и повторить bootstrap. Не смешивать `docker.io`/`containerd`/`docker-compose-v2` с Docker CE `containerd.io`/`docker-compose-plugin`, не обходить конфликт через `dpkg --force-overwrite`. На данном VPS восстановлен Docker CE без удаления данных/volumes, helper установлен и preflight проверен. Повторить Actions → failed run → **Re-run failed jobs**; пересобирать/пушить незавершённую модерацию для этого не требуется.

## 3. GitHub environment и доступ

В backend-репозитории создать environment **staging**:

- разрешить deployment только из защищённой ветки `main`;
- включить required reviewer, если тариф GitHub это поддерживает; не считать само имя environment механизмом approval;
- проверить, что запуск с другого ref не получает серверные secrets;
- разрешение на изменение workflow и frontend commit считать доступом к коду, который исполнится на сервере.

Environment variables:

| Variable | Значение |
| --- | --- |
| `DEPLOY_HOST` | `212.147.244.161` |
| `DEPLOY_USER` | `subwhale` либо выделенный deploy-user |
| `DEPLOY_PORT` | `22` |

Environment secrets:

| Secret | Назначение |
| --- | --- |
| `DEPLOY_SSH_KEY` | Приватный **отдельный CI deployment key**, не содержимое ключа из Git |
| `DEPLOY_KNOWN_HOSTS` | Проверенная строка/строки known_hosts для сервера |

Не читать/публиковать `~/.ssh/stokholm` в логах или репозитории. Лучше оставить этот ключ операторским, а для Actions создать отдельную пару:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/endlessdrawing-actions -C endlessdrawing-actions
# Публичный .pub добавить в authorized_keys выбранного пользователя на VPS.
# Приватный ключ вне репозитория вручную сохранить в GitHub environment secret.
```

Key без passphrase допустим для headless Actions только с жёстко защищённым environment. Доступ через `subwhale` с широким sudo является высокопривилегированным. Предпочтителен отдельный пользователь, с sudo только на root-owned helper; сам Docker/deploy всё равно доверенная привилегированная граница. Не добавлять CI-пользователя в docker group под видом непривилегированного доступа.

Host key при обследовании VPS через текущий SSH-доступ:

```text
ED25519 SHA256:19oA7HgNWNoiyXyQ+XH8L3lZ7y96Okcv6lVlJAm3qOE
```

Сверить с доверенной консолью провайдера/проверенным SSH-сеансом. `ssh-keyscan` только получает ключ, не доказывает его подлинность:

```bash
ssh-keyscan -t ed25519 212.147.244.161 > /tmp/endlessdrawing-known-hosts
ssh-keygen -lf /tmp/endlessdrawing-known-hosts
# После сверки сохранить файл как DEPLOY_KNOWN_HOSTS, не секрет из ~/.ssh/stokholm.
```

Workflow использует `StrictHostKeyChecking=yes`, без runtime keyscan и без `sudo` password в Actions.

Если frontend repo приватный, в **repository secrets backend** добавить `FRONTEND_READ_TOKEN`: fine-grained token только с contents:read для `endlessdrawing-frontend`. Он нужен build-job, без серверного SSH-доступа. Для публичного repo достаточно `GITHUB_TOKEN`. `persist-credentials: false` не оставляет checkout credentials для npm build.

## 4. Первый запуск

1. Закоммитить/запушить backend, включая realtime-файлы, Dockerfile, `deploy/` и workflows.
2. Убедиться, что выбранный frontend commit тоже запушен; локальные незакоммиченные файлы Actions не увидит.
3. Выполнить bootstrap, настроить environment, secrets и protected branch.
4. Actions → Deploy staging → `main`, `frontend_ref=<проверенный SHA>` → одобрить deployment.
5. Проверить, что job завершён успешно. Локально открыть туннель:

```bash
ssh -i ~/.ssh/stokholm -N -L 8080:127.0.0.1:8080 subwhale@212.147.244.161
# В другом терминале:
curl -f http://localhost:8080/healthz
curl -f http://localhost:8080/readyz
# В браузере: http://localhost:8080/
```

Порт 8080 на своём компьютере должен быть свободен; при смене local port надо добавить соответствующий Origin в server runtime.env. Ни публичный IP:8080, ни PostgreSQL из Интернета доступны быть не должны. Docker port publishing и cloud firewall проверить отдельно после реального запуска.

## 5. Состояние, откат и дисковое место

Helper сериализует деплои через `flock`, не делает `down -v`, не удаляет images/volumes/backups. Backend read-only, non-root, без Linux capabilities; PostgreSQL и backend ограничены по 2 ГБ RAM каждый. PostgreSQL остаётся доступен при замене backend.

При failed readiness:

- если прежний image существует — попытка поднять его;
- если он тоже неготов или первого image ещё нет — backend останавливается, оператору сообщается ошибка;
- current-image обновляется **только** после успешного health-check; при двойном сбое это last-known-good, не утверждение о работающем контейнере;
- причина расследуется по SSH, runtime.env/полный `docker inspect`/resolved Compose не печатаются в Actions.

Ручной image rollback на сохранённый release:

```bash
# RELEASE — suffix immutable tag из истории успешного Actions run.
ssh -i ~/.ssh/stokholm subwhale@212.147.244.161 \
  'sudo -n /usr/local/sbin/endlessdrawing-activate rollback BACKEND_SHA-RUN_ID-RUN_ATTEMPT'
```

**Image rollback не откатывает БД.** Миграции нового бинарника могли уже примениться. Более того, старый SQLx migrator может отказать из-за отсутствующей у него новой migration version даже при совместимой схеме. Для таких релизов нужен forward-fix либо специально проверенный rollback-образ с актуальным migration manifest. Не обещать универсальный автоматический откат.

Pre-deploy dump создаётся до запуска миграций. `pg_restore --list` проверяет читаемость архива, **не** восстановление. Автоматического pg_restore в рабочую БД нет: он мог бы удалить подтверждённые после backup штрихи. Восстановление допускается только по отдельному плану с остановкой записи и оценкой потери данных.

40 ГБ легко заполнить dumps/images/WAL. Helper откажет при свободном месте менее 2 GiB; перед dump дополнительно требует запас на полный размер БД плюс эти 2 GiB. Это не заменяет мониторинг и off-site backup. Настроить alert раньше. Не запускать `docker system prune --volumes`; сначала определить current/previous images и политику retention. Actions artifacts удалятся через 7 дней, runtime-образы на VPS автоматически не удаляются.

## 6. До публичного endlessdrawing.com

- Подключить frontend к реальному WS и пройти двухбраузерные E2E, не выдавать локальный прототип за сохраняемый мир.
- Реализовать и проверить [закрытую модерацию с MFA](moderation.md); `/admin/` не публичная продуктовая страница.
- Подтвердить DNS/прокси домена: сейчас публичный DNS не возвращает VPS IP напрямую; если используется Cloudflare, проверить origin, WebSocket и режим TLS, не делать вывод по одному A-record.
- Установить Caddy/reverse proxy, HTTPS/WSS на одном origin, точный HTTPS `ALLOWED_ORIGINS`, закрыть обход proxy и доверять forwarded IP только от него.
- Ограничить подключения/логин/модерацию по IP, защитить SSH, проверить firewall. Не отключать существующий доступ SSH при автоматизации.
- Перейти на отдельные DB roles без superuser у приложения; production migrations/backup выполнять закрытым операционным путём.
- Настроить ежедневные **внешние** зашифрованные бэкапы и расписание тестового восстановления в изолированную БД, мониторинг диска/памяти/ошибок. Копии на том же VPS недостаточны.
- Утвердить правила модерации, юрисдикцию и retention. Затем отдельно разрешить публичный запуск и автоматический deploy с main.

## 7. Что проверено локально

- actionlint для обоих workflows, синтаксис Bash и Compose config;
- 11 тестов activation state machine в Linux-контейнере с mock Docker, включая host lock, failed backup, мало места, failed readiness и image rollback;
- 62 backend-теста с PostgreSQL, включая 15 admin-тестов; frontend TypeScript, 41 unit-тест и build были проверены на предыдущем этапе, в этой backend-проверке повторно не запускались;
- фактическая сборка native ARM64 Docker-образа с provisioning CLI и smoke-test с одноразовым PostgreSQL: статика/assets, probes, non-root/read-only, admin-disabled default, отказ CLI без TTY, отсутствие published DB-порта, `pg_dump` и читаемость архива.

Bootstrap на VPS и серверный preflight проверены после восстановления Docker CE; доступ через SSH-туннель затем подтверждён владельцем. Новая модерация на VPS не включалась, полноценное восстановление backup и browser E2E общего мира/модерации пока не подтверждены. Последние функции frontend/admin ещё не реализованы. AMD64 container smoke-test включён в workflow как gate до SSH.

Для повторения локального container smoke-test (не деплой):

```bash
# После npm run build в соседнем frontend:
mkdir -p frontend-source/dist
cp -R ../endless-drawing-frontend/dist/. frontend-source/dist/
docker build -t endlessdrawing-local:check .
python3 tests/deploy/smoke_container.py endlessdrawing-local:check
```

Скрипт использует свободный local port 8080 и случайное Compose project name, создаёт/удаляет только собственный тестовый volume. Не запускать его вместо операционного restore-check рабочей БД.

## 8. Admin/passkeys: отдельный gate

`deploy/compose.yml` передаёт `ADMIN_ENABLED` (по умолчанию false), `ADMIN_ORIGIN` и `ADMIN_RP_ID`. Новая копия root-owned Compose должна быть установлена bootstrap до использования этих флагов: Actions доставляет образ, но не обновляет серверный Compose. Никаких CA mounts или admin bearer secrets в runtime.env не требуется.

[Операционная инструкция](admin-operations.md): окончательный HTTPS origin, отдельный frontend entry, доверенный интерактивный SSH provisioning и две passkey регистрации. HTTP SSH-туннель не заменяет HTTPS для Secure cookies и Touch ID. Backend не включается публично на основании software-authenticator тестов. [Dependency audit пока имеет замечания](security-dependencies.md), игноры не добавлены.
