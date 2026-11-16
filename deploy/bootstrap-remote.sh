#!/usr/bin/env bash
# Run manually after reviewing bootstrap.sh. Never sends your private SSH key.
set -euo pipefail
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
identity=${SSH_IDENTITY:-$HOME/.ssh/stokholm}
target=${1:-subwhale@212.147.244.161}
[[ $target =~ ^[a-z_][a-z0-9_-]*@[a-zA-Z0-9.-]+$ ]] || { printf '%s\n' 'Invalid SSH target' >&2; exit 1; }
[[ -f $identity ]] || { printf '%s\n' 'Set SSH_IDENTITY to the private key path' >&2; exit 1; }
printf '%s\n' 'This installs Docker/Compose and root-owned deployment files on the VPS.'
printf '%s\n' 'It does not start the app, open ports, change SSH or publish the site.'
COPYFILE_DISABLE=1 tar --no-xattrs -C "$source_dir" -czf - bootstrap.sh activate.sh compose.yml | \
    ssh -i "$identity" -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 "$target" \
    'set -eu; temporary=$(mktemp -d); trap '\''rm -rf -- "$temporary"'\'' EXIT; tar -xz -C "$temporary"; sudo -n bash "$temporary/bootstrap.sh"'
