#!/usr/bin/env bash
# Local export + isolated restore verification. NOT an off-site uploader or production restore.
# Installed root-owned; runtime.env stays server-only. Never source it or print resolved Compose.
set -euo pipefail
umask 077

app_dir=${ENDLESSDRAWING_APP_DIR:-/opt/endlessdrawing}
config_dir=${ENDLESSDRAWING_CONFIG_DIR:-/etc/endlessdrawing}
spool=${ENDLESSDRAWING_BACKUP_DIR:-/var/lib/endlessdrawing-backup}
operation=${1:-}
fail() { printf '%s\n' "$*" >&2; exit 1; }
[[ $operation == export && $# == 1 || $operation == verify && $# == 2 ]] || fail 'Usage: endlessdrawing-backup export | verify LOCAL_BUNDLE'
[[ -r "$app_dir/compose.yml" && -r "$config_dir/runtime.env" && -r "$app_dir/current-image" ]] || fail 'Deployment/configuration is unavailable'
IFS= read -r image < "$app_dir/current-image"
[[ $image =~ ^endlessdrawing-app:[0-9a-f]{40}-[0-9]+-[0-9]+$ ]] || fail 'Invalid current-image state'
compose() {
    ENDLESSDRAWING_IMAGE="$image" docker compose --env-file "$config_dir/runtime.env" -f "$app_dir/compose.yml" "$@"
}
exec 9>"$app_dir/deploy.lock"
flock -n 9 || fail 'A deployment or backup operation is active; retry later'
compose config --quiet
install -d -m 0700 "$spool"
spool=$(cd -- "$spool" && pwd -P)
log=$(mktemp "$spool/operation-XXXXXXXX.log")
verify_db=
bundle=
complete=false
cleanup() {
    if [[ -n $verify_db ]]; then
        # Name is generated locally, never comes from the archive or caller. No production DB drop.
        compose exec -T postgres sh -c 'exec dropdb --if-exists --force -U "$POSTGRES_USER" --maintenance-db=postgres "$1"' sh "$verify_db" >> "$log" 2>&1 </dev/null || {
            printf '%s\n' 'Could not remove the isolated verification DB; inspect the private operation log' >&2
            return 1
        }
    fi
    if [[ $complete == true ]]; then rm -f -- "$log"; fi
    if [[ $complete != true && $operation == export && -n $bundle ]]; then
        # Retain private failed output for diagnosis, but never mark it uploadable.
        printf '%s\n' 'Export failed; incomplete private bundle retained without READY marker' >&2
    fi
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# Keep capacity for WAL/application data. Restore verification needs another database copy.
database_bytes=$(compose exec -T postgres sh -c 'exec psql -X -At -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "SELECT pg_database_size(current_database())"' </dev/null 2>> "$log")
[[ $database_bytes =~ ^[0-9]{1,15}$ ]] || fail 'Cannot determine database size; see private operation log'
multiplier=1
if [[ $operation == verify ]]; then multiplier=2; fi
available_kib=$(df -Pk "$spool" | awk 'NR == 2 { print $4 }')
required_kib=$((database_bytes * multiplier / 1024 + 1 + 2097152))
[[ $available_kib =~ ^[0-9]+$ && $available_kib -ge $required_kib ]] || fail 'Insufficient disk headroom; backup/verification cancelled'

if [[ $operation == export ]]; then
    bundle=$(mktemp -d "$spool/export-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXXXX")
    frontend=$(docker image inspect "$image" -f '{{index .Config.Labels "io.endlessdrawing.frontend-revision"}}' </dev/null 2>> "$log")
    [[ $frontend =~ ^[0-9a-f]{40}$ ]] || fail 'Cannot identify frontend revision; see private operation log'
    if ! compose exec -T postgres sh -c 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom --no-owner --no-privileges' </dev/null > "$bundle/database.dump" 2>> "$log"; then
        fail 'pg_dump failed; see private operation log'
    fi
    [[ -s "$bundle/database.dump" ]] || fail 'Database export is empty'
    compose exec -T postgres pg_restore --list < "$bundle/database.dump" >> "$log" 2>&1 || fail 'Export archive is unreadable; see private operation log'
    printf '{"format":1,"postgres_major":17,"image":"%s","frontend_sha":"%s","database_bytes":%s,"created_utc":"%s"}\n' \
        "$image" "$frontend" "$database_bytes" "$(date -u +%FT%TZ)" > "$bundle/manifest.json"
    (cd -- "$bundle" && sha256sum database.dump manifest.json > SHA256SUMS)
    printf '%s\n' 'Local export only; NOT yet uploaded or off-site backed up' >&2
    # Last marker makes a completed export distinguishable from failed/partial output.
    printf '%s\n' 'local-export-v1' > "$bundle/READY"
    complete=true
    printf '%s\n' "$bundle"
    exit 0
fi

[[ -d $2 && ! -L $2 ]] || fail 'Verification requires a local export bundle, not a symlink'
bundle=$(cd -- "$2" && pwd -P)
[[ $bundle == "$spool"/export-* ]] || fail 'Only bundles in the private backup spool can be verified'
for file in READY database.dump manifest.json SHA256SUMS; do
    [[ -f "$bundle/$file" && ! -L "$bundle/$file" ]] || fail 'Bundle is incomplete or contains symlinks'
done
# A new failed drill must not leave an old verification-success marker behind.
rm -f -- "$bundle/RESTORE_VERIFIED"
# Recompute a fixed list. Do not allow an untrusted checksum list to read arbitrary paths.
(cd -- "$bundle" && sha256sum database.dump manifest.json) > "$log.checksums"
if ! cmp -s "$log.checksums" "$bundle/SHA256SUMS"; then
    rm -f -- "$log.checksums"
    fail 'Bundle checksum mismatch; verification cancelled'
fi
rm -f -- "$log.checksums"
IFS= read -r metadata < "$bundle/manifest.json"
[[ $metadata =~ \"database_bytes\":([0-9]{1,15})[,}] ]] || fail 'Bundle database size metadata is missing or invalid'
archived_bytes=${BASH_REMATCH[1]}
if (( archived_bytes > database_bytes )); then database_bytes=$archived_bytes; fi
required_kib=$((database_bytes * 2 / 1024 + 1 + 2097152))
[[ $available_kib -ge $required_kib ]] || fail 'Insufficient headroom for the archived database size'
suffix=$(openssl rand -hex 12)
[[ $suffix =~ ^[0-9a-f]{24}$ ]] || fail 'Cannot generate isolated database name'
candidate="ed_verify_$suffix"
compose exec -T postgres sh -c 'exec createdb -U "$POSTGRES_USER" --maintenance-db=postgres "$1"' sh "$candidate" >> "$log" 2>&1 </dev/null || fail 'Cannot create isolated verification database; see private operation log'
# Only a successfully created DB is eligible for cleanup; never drop a pre-existing name.
verify_db=$candidate
# timeout must execute an external command, not the shell compose function.
if ! ENDLESSDRAWING_IMAGE="$image" timeout --foreground 1800 docker compose --env-file "$config_dir/runtime.env" -f "$app_dir/compose.yml" \
    exec -T -e 'PGOPTIONS=-c statement_timeout=600000 -c lock_timeout=5000' postgres sh -c \
    'exec pg_restore --exit-on-error --no-owner --no-privileges -U "$POSTGRES_USER" --dbname="$1"' sh "$verify_db" \
    < "$bundle/database.dump" >> "$log" 2>&1; then
    fail 'Isolated pg_restore failed or timed out; see private operation log'
fi
# Validate expected relations and all migration results without printing private drawing/audit data.
if ! compose exec -T postgres sh -c 'exec psql -X -q -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$1"' sh "$verify_db" >> "$log" 2>&1 <<'SQL'
SELECT id, seq, points, bounds, deleted_at FROM strokes LIMIT 0;
SELECT id, login, auth_version FROM admin_accounts LIMIT 0;
SELECT id, actor_id, result FROM moderation_operations LIMIT 0;
DO $$ BEGIN
  IF EXISTS (SELECT 1 FROM _sqlx_migrations WHERE NOT success)
     OR NOT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 2 AND success) THEN
    RAISE EXCEPTION 'invalid restored migration state';
  END IF;
END $$;
SQL
then
    fail 'Restored schema validation failed; see private operation log'
fi
# Explicit cleanup must succeed before reporting a successful restore drill.
compose exec -T postgres sh -c 'exec dropdb --if-exists --force -U "$POSTGRES_USER" --maintenance-db=postgres "$1"' sh "$verify_db" >> "$log" 2>&1 </dev/null || fail 'Cannot remove isolated verification database; see private operation log'
verify_db=
printf '%s\n' "$(date -u +%FT%TZ)" > "$bundle/RESTORE_VERIFIED"
complete=true
printf '%s\n' 'Restore verification passed in a disposable database; production database was not restored'
