#!/usr/bin/env bash
# Explicit operator preparation. Never prints key bytes or changes credentials/sessions.
set -euo pipefail
umask 077
[[ $EUID == 0 ]] || { printf '%s\n' 'Run with sudo on the application VPS'; exit 1; }
[[ -f /etc/endlessdrawing/runtime.env && -d /opt/endlessdrawing ]] || exit 1
exec 9>/opt/endlessdrawing/deploy.lock
flock -n 9 || { printf '%s\n' 'Deployment/backup active; retry later'; exit 1; }
python3 - <<'PY'
from pathlib import Path
import os
import secrets
folder=Path('/opt/endlessdrawing/admin-secrets')
folder.mkdir(mode=0o700,exist_ok=True)
assert not folder.is_symlink()
key=folder/'totp.key'
if not key.exists():
    # Stop for manual recovery rather than silently replacing an encryption key.
    assert not Path('/var/lib/endlessdrawing-backup/totp-key-initialized').exists(), 'Restore the original key; do not regenerate it'
    with key.open('xb') as file: file.write(secrets.token_bytes(32))
assert key.is_file() and not key.is_symlink() and key.stat().st_size==32
os.chown(key,10001,10001); key.chmod(0o400)
os.chown(folder,10001,10001); folder.chmod(0o500)
marker=Path('/var/lib/endlessdrawing-backup/totp-key-initialized')
marker.parent.mkdir(mode=0o700,exist_ok=True)
marker.touch(mode=0o600)
p=Path('/etc/endlessdrawing/runtime.env')
values={'ADMIN_SECRETS_DIR':str(folder),'ADMIN_TOTP_KEY_FILE':'/run/admin-secrets/totp.key'}
text=p.read_text(); seen=set(); lines=[]
for line in text.splitlines():
    key=line.partition('=')[0]
    if key in values:
        assert key not in seen
        seen.add(key); lines.append(key+'='+values[key])
    else: lines.append(line)
for key in values.keys()-seen: lines.append(key+'='+values[key])
p.write_text('\n'.join(lines)+'\n'); p.chmod(0o600)
print('Private TOTP encryption file prepared; only file paths were added to runtime.env. No key value was printed.')
PY
