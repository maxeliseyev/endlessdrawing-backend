#!/usr/bin/env python3
"""Real public UI/WS/PostgreSQL gate using a fresh local test DB, never the source DB.

Use only a local Unix Docker endpoint and a loopback development/CI DATABASE_URL.
The existing PostgreSQL container is not stopped or removed. Only a successfully
created random test database and the backend process started here are cleaned up.
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


def local_database_source(value):
    source = urllib.parse.urlsplit(value)
    if (source.scheme not in ('postgres', 'postgresql') or source.hostname != '127.0.0.1'
            or not source.username or source.query or source.fragment):
        raise RuntimeError('Only explicit loopback PostgreSQL test URLs without overrides are accepted')
    return source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--frontend', type=Path, required=True)
    parser.add_argument('--backend', type=Path, required=True)
    parser.add_argument('--postgres-container', required=True)
    args = parser.parse_args()
    os.umask(0o077)
    source = local_database_source(os.environ['DATABASE_URL'])
    endpoint = os.environ.get('DOCKER_HOST')
    if os.environ.get('DOCKER_CONTEXT') or not endpoint:
        endpoint = subprocess.check_output(['docker', 'context', 'inspect', '--format', '{{.Endpoints.docker.Host}}'], text=True).strip()
    if not endpoint.startswith('unix://'):
        raise RuntimeError('Only a local Unix Docker endpoint is accepted')
    frontend = args.frontend.resolve()
    assert (frontend / 'dist/index.html').is_file()
    assert 'test:integration' in json.loads((frontend / 'package.json').read_text())['scripts']
    backend = args.backend.resolve()
    assert backend.is_file()
    database = 'ed_public_browser_' + secrets.token_hex(12)
    user = urllib.parse.unquote(source.username)
    # No caller-controlled SQL or DB name. Successful create is required before drop.
    postgres = ['docker', 'exec', args.postgres_container]
    subprocess.run(postgres + ['createdb', '-U', user, '--template=template0', '--maintenance-db=postgres', database], check=True, stdin=subprocess.DEVNULL)
    process = None
    try:
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        origin = f'http://127.0.0.1:{port}'
        env = os.environ.copy()
        env.update(DATABASE_URL=urllib.parse.urlunsplit(source._replace(path='/' + database)),
                   BIND_ADDR=f'127.0.0.1:{port}', STATIC_DIR=str(frontend / 'dist'),
                   ALLOWED_ORIGINS=origin, ADMIN_ENABLED='false', E2E_BACKEND_URL=origin,
                   DATABASE_MIGRATE_ON_START='true', DATABASE_REQUIRE_RUNTIME_ROLE='false')
        with tempfile.TemporaryDirectory(prefix='endlessdrawing-public-browser-') as temporary:
            with (Path(temporary) / 'private-backend.log').open('wb') as log:
                process = subprocess.Popen([str(backend)], env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
                for _ in range(150):
                    if process.poll() is not None:
                        raise RuntimeError('Test backend stopped before readiness; source DB was not used')
                    try:
                        with urllib.request.urlopen(origin + '/readyz', timeout=1) as reply:
                            if reply.status == 200:
                                break
                    except (urllib.error.URLError, TimeoutError):
                        pass
                    time.sleep(0.2)
                else:
                    raise RuntimeError('Test backend did not become ready')
                subprocess.run(['npm', 'run', 'test:integration', '--', '--output', str(Path(temporary) / 'browser-artifacts')],
                               cwd=frontend, env=env, check=True, timeout=300, stdin=subprocess.DEVNULL)
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        subprocess.run(postgres + ['dropdb', '--force', '-U', user, '--maintenance-db=postgres', database], check=True, stdin=subprocess.DEVNULL)
    print('Real public browser gate passed; source DB unchanged, disposable DB removed.')


if __name__ == '__main__':
    main()
