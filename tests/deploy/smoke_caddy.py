#!/usr/bin/env python3
"""LOCAL Caddy deny-by-default gate test; no ACME, public host port, backend or DB access."""
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[2]
IMAGE = "caddy:2-alpine"


def response(port, spoof=False):
    headers = {"Host": "localhost:8080"}
    if spoof:
        headers.update({"X-Forwarded-For": "127.0.0.1", "X-Real-IP": "127.0.0.1", "Forwarded": "for=127.0.0.1"})
    request = urllib.request.Request(f"http://127.0.0.1:{port}/admin/", headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=2) as reply:
            return reply.status
    except urllib.error.HTTPError as reply:
        return reply.code


def main():
    for allow_external in (False, True):
        name = "endlessdrawing-caddy-test-" + uuid.uuid4().hex[:12]
        command = ["docker", "run", "-d", "--name", name, "-p", "127.0.0.1::8080",
                   "-v", f"{ROOT / 'deploy/caddy/Caddyfile'}:/etc/caddy/Caddyfile:ro",
                   "-v", f"{ROOT / 'deploy/caddy/access.private.caddy'}:/etc/endlessdrawing/caddy/access.caddy:ro",
                   "-e", "ENDLESSDRAWING_SITE=http://localhost:8080", "-e", "ENDLESSDRAWING_UPSTREAM=127.0.0.1:9"]
        if allow_external:
            # Test-only loopback publishing. No real application is reachable behind port 9.
            command += ["-e", "ENDLESSDRAWING_ALLOWED_IPS=0.0.0.0/0 ::/0"]
        subprocess.run(command + [IMAGE], check=True, stdout=subprocess.DEVNULL)
        try:
            ports = json.loads(subprocess.check_output(["docker", "inspect", "-f", "{{json .NetworkSettings.Ports}}", name], text=True))
            binding = ports["8080/tcp"][0]
            assert binding["HostIp"] == "127.0.0.1"
            port = binding["HostPort"]
            for _ in range(100):
                try:
                    actual = response(port)
                    break
                except OSError:
                    time.sleep(0.1)
            else:
                raise AssertionError("Caddy did not become reachable")
            assert actual == (502 if allow_external else 403), actual
            if not allow_external:
                assert response(port, spoof=True) == 403, "Forwarded headers must not bypass the peer-IP gate"
        finally:
            subprocess.run(["docker", "rm", "-f", name], check=True, stdout=subprocess.DEVNULL)
    print("Caddy private gate passed: deny by default, no forwarded-IP spoofing, explicit allow reaches only the test upstream")


if __name__ == "__main__":
    main()
