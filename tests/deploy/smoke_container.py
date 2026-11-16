#!/usr/bin/env python3
"""Smoke-test a built image + production Compose in a disposable LOCAL Docker project.

Never pass production env/volumes. The generated test volume is deleted on exit.
"""

import argparse
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile
import time
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image")
    parser.add_argument("--expected-arch", choices=("amd64", "arm64"))
    args = parser.parse_args()
    if args.expected_arch:
        architecture = subprocess.check_output(
            ["docker", "image", "inspect", "-f", "{{.Architecture}}", args.image], text=True
        ).strip()
        assert architecture == args.expected_arch, architecture

    project = "endlessdrawing-deploy-smoke-" + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix="endlessdrawing-smoke-") as directory:
        env_file = Path(directory) / "runtime.env"
        password = secrets.token_hex(32)
        env_file.write_text(
            "POSTGRES_DB=canvas\nPOSTGRES_USER=canvas\n"
            f"POSTGRES_PASSWORD={password}\n"
            f"DATABASE_URL=postgres://canvas:{password}@postgres:5432/canvas\n"
            "ALLOWED_ORIGINS=http://localhost:8080,http://127.0.0.1:8080\nRUST_LOG=info\n"
        )
        env_file.chmod(0o600)
        env = dict(os.environ)
        # Do not let a developer/CI DATABASE_URL override the disposable test credentials.
        for key in ("POSTGRES_DB", "POSTGRES_USER", "POSTGRES_PASSWORD", "DATABASE_URL", "ALLOWED_ORIGINS", "RUST_LOG"):
            env.pop(key, None)
        env["ENDLESSDRAWING_IMAGE"] = args.image
        command = ["docker", "compose", "-p", project, "--env-file", str(env_file), "-f", str(ROOT / "deploy" / "compose.yml")]
        try:
            subprocess.run(command + ["up", "-d", "--wait", "--wait-timeout", "120"], env=env, check=True)
            for _ in range(100):
                try:
                    with urllib.request.urlopen("http://127.0.0.1:8080/readyz", timeout=1) as response:
                        assert json.load(response)["status"] == "ready"
                    break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("published loopback port did not become accessible")
            with urllib.request.urlopen("http://127.0.0.1:8080/healthz", timeout=5) as response:
                assert json.load(response)["status"] == "ok"
            with urllib.request.urlopen("http://127.0.0.1:8080/", timeout=5) as response:
                html = response.read().decode()
                assert "Endless Drawing" in html
                asset = re.search(r'src="([^"]+\.js)"', html).group(1)
            with urllib.request.urlopen("http://127.0.0.1:8080" + asset, timeout=5) as response:
                assert response.status == 200
            backend = subprocess.check_output(command + ["ps", "-q", "backend"], env=env, text=True).strip()
            settings = subprocess.check_output(
                ["docker", "inspect", "-f", "{{.Config.User}} {{.HostConfig.ReadonlyRootfs}}", backend], text=True
            ).strip()
            assert settings == "10001:10001 true", settings
            postgres = subprocess.check_output(command + ["ps", "-q", "postgres"], env=env, text=True).strip()
            ports = subprocess.check_output(
                ["docker", "inspect", "-f", "{{json .HostConfig.PortBindings}}", postgres], text=True
            ).strip()
            assert json.loads(ports) in (None, {}), ports
            backup = subprocess.check_output(
                command + ["exec", "-T", "postgres", "sh", "-c", 'exec pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB" --format=custom'], env=env
            )
            assert backup.startswith(b"PGDMP")
            subprocess.run(command + ["exec", "-T", "postgres", "pg_restore", "--list"], env=env, input=backup, stdout=subprocess.DEVNULL, check=True)
            print("Container smoke passed: frontend/assets, probes, non-root/read-only app, no published DB port, readable pg_dump archive")
        finally:
            # Only the random project created above: never the persistent staging volume.
            subprocess.run(command + ["down", "--volumes", "--timeout", "20"], env=env, check=True)


if __name__ == "__main__":
    main()
