"""Backup safety tests. Fake Docker/Storage Box only; no real repository is accessed."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "deploy" / "backup.sh"
spec = importlib.util.spec_from_file_location("storagebox", ROOT / "deploy" / "backup-storagebox.py")
backup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backup)
IMAGE = "endlessdrawing-app:" + "a" * 40 + "-1-1"
STAMP = "20261116T041000Z-aabbccdd"

FAKE_DOCKER = r'''#!/usr/bin/env python3
import json, os, re, sys
args = sys.argv[1:]
with open(os.environ["MOCK_LOG"], "a") as log:
    log.write(json.dumps(args) + "\n")
if args[:2] == ["image", "inspect"]:
    print("b" * 40); sys.exit(0)
if args[:1] != ["compose"]: sys.exit(90)
while args[1:2] in (["--env-file"], ["-f"]): args = args[:1] + args[3:]
args = args[1:]
if args == ["config", "--quiet"]: sys.exit(0)
if args[:2] != ["exec", "-T"]: sys.exit(91)
command = " ".join(args)
if "pg_database_size" in command:
    print(os.environ.get("MOCK_SIZE", "1048576")); sys.exit(0)
if "pg_dump" in command:
    sys.stdout.buffer.write(b"PGDMP-test-only archive")
    if os.environ.get("MOCK_DUMP_FAIL"):
        print("private-fixture-reason", file=sys.stderr); sys.exit(1)
    sys.exit(0)
if "pg_restore" in command:
    assert sys.stdin.buffer.read().startswith(b"PGDMP")
    if os.environ.get("MOCK_RESTORE_FAIL") and "--list" not in args:
        print("private-fixture-reason", file=sys.stderr); sys.exit(1)
    sys.exit(0)
if "createdb" in command or "dropdb" in command:
    assert re.fullmatch(r"ed_verify_[0-9a-f]{24}", args[-1]), args[-1]
    if "createdb" in command and os.environ.get("MOCK_CREATE_FAIL"): sys.exit(1)
    sys.exit(0)
if "psql" in command:
    assert "FROM strokes" in sys.stdin.read()
    sys.exit(1 if os.environ.get("MOCK_SCHEMA_FAIL") else 0)
sys.exit(92)
'''


@unittest.skipUnless(shutil.which("flock"), "run on Linux to exercise real flock")
class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.app = self.root / "app"
        self.config = self.root / "config"
        self.spool = self.root / "spool"
        self.bin = self.root / "bin"
        for directory in (self.app, self.config, self.spool, self.bin): directory.mkdir()
        (self.app / "compose.yml").write_text("services: {}\n")
        (self.app / "current-image").write_text(IMAGE + "\n")
        (self.config / "runtime.env").write_text("POSTGRES_PASSWORD=test-only-not-for-logs\n")
        self.log = self.root / "calls.jsonl"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        ENDLESSDRAWING_APP_DIR=str(self.app), ENDLESSDRAWING_CONFIG_DIR=str(self.config),
                        ENDLESSDRAWING_BACKUP_DIR=str(self.spool), MOCK_LOG=str(self.log))
        for name in ("MOCK_SIZE", "MOCK_DUMP_FAIL", "MOCK_RESTORE_FAIL", "MOCK_CREATE_FAIL", "MOCK_SCHEMA_FAIL"):
            self.env.pop(name, None)
        self.executable("docker", FAKE_DOCKER)
        self.executable("openssl", "#!/bin/sh\nprintf '%s\\n' 0123456789abcdef01234567\n")
        self.executable("df", "#!/bin/sh\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted\\nmock 90000000 1 80000000 1%% /\\n'\n")

    def executable(self, name, content):
        file = self.bin / name
        file.write_text(content)
        file.chmod(0o755)

    def run_script(self, *args):
        result = subprocess.run(["bash", str(SCRIPT), *map(str, args)], env=self.env, capture_output=True, text=True)
        self.assertNotIn("test-only-not-for-logs", result.stdout + result.stderr)
        self.assertNotIn("private-fixture-reason", result.stdout + result.stderr)
        return result

    def export(self):
        result = self.run_script("export")
        self.assertEqual(result.returncode, 0, result.stderr)
        return Path(result.stdout.strip())

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_export_is_private_checksummed_versioned_and_explicitly_not_offsite(self):
        result = self.run_script("export")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NOT yet uploaded", result.stderr)
        bundle = Path(result.stdout.strip())
        self.assertEqual(bundle.stat().st_mode & 0o777, 0o700)
        self.assertTrue((bundle / "READY").is_file())
        self.assertFalse((bundle / "RESTORE_VERIFIED").exists())
        metadata = json.loads((bundle / "manifest.json").read_text())
        self.assertEqual(metadata["image"], IMAGE)
        self.assertEqual(metadata["frontend_sha"], "b" * 40)
        self.assertEqual(metadata["database_bytes"], 1048576)

    def test_restore_uses_new_database_and_removes_it_before_success(self):
        bundle = self.export()
        result = self.run_script("verify", bundle)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((bundle / "RESTORE_VERIFIED").is_file())
        creates = [args[-1] for args in self.calls() if "exec createdb" in " ".join(args)]
        drops = [args[-1] for args in self.calls() if "exec dropdb" in " ".join(args)]
        self.assertEqual(creates, drops)
        self.assertEqual(len(creates), 1)
        self.assertTrue(creates[0].startswith("ed_verify_"))

    def test_restore_failure_cleans_only_owned_database_and_never_reports_success(self):
        bundle = self.export()
        self.env["MOCK_RESTORE_FAIL"] = "1"
        result = self.run_script("verify", bundle)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((bundle / "RESTORE_VERIFIED").exists())
        self.assertEqual(sum("exec dropdb" in " ".join(args) for args in self.calls()), 1)

    def test_failed_create_never_drops_an_existing_database(self):
        bundle = self.export()
        self.env["MOCK_CREATE_FAIL"] = "1"
        self.assertNotEqual(self.run_script("verify", bundle).returncode, 0)
        self.assertFalse(any("dropdb" in " ".join(args) for args in self.calls()))

    def test_corruption_clears_old_success_marker_before_any_restore(self):
        bundle = self.export()
        (bundle / "RESTORE_VERIFIED").write_text("old success")
        (bundle / "database.dump").write_bytes(b"corrupt")
        self.assertNotEqual(self.run_script("verify", bundle).returncode, 0)
        self.assertFalse((bundle / "RESTORE_VERIFIED").exists())
        self.assertFalse(any("createdb" in " ".join(args) for args in self.calls()))

    def test_failed_dump_has_no_ready_marker(self):
        self.env["MOCK_DUMP_FAIL"] = "1"
        self.assertNotEqual(self.run_script("export").returncode, 0)
        self.assertFalse(any(self.spool.glob("export-*/READY")))

    def test_low_disk_or_bad_image_stops_before_dump(self):
        self.executable("df", "#!/bin/sh\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted\\nmock 100 1 99 1%% /\\n'\n")
        self.assertNotEqual(self.run_script("export").returncode, 0)
        self.assertFalse(any("pg_dump" in " ".join(args) for args in self.calls()))
        (self.app / "current-image").write_text("untrusted:latest\n")
        self.assertNotEqual(self.run_script("export").returncode, 0)

    def test_external_or_symlink_bundle_is_rejected(self):
        external = self.root / "external"
        external.mkdir()
        self.assertNotEqual(self.run_script("verify", external).returncode, 0)
        bundle = self.export()
        (bundle / "manifest.json").unlink()
        (bundle / "manifest.json").symlink_to(self.config / "runtime.env")
        self.assertNotEqual(self.run_script("verify", bundle).returncode, 0)

    def test_deployment_lock_prevents_export(self):
        import fcntl
        with (self.app / "deploy.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertNotEqual(self.run_script("export").returncode, 0)
        self.assertEqual(self.calls(), [])


class FakeStorageBox(backup.StorageBox):
    def __init__(self, config, fail="", listing=""):
        self.config, self.fail, self.listing = config, fail, listing
        self.calls, self.files = [], {}

    def command(self, *args):
        self.calls.append(args)
        if args[0] == "sha256sum":
            path = args[1]
            return f'{"0" * 64 if self.fail == "checksum" else self.files.get(path, "missing")}  {path}\n'
        if args[0] in ("mv", "cp"):
            source, target = args[-2:]
            if self.fail == "move" and args[0] == "mv": return ""  # Storage Box false-success simulation.
            for path, digest in list(self.files.items()):
                if path.startswith(source + "/"):
                    self.files[target + path[len(source):]] = digest
                    if args[0] == "mv": del self.files[path]
        if args[0] == "ls": return self.listing
        return ""

    def put(self, source, destination):
        self.calls.append(("scp", destination))
        self.files[destination] = backup.checksum(source)


class StorageBoxTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.identity = self.root / "ssh-identity"
        self.identity.write_text("opaque test-only identity; not a real private key")
        self.identity.chmod(0o600)
        self.known_hosts = self.root / "known_hosts"
        self.known_hosts.write_text("test-only verified public host key")
        self.config_file = self.root / "backup.env"
        self.base = {
            "BACKUP_SSH_HOST": "u12345.your-storagebox.de", "BACKUP_SSH_USER": "u12345-sub1",
            "BACKUP_AGE_RECIPIENT": "age1" + "a" * 58, "BACKUP_REMOTE_DIR": "backups",
            "BACKUP_SSH_IDENTITY_FILE": str(self.identity), "BACKUP_SSH_KNOWN_HOSTS_FILE": str(self.known_hosts),
        }
        self.config = self.load()
        self.cipher = self.root / f"pg-{STAMP}.tar.age"
        self.cipher.write_bytes(b"opaque ciphertext fixture")

    def load(self, **changes):
        values = dict(self.base, **changes)
        self.config_file.write_text("\n".join(f"{key}={value}" for key, value in values.items()) + "\n")
        self.config_file.chmod(0o600)
        return backup.read_config(self.config_file)

    def test_configuration_rejects_master_construct_traversal_and_shell_code(self):
        for changes in [{"BACKUP_SSH_USER": "u12345"}, {"BACKUP_REMOTE_DIR": "construct"},
                        {"BACKUP_REMOTE_DIR": "../construct"}, {"BACKUP_SSH_HOST": "host;rm"},
                        {"BACKUP_KEEP_DAILY": "0"}, {"BACKUP_KEEP_WEEKLY": "999"},
                        {"BACKUP_AGE_RECIPIENT": "$(touch /tmp/untrusted)"}]:
            with self.subTest(changes=changes), self.assertRaises(backup.BackupError): self.load(**changes)

    def test_subaccount_hostname_is_supported_but_cross_account_hosts_are_rejected(self):
        values = self.load(BACKUP_SSH_HOST='u12345-sub1.your-storagebox.de')
        self.assertEqual(values['BACKUP_SSH_USER'], 'u12345-sub1')
        for host in ['u99999.your-storagebox.de', 'u12345-sub2.your-storagebox.de',
                     'u12345-sub1.your-storagebox.de.attacker.test', 'u12345-sub1.your-storagebox.de;rm']:
            with self.subTest(host=host), self.assertRaises(backup.BackupError):
                self.load(BACKUP_SSH_HOST=host)

    def test_local_only_requires_no_ssh_key_or_storage_account(self):
        self.config_file.write_text('BACKUP_AGE_RECIPIENT=' + 'age1' + 'a' * 58 + '\n')
        values = backup.read_config(self.config_file, require_remote=False)
        self.assertEqual(values['BACKUP_KEEP_DAILY'], 7)
        self.assertNotIn('BACKUP_SSH_IDENTITY_FILE', values)
        with self.assertRaises(backup.BackupError): backup.read_config(self.config_file)

    def test_local_main_never_passes_operator_stdin_to_docker_helpers(self):
        import io
        spool = self.root / 'spool'
        spool.mkdir()
        bundle = spool / 'export-fixture'
        bundle.mkdir()
        calls = []
        def run(args, **kwargs):
            self.assertEqual(kwargs.get('stdin'), subprocess.DEVNULL)
            calls.append(args[1])
            return subprocess.CompletedProcess(args, 0, (str(bundle) + '\n').encode() if args[1] == 'export' else b'', b'')
        def encrypt(source, destination, recipient, log):
            destination.write_bytes(b'test-only ciphertext')
        with patch.object(backup.subprocess, 'run', side_effect=run), \
             patch.object(backup, 'encrypt_bundle', side_effect=encrypt), \
             patch.dict(os.environ, {'ENDLESSDRAWING_BACKUP_DIR': str(spool)}), \
             patch.object(backup.sys, 'argv', ['backup', '--config', str(self.config_file), '--local-only']), \
             patch('sys.stdout', new_callable=io.StringIO):
            backup.main()
        self.assertEqual(calls, ['export', 'verify'])
        self.assertFalse((spool / 'last-success.json').exists())

    def test_stamp_filter_rejects_paths_and_invalid_calendar_dates(self):
        for value in ["../construct", "20261399T999999Z-aabbccdd", STAMP + "/../construct", "construct"]:
            self.assertFalse(backup.stamp_valid(value))
        self.assertTrue(backup.stamp_valid(STAMP))

    def test_failed_checksum_or_false_success_rename_never_rotates(self):
        for mode in ("checksum", "move"):
            box = FakeStorageBox(self.config, fail=mode)
            with self.assertRaises(backup.BackupError): box.upload(self.cipher, STAMP)
            self.assertFalse(any(args[0] == "rm" for args in box.calls))

    def test_weekly_has_its_own_checksums_before_rotation(self):
        box = FakeStorageBox(self.config)
        digest = box.upload(self.cipher, STAMP, weekly=True)
        checks = [args[1] for args in box.calls if args[0] == "sha256sum"]
        self.assertEqual(len(checks), 4)
        self.assertEqual(checks[-1], f"backups/weekly/{STAMP}/{self.cipher.name}")
        self.assertEqual(digest, hashlib.sha256(self.cipher.read_bytes()).hexdigest())
        first_listing = next(i for i, args in enumerate(box.calls) if args[0] == "ls")
        last_check = max(i for i, args in enumerate(box.calls) if args[0] == "sha256sum")
        self.assertGreater(first_listing, last_check)
        self.assertTrue(all(not any(token in " ".join(args) for token in ("&&", "|", ";")) for args in box.calls))

    def test_rotation_cannot_delete_construct_unknown_names_or_parent_paths(self):
        names = [f"202611{day:02d}T041000Z-aabbccdd" for day in range(1, 11)]
        box = FakeStorageBox(self.config, listing="\n".join(names + ["../construct", "construct", "/", STAMP + "/../construct"]))
        box.rotate("daily", 7)
        removed = [args[2] for args in box.calls if args[0] == "rm"]
        self.assertEqual(removed, [f"backups/daily/{name}" for name in names[:3]])

    def test_ssh_host_pin_dedicated_identity_and_distinct_port_flags_are_mandatory(self):
        with tempfile.TemporaryFile() as log, patch.object(backup.subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess([], 0, b"", b"")
            box = backup.StorageBox(self.config, log)
            box.command("mkdir", "-p", "backups/daily")
            box.put(self.cipher, f"backups/incoming/{STAMP}/{self.cipher.name}")
            ssh, scp = [call.args[0] for call in run.call_args_list]
            self.assertEqual(ssh[:4], ["ssh", "-n", "-p", "23"])
            self.assertEqual(scp[:3], ["scp", "-P", "23"])
            for args in (ssh, scp):
                self.assertIn("StrictHostKeyChecking=yes", args)
                self.assertIn("IdentitiesOnly=yes", args)
                self.assertIn("IdentityAgent=none", args)
                self.assertIn(str(self.identity), args)
                self.assertIn(f"UserKnownHostsFile={self.known_hosts}", args)
            with self.assertRaises(backup.BackupError): box.command("mkdir", "backups;rm")

    def test_unverified_bundle_is_never_encrypted(self):
        with tempfile.TemporaryFile() as log, self.assertRaises(backup.BackupError):
            backup.encrypt_bundle(self.root, self.root / "output.age", "age1" + "a" * 58, log)


class OperatorRestoreTests(unittest.TestCase):
    def setUp(self):
        verify_spec = importlib.util.spec_from_file_location('verify_backup', ROOT / 'deploy/verify-backup.py')
        self.verify = importlib.util.module_from_spec(verify_spec)
        verify_spec.loader.exec_module(self.verify)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / 'output'
        self.output.mkdir()

    def archive(self, extra=None):
        import io
        import tarfile
        dump = b'PGDMP-test-only fixture'
        manifest = json.dumps({'format': 1, 'postgres_major': 17, 'database_bytes': 1024}).encode()
        hashes = (hashlib.sha256(dump).hexdigest() + '  database.dump\n' + hashlib.sha256(manifest).hexdigest() + '  manifest.json\n').encode()
        file = self.root / 'payload.tar'
        with tarfile.open(file, 'w') as target:
            for name, data in [('database.dump', dump), ('manifest.json', manifest), ('SHA256SUMS', hashes)]:
                item = tarfile.TarInfo(name)
                item.size = len(data)
                target.addfile(item, io.BytesIO(data))
            if extra:
                target.addfile(extra)
        return file

    def test_valid_payload_is_checksummed_without_private_identity_access(self):
        manifest = self.verify.extract_payload(self.archive(), self.output)
        self.assertEqual(manifest['postgres_major'], 17)
        self.assertEqual((self.output / 'database.dump').read_bytes(), b'PGDMP-test-only fixture')

    def test_path_traversal_duplicate_and_symlink_are_rejected(self):
        import tarfile
        for name in ['../identity', '/tmp/identity', 'database.dump']:
            with self.subTest(name=name), self.assertRaises(self.verify.VerificationError):
                self.verify.extract_payload(self.archive(tarfile.TarInfo(name)), self.output)
        link = tarfile.TarInfo('manifest.json')
        link.type = tarfile.SYMTYPE
        link.linkname = '/etc/passwd'
        with self.assertRaises(self.verify.VerificationError): self.verify.extract_payload(self.archive(link), self.output)
        self.assertEqual(list(self.output.iterdir()), [])

    def test_payload_corruption_is_rejected(self):
        file = self.archive()
        data = file.read_bytes().replace(b'PGDMP-test-only', b'PGDMP-bad!-only', 1)
        file.write_bytes(data)
        with self.assertRaises(self.verify.VerificationError): self.verify.extract_payload(file, self.output)

    def test_remote_docker_endpoints_are_never_used_for_operator_drill(self):
        self.assertTrue(self.verify.local_endpoint('unix:///var/run/docker.sock'))
        for endpoint in ['ssh://production', 'tcp://production:2376', 'http://127.0.0.1:2375', '']:
            self.assertFalse(self.verify.local_endpoint(endpoint))


class BackupHealthTests(unittest.TestCase):
    def test_missing_stale_future_local_only_or_invalid_state_is_not_healthy(self):
        from datetime import datetime, timedelta, timezone
        health_spec = importlib.util.spec_from_file_location("backup_health", ROOT / "deploy/backup-health.py")
        health = importlib.util.module_from_spec(health_spec)
        health_spec.loader.exec_module(health)
        now = datetime.now(timezone.utc)
        with tempfile.TemporaryDirectory() as directory:
            file = Path(directory) / 'state.json'
            self.assertFalse(health.healthy(file, now))
            good = {'last_success_utc': now.isoformat(), 'offsite': True, 'restore_drill': True}
            for state in [dict(good, offsite=False), dict(good, restore_drill=False),
                          dict(good, last_success_utc=(now - timedelta(hours=37)).isoformat()),
                          dict(good, last_success_utc=(now + timedelta(hours=1)).isoformat()),
                          dict(good, last_success_utc='unparseable'), []]:
                file.write_text(json.dumps(state))
                self.assertFalse(health.healthy(file, now))
            file.write_text(json.dumps(good))
            self.assertTrue(health.healthy(file, now))


if __name__ == "__main__":
    unittest.main()
