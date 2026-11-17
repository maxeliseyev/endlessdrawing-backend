#!/usr/bin/env bash
# Root-owned helper on the VPS. Never source runtime.env or print resolved config.
set -euo pipefail
umask 077

app_dir=${ENDLESSDRAWING_APP_DIR:-/opt/endlessdrawing}
config_dir=${ENDLESSDRAWING_CONFIG_DIR:-/etc/endlessdrawing}
operation=${1:-}
release=${2:-}
image=endlessdrawing-app:preflight

fail() { printf '%s\n' "$*" >&2; exit 1; }
valid_image() { [[ $1 =~ ^endlessdrawing-app:[0-9a-f]{40}-[0-9]+-[0-9]+$ ]]; }
compose() {
    ENDLESSDRAWING_IMAGE="$image" docker compose --env-file "$config_dir/runtime.env" \
        -f "$app_dir/compose.yml" "$@"
}

[[ -f "$app_dir/compose.yml" && -f "$config_dir/runtime.env" ]] || fail 'Run deploy/bootstrap.sh on the VPS first'
command -v docker >/dev/null || fail 'Docker is not installed'
command -v flock >/dev/null || fail 'flock is not installed'
exec 9>"$app_dir/deploy.lock"
flock -n 9 || fail 'Another deployment is running; retry after it finishes'
docker info >/dev/null

case "$operation" in
    check)
        [[ $# == 1 ]] || fail 'Usage: endlessdrawing-activate check'
        compose config --quiet
        printf '%s\n' 'Deployment prerequisites are ready; private staging uses 127.0.0.1:8080'
        exit 0
        ;;
    deploy|rollback)
        [[ $# == 2 && $release =~ ^[0-9a-f]{40}-[0-9]+-[0-9]+$ ]] || fail 'Invalid immutable release identifier'
        image="endlessdrawing-app:$release"
        ;;
    *) fail 'Usage: endlessdrawing-activate check | deploy RELEASE | rollback RELEASE' ;;
esac

# Leave headroom for PostgreSQL/WAL, a backup and a second image. No automatic pruning.
available_kib=$(df -Pk "$app_dir" | awk 'NR == 2 { print $4 }')
[[ $available_kib =~ ^[0-9]+$ && $available_kib -ge 2097152 ]] || fail 'Less than 2 GiB free; inspect disk usage before deploying'
compose config --quiet
previous=
if [[ -f "$app_dir/current-image" ]]; then
    IFS= read -r previous < "$app_dir/current-image"
    valid_image "$previous" || fail 'Invalid current-image state; inspect the VPS'
fi

if [[ $operation == deploy ]]; then
    # CI streams a gzip-compressed docker save archive over verified SSH, not credentials.
    docker image load
fi
docker image inspect "$image" >/dev/null
if [[ -n $previous ]]; then docker image inspect "$previous" >/dev/null; fi
compose up -d --wait --wait-timeout 90 postgres

# A dump can be as large as the database; do not consume PostgreSQL's remaining space.
database_bytes=$(compose exec -T postgres sh -c 'exec psql -X -At -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "SELECT pg_database_size(current_database())"')
[[ $database_bytes =~ ^[0-9]+$ ]] || fail 'Could not determine database size; activation cancelled'
available_kib=$(df -Pk "$app_dir" | awk 'NR == 2 { print $4 }')
required_kib=$((database_bytes / 1024 + 1 + 2097152))
[[ $available_kib =~ ^[0-9]+$ && $available_kib -ge $required_kib ]] || fail 'Insufficient headroom for database backup plus 2 GiB reserve; activation cancelled'

# Always snapshot the DB before the new binary can run its automatic migrations.
backup_dir="$app_dir/backups"
install -d -m 0700 "$backup_dir"
backup_tmp=$(mktemp "$backup_dir/pre-${operation}-${release}-XXXXXXXX.dump.tmp")
backup=${backup_tmp%.tmp}
trap 'rm -f -- "$backup_tmp"' EXIT
compose exec -T postgres sh -c 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom' > "$backup_tmp"
[[ -s $backup_tmp ]] || fail 'Database backup is empty; activation cancelled'
# Check archive readability. This does NOT replace a test restore or an off-VPS backup.
compose exec -T postgres pg_restore --list < "$backup_tmp" > /dev/null
mv -- "$backup_tmp" "$backup"

# Maintenance credentials reach ONLY the short-lived migration service, never backend.
# Leave the old app untouched if migration/manifest verification fails.
migration_log="$backup.migrate.log"
if ! compose run --rm --no-deps migrate > "$migration_log" 2>&1; then
    fail 'Maintenance migration failed before backend replacement; inspect the private migration log. Current backend left untouched.'
fi
rm -f -- "$migration_log"

if compose up -d --no-deps --wait --wait-timeout 120 backend; then
    if [[ -n $previous && $previous != "$image" ]]; then
        printf '%s\n' "$previous" > "$app_dir/previous-image.tmp"
        mv -- "$app_dir/previous-image.tmp" "$app_dir/previous-image"
    fi
    printf '%s\n' "$image" > "$app_dir/current-image.tmp"
    mv -- "$app_dir/current-image.tmp" "$app_dir/current-image"
    printf '%s\n' "Healthy release activated: $image" "Local pre-deploy backup: $backup"
    compose ps
    exit 0
fi

printf '%s\n' 'New backend failed readiness; deployment is FAILED' >&2
if [[ -n $previous ]]; then
    image="$previous"
    if compose up -d --no-deps --wait --wait-timeout 120 backend; then
        printf '%s\n' "Previous image recovered: $previous; no database restore was performed" >&2
    else
        printf '%s\n' 'Previous image also failed; operator intervention required (schema may have changed)' >&2
        compose stop backend || true
    fi
else
    compose stop backend || true
    printf '%s\n' 'No previous release exists; backend left stopped' >&2
fi
printf '%s\n' 'Backups are preserved. Never restore automatically: doing so can lose confirmed strokes.' >&2
exit 1
