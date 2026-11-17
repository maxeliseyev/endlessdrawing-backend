#!/usr/bin/env python3
"""Operator-only age decrypt + isolated PostgreSQL restore, outside the application VPS.

The age identity is passed to the native age CLI, never read/printed by this script.
No production restore, host directory mounts, container network or published ports.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid

MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_DATABASE_BYTES = 256 * 1024 * 1024


class VerificationError(Exception):
    pass


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def extract_payload(archive, directory):
    expected = {'database.dump', 'manifest.json', 'SHA256SUMS'}
    with tarfile.open(archive, mode='r:') as source:
        members = source.getmembers()
        if len(members) != 3 or {item.name for item in members} != expected:
            raise VerificationError('Unexpected or duplicate archive entries')
        if any(not item.isfile() or item.size < 0 for item in members):
            raise VerificationError('Archive links and special files are forbidden')
        if sum(item.size for item in members) > MAX_ARCHIVE_BYTES:
            raise VerificationError('Archive exceeds this small-backup drill limit')
        for item in members:
            if item.name != 'database.dump' and item.size > 64 * 1024:
                raise VerificationError('Archive metadata is oversized')
            with source.extractfile(item) as data, (directory / item.name).open('xb') as output:
                shutil.copyfileobj(data, output, 1024 * 1024)
    manifest = json.loads((directory / 'manifest.json').read_text())
    if (manifest.get('format') != 1 or manifest.get('postgres_major') != 17
            or type(manifest.get('database_bytes')) is not int
            or not 0 < manifest['database_bytes'] <= MAX_DATABASE_BYTES):
        raise VerificationError('Unsupported format, PostgreSQL version or database size; plan a larger isolated drill explicitly')
    with (directory / 'database.dump').open('rb') as file:
        if file.read(5) != b'PGDMP':
            raise VerificationError('Not a custom PostgreSQL dump')
    checksums = ''.join(f'{digest(directory / name)}  {name}\n' for name in ['database.dump', 'manifest.json'])
    if checksums != (directory / 'SHA256SUMS').read_text():
        raise VerificationError('Payload checksum mismatch')
    return manifest


def local_endpoint(endpoint):
    return endpoint.startswith('unix://')


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--identity', type=Path, required=True, help='private age identity FILE, never its contents')
    parser.add_argument('--ciphertext', type=Path, required=True)
    parser.add_argument('--sha256', required=True, help='trusted digest obtained independently from the application VPS')
    parser.add_argument('--report', type=Path, required=True, help='new nonsensitive JSON proof file, not plaintext data')
    args = parser.parse_args()
    if args.ciphertext.stat().st_size > MAX_ARCHIVE_BYTES + 1024 * 1024:
        raise VerificationError('Ciphertext exceeds this bounded operator drill limit')
    if shutil.disk_usage(tempfile.gettempdir()).free < args.ciphertext.stat().st_size * 2 + 512 * 1024 * 1024:
        raise VerificationError('Insufficient private temporary disk headroom')
    if not re.fullmatch('[0-9a-f]{64}', args.sha256) or digest(args.ciphertext) != args.sha256:
        raise VerificationError('Ciphertext does not match the trusted checksum; age was not invoked')
    identity = args.identity.stat()
    if not stat.S_ISREG(identity.st_mode) or identity.st_uid != os.getuid() or identity.st_mode & 0o077:
        raise VerificationError('Identity file must be operator-owned and private (0600/0400)')
    if args.report.exists():
        raise VerificationError('Report already exists; refusing to overwrite it')
    endpoint = os.environ.get('DOCKER_HOST')
    if os.environ.get('DOCKER_CONTEXT') or not endpoint:
        endpoint = subprocess.check_output(['docker', 'context', 'inspect', '--format', '{{.Endpoints.docker.Host}}'], text=True).strip()
    if not local_endpoint(endpoint):
        raise VerificationError('Use a local Unix Docker endpoint, not SSH/TCP or an application VPS context')
    name = 'endlessdrawing-operator-restore-' + uuid.uuid4().hex[:16]
    created = False
    with tempfile.TemporaryDirectory(prefix='endlessdrawing-private-restore-') as temporary:
        directory = Path(temporary)
        payload = directory / 'payload'
        payload.mkdir(mode=0o700)
        with (directory / 'private-operation.log').open('xb') as log:
            def run(command, **options):
                result = subprocess.run(command, stderr=log, stdout=subprocess.PIPE, timeout=180, **options)
                if result.returncode:
                    raise VerificationError('Isolated restore/decryption operation failed; no production database was touched')
                return result.stdout
            run(['age', '--decrypt', '--identity', str(args.identity), '--output', str(directory / 'payload.tar'), str(args.ciphertext)], stdin=subprocess.DEVNULL)
            manifest = extract_payload(directory / 'payload.tar', payload)
            env_file = directory / 'postgres.env'
            env_file.write_text('POSTGRES_USER=restore_operator\nPOSTGRES_DB=restore_candidate\nPOSTGRES_PASSWORD=' + secrets.token_hex(32) + '\n')
            try:
                run(['docker', 'create', '--name', name, '--network', 'none', '--read-only',
                     '--security-opt', 'no-new-privileges:true', '--memory', '768m', '--cpus', '1', '--pids-limit', '128',
                     '--tmpfs', '/var/lib/postgresql/data:rw,nosuid,nodev,size=512m',
                     '--tmpfs', '/var/run/postgresql:rw,nosuid,nodev,size=16m',
                     '--tmpfs', '/tmp:rw,nosuid,nodev,size=32m', '--env-file', str(env_file),
                     'postgres:17-alpine', 'postgres', '-c', 'shared_buffers=32MB', '-c', 'max_connections=10'], stdin=subprocess.DEVNULL)
                created = True
                run(['docker', 'start', name], stdin=subprocess.DEVNULL)
                for _ in range(120):
                    ready = subprocess.run(['docker', 'exec', name, 'sh', '-c',
                        'IFS= read -r process < /proc/1/comm; test "$process" = postgres && exec pg_isready -U restore_operator -d restore_candidate'],
                                           stdout=log, stderr=log, stdin=subprocess.DEVNULL, timeout=5)
                    if ready.returncode == 0:
                        break
                    time.sleep(0.25)
                else:
                    raise VerificationError('Isolated PostgreSQL did not become ready')
                with (payload / 'database.dump').open('rb') as dump:
                    run(['docker', 'exec', '-i', '-e', 'PGOPTIONS=-c statement_timeout=120000 -c lock_timeout=5000', name,
                         'pg_restore', '--exit-on-error', '--no-owner', '--no-privileges', '-U', 'restore_operator', '-d', 'restore_candidate'], stdin=dump)
                # Candidate only: do not allow recovery to resurrect old sessions/enrollment.
                sql = b'''BEGIN;
UPDATE admin_accounts SET auth_version = auth_version + 1;
UPDATE admin_sessions SET revoked_at = clock_timestamp() WHERE revoked_at IS NULL;
DELETE FROM admin_preauth;
DELETE FROM admin_enrollments;
COMMIT;
SELECT json_build_object(
 'strokes', (SELECT count(*) FROM strokes),
 'admin_accounts', (SELECT count(*) FROM admin_accounts),
 'moderation_operations', (SELECT count(*) FROM moderation_operations),
 'active_sessions', (SELECT count(*) FROM admin_sessions WHERE revoked_at IS NULL),
 'pending_ceremonies', (SELECT count(*) FROM admin_preauth),
 'enrollment_grants', (SELECT count(*) FROM admin_enrollments),
 'migrations_valid', (SELECT bool_and(success) FROM _sqlx_migrations),
 'migrations', (SELECT json_agg(version ORDER BY version) FROM _sqlx_migrations));'''
                result = json.loads(run(['docker', 'exec', '-i', name, 'psql', '-X', '-q', '-At', '-v', 'ON_ERROR_STOP=1',
                                         '-U', 'restore_operator', '-d', 'restore_candidate'], input=sql))
                if not result['migrations_valid'] or 2 not in result['migrations'] or any(result[key] != 0 for key in ['active_sessions', 'pending_ceremonies', 'enrollment_grants']):
                    raise VerificationError('Restored schema/auth revocation checks failed')
            finally:
                if created:
                    run(['docker', 'rm', '--force', '--volumes', name], stdin=subprocess.DEVNULL)
            report = {'verified_utc': datetime.now(timezone.utc).isoformat(), 'ciphertext_sha256': args.sha256,
                      'operator_age_decryption': True, 'isolated_restore': True, 'candidate_sessions_revoked': True,
                      'production_touched': False, 'image': manifest.get('image'), 'frontend_sha': manifest.get('frontend_sha'),
                      'restored': result}
    # Record success only AFTER both container and private plaintext cleanup succeeded.
    args.report.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    with args.report.open('x') as output:
        output.write(json.dumps(report, indent=2) + '\n')
    print('VERIFIED: real remote backup decrypted, checksummed and restored in isolated local PostgreSQL; candidate auth revoked; plaintext and container removed.')


if __name__ == '__main__':
    try:
        main()
    except VerificationError as error:
        print('Verification FAILED: ' + str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, TypeError, KeyError, tarfile.TarError, subprocess.SubprocessError):
        print('Verification FAILED; no success report was created. Private key and database contents were not logged.', file=sys.stderr)
        sys.exit(1)
