"""Deployment state-machine tests with a fake Docker CLI; no real VPS or DB is touched."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[2] / "deploy" / "activate.sh"
FIRST = "a" * 40 + "-1-1"
SECOND = "b" * 40 + "-2-1"
PREFIX = "endlessdrawing-app:"

FAKE_DOCKER = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys
args = sys.argv[1:]
image = os.environ.get("ENDLESSDRAWING_IMAGE", "")
with open(os.environ["MOCK_DOCKER_LOG"], "a") as log:
    log.write(json.dumps({"args": args, "image": image}) + "\n")
if args[:2] == ["image", "load"]:
    sys.stdin.buffer.read()
    sys.exit(0)
if args[:2] == ["image", "inspect"] or args == ["info"]:
    sys.exit(0)
if args[:1] != ["compose"]:
    sys.exit(90)
args = args[1:]
while args and args[0] in ("--env-file", "-f"):
    args = args[2:]
if args == ["config", "--quiet"] or args == ["ps"]:
    sys.exit(0)
if args[:2] == ["exec", "-T"]:
    if "pg_dump" in " ".join(args):
        if os.environ.get("MOCK_BACKUP_FAIL"):
            sys.exit(1)
        sys.stdout.buffer.write(b"PGDMP-mock-archive")
    elif "pg_restore" in args:
        if not sys.stdin.buffer.read().startswith(b"PGDMP"):
            sys.exit(1)
    elif "pg_database_size" in " ".join(args):
        print(os.environ.get("MOCK_DATABASE_BYTES", "1048576"))
    else:
        sys.exit(91)
    sys.exit(0)
if args[:1] == ["up"]:
    if args[-1] == "backend":
        if os.environ.get("MOCK_FAIL_ALL") or image == os.environ.get("MOCK_FAIL_IMAGE"):
            sys.exit(1)
    sys.exit(0)
if args == ["stop", "backend"]:
    sys.exit(0)
sys.exit(92)
'''


class ActivationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.app = root / "app"
        self.config = root / "config"
        self.bin = root / "bin"
        for directory in (self.app, self.config, self.bin):
            directory.mkdir()
        (self.app / "compose.yml").write_text("services: {}\n")
        (self.config / "runtime.env").write_text("POSTGRES_PASSWORD=not-for-logs\n")
        self.log = root / "calls.jsonl"
        self.env = dict(os.environ)
        self.env.update(
            PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
            ENDLESSDRAWING_APP_DIR=str(self.app),
            ENDLESSDRAWING_CONFIG_DIR=str(self.config),
            MOCK_DOCKER_LOG=str(self.log),
        )
        for key in ("MOCK_FAIL_ALL", "MOCK_FAIL_IMAGE", "MOCK_BACKUP_FAIL", "MOCK_DATABASE_BYTES"):
            self.env.pop(key, None)
        self.executable("docker", FAKE_DOCKER)
        self.executable("df", "#!/bin/sh\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted\\nmock 20000000 1000 19000000 1%% /\\n'\n")
        # macOS lacks flock; Linux CI exercises the real lock implementation.
        if shutil.which("flock") is None:
            self.executable("flock", "#!/bin/sh\nexit 0\n")

    def executable(self, name, content):
        path = self.bin / name
        path.write_text(content)
        path.chmod(0o755)

    def run_activation(self, *args):
        result = subprocess.run(
            ["bash", str(SCRIPT), *args],
            env=self.env,
            input=b"mock-compressed-image",
            capture_output=True,
            timeout=10,
        )
        self.assertNotIn(b"not-for-logs", result.stdout + result.stderr)
        return result

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def current(self):
        return (self.app / "current-image").read_text().strip()

    def record_current(self, release=FIRST):
        (self.app / "current-image").write_text(PREFIX + release + "\n")

    def backend_starts(self):
        return [call["image"] for call in self.calls() if "up" in call["args"] and call["args"][-1] == "backend"]

    def test_preflight_does_not_start_services_or_load_an_image(self):
        result = self.run_activation("check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("up" in call["args"] or "load" in call["args"] for call in self.calls()))

    @unittest.skipUnless(shutil.which("flock"), "real flock is checked on Linux CI")
    def test_deployment_lock_rejects_a_concurrent_activation(self):
        import fcntl
        with (self.app / "deploy.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_activation("check")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_rejects_injected_release_before_loading(self):
        result = self.run_activation("deploy", FIRST + "; touch /tmp/unwanted")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any("load" in call["args"] for call in self.calls()))

    def test_success_backs_up_before_activation_and_updates_state(self):
        self.record_current()
        result = self.run_activation("deploy", SECOND)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.current(), PREFIX + SECOND)
        self.assertEqual((self.app / "previous-image").read_text().strip(), PREFIX + FIRST)
        backups = list((self.app / "backups").glob("*.dump"))
        self.assertEqual(len(backups), 1)
        self.assertTrue(backups[0].read_bytes().startswith(b"PGDMP"))
        calls = self.calls()
        backup_index = next(i for i, call in enumerate(calls) if "pg_dump" in " ".join(call["args"]))
        activate_index = next(i for i, call in enumerate(calls) if "up" in call["args"] and call["args"][-1] == "backend")
        self.assertLess(backup_index, activate_index)

    def test_backup_failure_does_not_restart_backend_or_change_state(self):
        self.record_current()
        self.env["MOCK_BACKUP_FAIL"] = "1"
        result = self.run_activation("deploy", SECOND)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.current(), PREFIX + FIRST)
        self.assertEqual(self.backend_starts(), [])
        self.assertEqual(list((self.app / "backups").glob("*")), [])

    def test_unhealthy_release_recovers_previous_image_but_job_stays_failed(self):
        self.record_current()
        self.env["MOCK_FAIL_IMAGE"] = PREFIX + SECOND
        result = self.run_activation("deploy", SECOND)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.current(), PREFIX + FIRST)
        self.assertEqual(self.backend_starts(), [PREFIX + SECOND, PREFIX + FIRST])
        self.assertTrue(list((self.app / "backups").glob("*.dump")))

    def test_unrecoverable_release_stops_backend_without_restoring_database(self):
        self.record_current()
        self.env["MOCK_FAIL_ALL"] = "1"
        result = self.run_activation("deploy", SECOND)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(any(call["args"][-2:] == ["stop", "backend"] for call in self.calls()))
        # pg_restore --list checks readability only; no SQL restoration is attempted.
        restores = [call["args"] for call in self.calls() if "pg_restore" in call["args"]]
        self.assertEqual(len(restores), 1)
        self.assertIn("--list", restores[0])

    def test_first_bad_deploy_stays_stopped_without_fabricating_current_state(self):
        self.env["MOCK_FAIL_IMAGE"] = PREFIX + FIRST
        result = self.run_activation("deploy", FIRST)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.app / "current-image").exists())
        self.assertEqual(self.backend_starts(), [PREFIX + FIRST])

    def test_manual_rollback_uses_existing_image_and_takes_a_backup(self):
        self.record_current(SECOND)
        result = self.run_activation("rollback", FIRST)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.current(), PREFIX + FIRST)
        self.assertFalse(any("load" in call["args"] for call in self.calls()))
        self.assertTrue(list((self.app / "backups").glob("*.dump")))

    def test_large_database_requires_backup_space_without_restarting_backend(self):
        self.record_current()
        self.env["MOCK_DATABASE_BYTES"] = "100000000000"
        result = self.run_activation("deploy", SECOND)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.current(), PREFIX + FIRST)
        self.assertEqual(self.backend_starts(), [])
        self.assertFalse(any("pg_dump" in " ".join(call["args"]) for call in self.calls()))

    def test_low_disk_cancels_before_loading_or_mutating(self):
        self.executable("df", "#!/bin/sh\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted\\nmock 20000000 19000000 1000000 95%% /\\n'\n")
        result = self.run_activation("deploy", FIRST)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any("load" in call["args"] or "up" in call["args"] for call in self.calls()))


if __name__ == "__main__":
    unittest.main()
