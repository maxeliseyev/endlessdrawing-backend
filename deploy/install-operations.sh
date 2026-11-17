#!/usr/bin/env bash
# Explicit HOST preparation only. Does not start backups/Caddy, open ports or contact Storage Box.
set -euo pipefail
umask 077
[[ $EUID == 0 ]] || { printf '%s\n' 'Run with sudo on the VPS'; exit 1; }
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
[[ -d /opt/endlessdrawing && -f /etc/endlessdrawing/runtime.env ]] || { printf '%s\n' 'Run the application bootstrap first'; exit 1; }
if systemctl is-active --quiet endlessdrawing-backup.service; then
    printf '%s\n' 'A backup is active; retry after it finishes' >&2
    exit 1
fi
exec 9>/opt/endlessdrawing/deploy.lock
flock -n 9 || { printf '%s\n' 'Deployment/backup is active; retry later' >&2; exit 1; }
[[ -z $(dpkg --audit) ]] || { printf '%s\n' 'Repair the package database before operations setup' >&2; exit 1; }
export DEBIAN_FRONTEND=noninteractive
packages=()
for pair in age:age python3:python3 ssh:openssh-client sha256sum:coreutils flock:util-linux openssl:openssl tar:tar; do
    command -v "${pair%%:*}" >/dev/null || packages+=("${pair#*:}")
done
if (( ${#packages[@]} )); then
    apt-get update
    apt-get install -y --no-install-recommends --no-upgrade "${packages[@]}"
fi
install -d -o root -g root -m 0700 /etc/endlessdrawing/backup /var/lib/endlessdrawing-backup /var/lib/endlessdrawing-monitor
install -o root -g root -m 0755 "$source_dir/backup.sh" /usr/local/sbin/endlessdrawing-backup-export
install -o root -g root -m 0755 "$source_dir/backup-storagebox.py" /usr/local/sbin/endlessdrawing-backup
install -o root -g root -m 0755 "$source_dir/backup-health.py" /usr/local/sbin/endlessdrawing-backup-health
install -o root -g root -m 0755 "$source_dir/monitor.py" /usr/local/sbin/endlessdrawing-monitor
install -o root -g root -m 0755 "$source_dir/db-roles.py" /usr/local/sbin/endlessdrawing-db-roles
if [[ ! -e /etc/endlessdrawing/backup.env ]]; then
    install -o root -g root -m 0600 "$source_dir/backup.env.example" /etc/endlessdrawing/backup.env
fi
for unit in endlessdrawing-backup.service endlessdrawing-backup.timer endlessdrawing-monitor.service endlessdrawing-monitor.timer; do
    install -o root -g root -m 0644 "$source_dir/systemd/$unit" "/etc/systemd/system/$unit"
done
# Prepared snippets only. No replacement of /etc/caddy/Caddyfile, service reload, or ACME action.
install -d -o root -g root -m 0755 /etc/endlessdrawing/caddy
install -o root -g root -m 0644 "$source_dir/caddy/Caddyfile" /etc/endlessdrawing/caddy/Caddyfile.prepared
install -o root -g root -m 0644 "$source_dir/caddy/access.private.caddy" /etc/endlessdrawing/caddy/access.private.prepared
install -o root -g root -m 0644 "$source_dir/caddy/service.conf" /etc/endlessdrawing/caddy/service.conf.prepared
if [[ ! -e /etc/endlessdrawing/caddy/caddy.env ]]; then
    install -o root -g root -m 0600 "$source_dir/caddy/caddy.env.example" /etc/endlessdrawing/caddy/caddy.env
fi
systemctl daemon-reload
printf '%s\n' \
    'Operations tools installed; no timers, uploads, Caddy or public ports were enabled.' \
    'Configure the dedicated Storage Box subaccount, age PUBLIC recipient, verified host key and SSH identity.' \
    'Run a real backup, operator-side decrypt/restore drill and health check before enabling the timer.' \
    'Telegram monitor is installed but not configured/enabled. Configure only in the owner trusted TTY; never paste its token into chat.'
