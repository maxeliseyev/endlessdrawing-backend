#!/usr/bin/env python3
"""Real pg_dump/pg_restore + age encrypt/decrypt drill in a disposable LOCAL Docker project.

No real Storage Box, SSH credentials, operator age identity or production database is used.
Only this random project's volume and temporary image tag are deleted afterwards.
"""
import argparse
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[2]
TOOLS = "endlessdrawing-local:backup-test-tools"

DRILL = r'''
set -euo pipefail
umask 077
root=$1
[[ $root == /fixture ]] || { echo 'Disposable fixture root required' >&2; exit 1; }
# docker cp may preserve the host UID; the production helper correctly demands root ownership.
chown root:root "$root/config/runtime.env"
chmod 0600 "$root/config/runtime.env"
# Keep test key/config on the container filesystem: Docker Desktop bind mounts remap UIDs.
work=$(mktemp -d /tmp/endlessdrawing-backup-drill-XXXXXXXX)
mkdir -m 0700 "$work/received"
trap 'rm -rf -- "$work"' EXIT
export ENDLESSDRAWING_APP_DIR="$root/app" ENDLESSDRAWING_CONFIG_DIR="$root/config"
export ENDLESSDRAWING_BACKUP_DIR="$work/spool" ENDLESSDRAWING_BACKUP_EXPORT=/workspace/deploy/backup.sh
age-keygen -o "$work/test-only-age-identity" > "$work/keygen.log" 2>&1
recipient=$(age-keygen -y "$work/test-only-age-identity")
printf '%s\n' 'opaque test-only SSH identity: never used for a connection' > "$work/ssh-identity"
chmod 0600 "$work/ssh-identity"
printf '%s\n' 'test-only public host pin: never used for a connection' > "$work/known_hosts"
printf '%s\n' \
    "BACKUP_AGE_RECIPIENT=$recipient" \
    'BACKUP_SSH_HOST=u12345.your-storagebox.de' \
    'BACKUP_SSH_USER=u12345-sub1' \
    "BACKUP_SSH_IDENTITY_FILE=$work/ssh-identity" \
    "BACKUP_SSH_KNOWN_HOSTS_FILE=$work/known_hosts" > "$work/backup.env"
if ! python3 /workspace/deploy/backup-storagebox.py --config "$work/backup.env" --local-only; then
    # This entire project contains disposable fixtures ONLY. Never use this diagnostic on VPS.
    python3 - "$work/spool" <<'PY'
from pathlib import Path
import sys
for file in Path(sys.argv[1]).glob('*.log'):
    print(file.name + ':\n' + file.read_text(errors='replace')[-6000:])
PY
    exit 1
fi
[[ ! -e "$work/spool/last-success.json" ]] # Local-only must never claim off-site success.
[[ -z $(find "$work/spool" -maxdepth 1 -type d -name 'export-*' -print -quit) ]]
mapfile -t ciphers < <(find "$work/spool" -maxdepth 1 -name '*.tar.age' -type f)
[[ ${#ciphers[@]} == 1 ]]
# This newly generated identity is ONLY a test fixture, not the operator's real age key.
age -d -i "$work/test-only-age-identity" -o "$work/recovered.tar" "${ciphers[0]}"
tar -xf "$work/recovered.tar" -C "$work/received"
(cd "$work/received" && sha256sum --check SHA256SUMS > /dev/null)
[[ $(stat -c %s "$work/received/totp-key.bin") == 32 ]] # Encryption key travels only INSIDE age.
IFS= read -r ENDLESSDRAWING_IMAGE < "$root/app/current-image"
export ENDLESSDRAWING_IMAGE
compose() { docker compose --env-file "$root/config/runtime.env" -f "$root/app/compose.yml" "$@"; }
db="ed_readback_$(openssl rand -hex 10)"
[[ $db =~ ^ed_readback_[0-9a-f]{20}$ ]]
compose exec -T postgres createdb -U canvas "$db"
cleanup_db() { compose exec -T postgres dropdb --if-exists --force -U canvas "$db"; rm -rf -- "$work"; }
trap cleanup_db EXIT
compose exec -T postgres pg_restore --exit-on-error --no-owner --no-privileges -U canvas -d "$db" < "$work/received/database.dump"
count=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 -U canvas -d "$db" -c "SELECT count(*) FROM strokes")
[[ $count == 1 ]]
count=$(compose exec -T postgres psql -X -At -v ON_ERROR_STOP=1 -U canvas -d "$db" -c "SELECT count(*) FROM admin_accounts WHERE NOT enabled")
[[ $count == 1 ]]
leftovers=$(compose exec -T postgres psql -X -At -U canvas -d canvas -c "SELECT count(*) FROM pg_database WHERE datname LIKE 'ed_verify_%'")
[[ $leftovers == 0 ]]
# Source data remains unchanged: neither of the restore drills targeted the application DB.
count=$(compose exec -T postgres psql -X -At -U canvas -d canvas -c "SELECT count(*) FROM strokes")
[[ $count == 1 ]]
# Real ownership transfer, migration SCRAM login and app TCP login; no fixture secrets logged.
python3 /workspace/deploy/db-roles.py prepare
compose run --rm --no-deps migrate
python3 /workspace/deploy/db-roles.py switch
compose up -d --no-deps --force-recreate --wait --wait-timeout 120 backend
if compose exec -T backend /app/canvas-migrate > "$work/runtime-migration-denied.log" 2>&1; then
    echo 'ERROR: runtime unexpectedly migrated schema' >&2; exit 1
fi
python3 /workspace/deploy/db-roles.py status
# Maintenance CREATEDB/restore still works after app loses cluster/DDL privileges.
python3 /workspace/deploy/backup-storagebox.py --config "$work/backup.env" --local-only > "$work/restricted-backup.log"
printf '%s\n' 'Real backup/role drill passed: dump/restore, age round-trip, checksums/readback, isolated migrator, restricted runtime, denied runtime DDL and maintenance restore; no off-site success claimed.'
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", help="built LOCAL application image containing current migrations")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9./:@_-]+", args.image):
        parser.error("pass a reviewed local image reference, without whitespace or Dockerfile syntax")
    project = "endlessdrawing-backup-smoke-" + uuid.uuid4().hex[:12]
    image = "endlessdrawing-app:" + "a" * 40 + "-" + str(secrets.randbits(64)) + "-1"
    subprocess.run(["docker", "build", "-f", str(ROOT / "tests/deploy/backup-tools.Dockerfile"), "-t", TOOLS, str(ROOT)], check=True)
    # Keep bind sources under the shared checkout (macOS private /var may be unshared).
    (ROOT / 'target').mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="endlessdrawing-backup-smoke-", dir=ROOT / 'target') as temporary:
        root = Path(temporary).resolve()
        app, config = root / "app", root / "config"
        app.mkdir(); config.mkdir()
        secret_dir=app/'admin-secrets'; secret_dir.mkdir()
        # Public, deterministic TEST fixture, not a real server encryption key.
        (secret_dir/'totp.key').write_bytes(b'U'*32)
        (secret_dir/'totp.key').chmod(0o644)
        dockerfile = root / "Dockerfile"
        dockerfile.write_text(f'FROM {args.image}\nLABEL io.endlessdrawing.frontend-revision="' + "b" * 40 + '"\n')
        subprocess.run(["docker", "build", "-t", image, str(root)], check=True)
        compose_text = (ROOT / "deploy/compose.yml").read_text()
        assert compose_text.count("name: endlessdrawing\n") == 1
        compose_text = compose_text.replace("name: endlessdrawing\n", f"name: {project}\n", 1)
        compose_text = compose_text.replace('"127.0.0.1:8080:3000"', '"127.0.0.1::3000"')
        (app / "compose.yml").write_text(compose_text)
        (app / "current-image").write_text(image + "\n")
        password = secrets.token_hex(32)
        (config / "runtime.env").write_text(
            "POSTGRES_DB=canvas\nPOSTGRES_USER=canvas\n" + f"POSTGRES_PASSWORD={password}\n" +
            f"DATABASE_URL=postgres://canvas:{password}@postgres:5432/canvas\n" +
            f"MIGRATION_DATABASE_URL=postgres://canvas:{password}@postgres:5432/canvas\n" +
            "ALLOWED_ORIGINS=http://localhost:8080\nADMIN_ENABLED=false\n" + f"ADMIN_SECRETS_DIR={secret_dir}\n"
        )
        (config / "runtime.env").chmod(0o600)
        env = dict(os.environ)
        for key in ("POSTGRES_DB", "POSTGRES_USER", "POSTGRES_PASSWORD", "DATABASE_URL", "ALLOWED_ORIGINS", "ADMIN_ENABLED", "ADMIN_ORIGIN", "ADMIN_RP_ID"):
            env.pop(key, None)
        env.pop('MIGRATION_DATABASE_URL', None)
        env['DATABASE_MIGRATE_ON_START'] = 'false'
        env['DATABASE_REQUIRE_RUNTIME_ROLE'] = 'false'
        env['ADMIN_SECRETS_DIR'] = str(secret_dir)
        env['ADMIN_TOTP_KEY_FILE'] = ''
        env["ENDLESSDRAWING_IMAGE"] = image
        command = ["docker", "compose", "-p", project, "--env-file", str(config / "runtime.env"), "-f", str(app / "compose.yml")]
        try:
            subprocess.run(command + ["up", "-d", "--wait", "--wait-timeout", "120", "postgres"], env=env, check=True)
            subprocess.run(command + ["run", "--rm", "--no-deps", "migrate"], env=env, check=True)
            subprocess.run(command + ["up", "-d", "--no-deps", "--wait", "--wait-timeout", "120", "backend"], env=env, check=True)
            seed = """INSERT INTO strokes(id,owner_digest,points,brush,width,color,opacity,bounds)
            VALUES(gen_random_uuid(),decode(repeat('00',32),'hex'),'[[0,0],[1,1]]','round',4,'#123456',1,box(point(-3,-3),point(4,4)));
            INSERT INTO admin_accounts(id,login,password_hash,enabled) VALUES(gen_random_uuid(),'backup-fixture','disabled-test-fixture-no-login',false);"""
            subprocess.run(command + ["exec", "-T", "postgres", "psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-U", "canvas", "-d", "canvas"], env=env, input=seed.encode(), check=True)
            # Copy fixture config instead of bind-mounting macOS 0600 files: Docker Desktop
            # UID remapping can make a host-private runtime.env unreadable to container root.
            tools = subprocess.check_output(["docker", "create", "-v", "/var/run/docker.sock:/var/run/docker.sock",
                "-v", f"{ROOT}:/workspace:ro", TOOLS, "bash", "-ceu", DRILL, "--", "/fixture"], text=True).strip()
            try:
                subprocess.run(["docker", "cp", str(root) + "/.", tools + ":/fixture"], check=True)
                subprocess.run(["docker", "start", "--attach", tools], check=True)
            finally:
                subprocess.run(["docker", "rm", "--force", tools], check=True, stdout=subprocess.DEVNULL)
        finally:
            subprocess.run(command + ["down", "--volumes", "--timeout", "20"], env=env, check=True)
            subprocess.run(["docker", "image", "rm", image], check=True, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
