# PostgreSQL: maintenance, migration, runtime

## Граница

Не делать `REVOKE SUPERUSER/CREATEDB` у старого `canvas`: он остаётся закрытым maintenance/provisioning/backup role внутри непубликуемого PostgreSQL. Backup должен создавать/удалять **только собственную одноразовую restore DB**. Пароль этого role не попадает в backend container.

- `canvas_migrator`: отдельный LOGIN, NOSUPERUSER/NOCREATEDB/NOCREATEROLE/NOREPLICATION/NOBYPASSRLS; owner public schema, application tables/sequences и `_sqlx_migrations`. CREATE **в application database** (не CREATEDB) для схем/доверенных extensions.
- `canvas_app`: отдельный LOGIN без перечисленных cluster privileges, role memberships и persistent ownership/CREATE. SELECT/INSERT/UPDATE/DELETE application tables, sequence USAGE/SELECT, schema USAGE; `_sqlx_migrations` **SELECT-only**.
- Мигратор `/app/canvas-migrate`: краткоживущий Compose `maintenance` profile, отдельный DSN, без HTTP, host ports, admin KEK mount и application session. Lock timeout 30s / statement timeout 120s. Большие миграции требуют отдельного плана.
- Backend: `DATABASE_MIGRATE_ON_START=false`, `DATABASE_REQUIRE_RUNTIME_ROLE=true`. До listener проверяет effective role, отсутствие cluster/DDL/ownership/membership capabilities и точное совпадение версий/checksum/success embedded manifest. Никакого CREATE/ALTER/migration lock.
- Только локальная разработка по умолчанию сохраняет embedded startup migrations. Нельзя одновременно включать mandatory runtime role и startup migration.

Compose передаёт maintenance/migration credentials только соответствующему контейнеру, не через общий env_file backend. Не печатать `runtime.env`, state JSON, resolved compose или `.Config.Env`. CLI/SQLx statement logging отключено; credential SQL не попадает в diagnostics и исполняется с отключённым server statement/error-statement logging.

## Два этапа перехода без потери rollback

Helper `deploy/db-roles.py` устанавливается как `/usr/local/sbin/endlessdrawing-db-roles`. Root-only, один deploy/backup flock; credentials создаёт сам на VPS и сохраняет в root-only state/config. Не вводить/копировать их через чат или Actions. Таблицы/seed/passkey/password владельца не сбрасывает. Не перезапускает приложение и не восстанавливает данные.

1. Убедиться в recent off-site backup + operator decrypt/restore proof, сохранить private gate. Проверить текущую рабочую схему 1/2/3/4 и healthy image.
2. `sudo endlessdrawing-db-roles prepare`: создать отдельные роли, перенести ownership **только application schema objects**, выдать DML, подготовить MIGRATION_DATABASE_URL. Runtime DATABASE_URL пока намеренно остаётся старым; mandatory guard false. Никогда не использовать кластерный `REASSIGN OWNED BY canvas`: он затронул бы shared databases/templates.
3. Через manual Actions развернуть reviewed bridge SHA с мигратором/manifest check. Backend по-прежнему временно использует старый role. В случае отказа предыдущий образ ещё совместим с прежним DSN. Migration failure останавливает deploy **до** замены старого backend; private log и pre-deploy dump сохраняются.
4. `sudo endlessdrawing-db-roles switch`: требует label `io.endlessdrawing.db-roles-version=1`, ровно один healthy running backend с текущим image, проверяет настоящий TCP/SCRAM app login и отсутствие cluster/public CREATE privileges. Сохраняет private previous env; атомарно готовит app DSN + schema-only mode + mandatory guard для **следующей** выкладки. Не рестартует сам.
5. Ещё одна manual Actions выкладка immutable SHA pair: новый runtime использует `canvas_app`; предыдущий bridge понимает schema-only mode и остаётся совместимым rollback. Не прыгать напрямую со старого образа на ограниченный DSN: его startup migrations сломаются.
6. Проверить runtime effective role/flags/ownership/DDL denials, health, реальные public/admin operations, новый off-site backup и operator decrypt/restore. Затем отметить production acceptance отдельно от реализации. `status` выводит только role metadata.

Мигратор после каждой схемной выкладки атомарно обновляет DML/sequence grants новых application tables; migration metadata остаётся SELECT-only, default DML privileges на неё не выдаются. Restore использует `--no-owner --no-privileges`; при disaster recovery сначала closed maintenance restore и настройка ролей/grants, а не публикация restored DB. Server DB login credentials можно заново сгенерировать независимо от owner password/passkey/TOTP; recovery не означает пере-enrollment владельца.

Автоматическое image rollback не восстанавливает БД. Несовпадение schema manifest (в том числе newer schema) fail closed; несовместимые миграции требуют forward-fix/operator review.

## Доказательства

Три PostgreSQL tests: readonly manifest missing/dirty/changed/future; полноценная fresh migration от non-superuser; application DML и отказ CREATE/ALTER/DROP/role/files/program/metadata writes, включая NOINHERIT membership escape. Все role DDL внутри rollback transaction одноразовой SQLx DB, cleanup также при panic.

Deployment tests: migration fail до backend replacement, private atomic state/credentials, unknown existing role отказ, bridge capability/health/TCP-login guards, nonroot/symlink/permission protection. Настоящий Docker backup smoke дополнительно делает ownership transfer → migration role login → app login → healthy guarded app → denied runtime migrator → maintenance backup restore. Ни credentials владельца, ни operator age identity не используются.

**Статус реализации не является production acceptance:** до завершения обоих Actions этапов runtime `canvas` всё ещё superuser и публичный gate не снимать.

**Production acceptance 17.11.2026:** двухэтапный переход выполнен на VPS. `prepare` создал `canvas_app`/`canvas_migrator` без cluster privileges, `switch` сохранил private previous env и застейджил app DSN, вторая manual Actions выкладка (backend `2cb29a6`, frontend `4cd22e0`, image `...44113464223-1`) активировала runtime `canvas_app` с mandatory guard. `pg_stat_activity` показывает только `canvas_app`/`canvas` maintenance-соединения, flags у `canvas_app`/`canvas_migrator` все false; readyz healthy. Внешний WAN public suite прошёл дважды 3/3 на новом role.
