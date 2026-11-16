#!/usr/bin/env bash
# Explicit one-time VPS preparation. Run as root on Ubuntu, not on the developer laptop.
set -euo pipefail
umask 077

[[ $EUID == 0 ]] || { printf '%s\n' 'Run with sudo on the VPS' >&2; exit 1; }
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
app_dir=/opt/endlessdrawing
config_dir=/etc/endlessdrawing
install -d -m 0750 "$app_dir"
exec 9>"$app_dir/deploy.lock"
flock -n 9 || { printf '%s\n' 'Deployment is running; retry later' >&2; exit 1; }

# Use Ubuntu packages, not a remote curl | sh installer. Does not alter SSH/firewall.
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends docker.io docker-compose-v2 openssl ca-certificates curl util-linux
systemctl enable --now docker

docker compose version
install -d -m 0700 "$config_dir" "$app_dir/backups"
install -o root -g root -m 0644 "$source_dir/compose.yml" "$app_dir/compose.yml"
install -o root -g root -m 0755 "$source_dir/activate.sh" /usr/local/sbin/endlessdrawing-activate

if [[ ! -e "$config_dir/runtime.env" ]]; then
    password=$(openssl rand -hex 32)
    # Hex avoids ambiguity in both dotenv and PostgreSQL URL encoding.
    {
        printf '%s\n' 'POSTGRES_DB=canvas' 'POSTGRES_USER=canvas'
        printf 'POSTGRES_PASSWORD=%s\n' "$password"
        printf 'DATABASE_URL=postgres://canvas:%s@postgres:5432/canvas\n' "$password"
        printf '%s\n' 'ALLOWED_ORIGINS=http://localhost:8080,http://127.0.0.1:8080' 'RUST_LOG=info'
    } > "$config_dir/runtime.env"
    chmod 0600 "$config_dir/runtime.env"
    unset password
    printf '%s\n' 'Generated server-only PostgreSQL credentials (not printed)'
else
    printf '%s\n' 'Existing runtime.env preserved; PostgreSQL passwords were not rotated'
fi
chown root:root "$config_dir/runtime.env"
chmod 0600 "$config_dir/runtime.env"
printf '%s\n' \
    'Bootstrap complete. No application or public reverse proxy has been started.' \
    'Next: configure GitHub staging environment and run Deploy staging in Actions.' \
    'Do not expose the app publicly until moderation, TLS and external backups are ready.'
