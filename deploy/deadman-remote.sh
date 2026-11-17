#!/usr/bin/env bash
# Install the off-VPS dead-man on a SEPARATE always-on host over verified SSH.
# Installs helper/units only; Telegram configure + timer enable are interactive.
set -euo pipefail
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
identity=${SSH_IDENTITY:-$HOME/.ssh/stokholm}
target=${1:-}
[[ $target =~ ^[a-z_][a-z0-9_-]*@[a-zA-Z0-9.-]+$ ]] || { printf '%s\n' 'Usage: deadman-remote.sh user@host (set SSH_IDENTITY to the key)' >&2; exit 1; }
[[ -f $identity ]] || { printf '%s\n' 'Set SSH_IDENTITY to the private key path' >&2; exit 1; }
printf '%s\n' 'Installs the dead-man helper/units on the separate host. No Telegram token is sent or enabled.'
COPYFILE_DISABLE=1 tar --no-xattrs -C "$source_dir" -czf - \
    deadman-monitor.py install-deadman.sh deadman/endlessdrawing-deadman.service deadman/endlessdrawing-deadman.timer | \
    ssh -i "$identity" -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 "$target" \
    'set -eu; temporary=$(mktemp -d); trap '\''rm -rf -- "$temporary"'\'' EXIT; tar -xz -C "$temporary"; sudo -n bash "$temporary/install-deadman.sh"'
printf '%s\n' 'Installed. Now configure interactively (token is hidden; not through agent/CI):'
printf '%s\n' "  ssh -t -i $identity $target 'sudo -n /usr/local/sbin/endlessdrawing-deadman configure --chat-id <numeric-chat-id>'"
