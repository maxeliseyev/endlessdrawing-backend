#!/usr/bin/env bash
# Install the off-VPS dead-man on a SEPARATE always-on host (not the app VPS).
# Does not create Telegram credentials and does not enable the timer until the
# owner runs the interactive configure command.
set -euo pipefail
[[ $EUID == 0 ]] || { printf '%s\n' 'Run as root on the separate monitor host' >&2; exit 1; }
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
install -o root -g root -m 0755 "$source_dir/deadman-monitor.py" /usr/local/sbin/endlessdrawing-deadman
install -o root -g root -m 0644 "$source_dir/deadman/endlessdrawing-deadman.service" /etc/systemd/system/
install -o root -g root -m 0644 "$source_dir/deadman/endlessdrawing-deadman.timer" /etc/systemd/system/
systemctl daemon-reload
printf '%s\n' \
    'Dead-man installed. Run the interactive configure as root in your own TTY:' \
    '  sudo /usr/local/sbin/endlessdrawing-deadman configure --chat-id <numeric-chat-id>' \
    'The token is entered hidden (getpass), a test message is sent, and only after' \
    'you type yes the config is saved and the timer enabled.'
