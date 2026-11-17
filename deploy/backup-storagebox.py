#!/usr/bin/env python3
"""PostgreSQL export/restore drill -> age -> isolated Hetzner Storage Box namespace.

No age private identity is read or stored here. Production restore is deliberately absent.
The SSH account MUST be a dedicated Storage Box subaccount chrooted to endlessdrawing.
"""
import argparse
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile


class BackupError(Exception):
    pass


def stamp_valid(value):
    if not re.fullmatch(r"20[0-9]{6}T[0-9]{6}Z-[0-9a-f]{8}", value):
        return False
    try:
        datetime.strptime(value[:16], "%Y%m%dT%H%M%SZ")
        return True
    except ValueError:
        return False


def checksum(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_config(path, require_remote=True):
    allowed = {"BACKUP_AGE_RECIPIENT", "BACKUP_SSH_HOST", "BACKUP_SSH_USER", "BACKUP_SSH_PORT",
               "BACKUP_REMOTE_DIR", "BACKUP_KEEP_DAILY", "BACKUP_KEEP_WEEKLY",
               "BACKUP_SSH_IDENTITY_FILE", "BACKUP_SSH_KNOWN_HOSTS_FILE"}
    if not path.is_file() or path.is_symlink() or path.stat().st_mode & 0o077 or path.stat().st_uid != os.geteuid():
        raise BackupError("Backup configuration must be owned by this operator and private (0600)")
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, raw = line.partition("=")
        key = key.strip()
        fields = shlex.split(raw, comments=True) if separator else []
        if key not in allowed or key in values or len(fields) != 1:
            raise BackupError("Invalid backup configuration; shell code is never sourced")
        values[key] = fields[0]
    # This name is fixed on purpose. The subaccount's REAL directory is endlessdrawing;
    # its logical SFTP/SSH root contains backups/, never ../construct or the box root.
    if values.get("BACKUP_REMOTE_DIR", "backups") != "backups":
        raise BackupError("Only the isolated backups namespace is accepted")
    if not re.fullmatch(r"age1[a-z0-9]{50,100}", values.get("BACKUP_AGE_RECIPIENT", "")):
        raise BackupError("An age public recipient is required; no private identity belongs here")
    for key, default, maximum in [("BACKUP_KEEP_DAILY", "7", 366), ("BACKUP_KEEP_WEEKLY", "4", 52)]:
        raw = values.get(key, default)
        if not raw.isdecimal() or not 1 <= int(raw) <= maximum:
            raise BackupError("Retention must keep at least one backup and remain bounded")
        values[key] = int(raw)
    if not require_remote:
        return values
    host = values.get("BACKUP_SSH_HOST", "")
    user = values.get("BACKUP_SSH_USER", "")
    if not re.fullmatch(r"u[0-9]+-sub[0-9]+", user):
        raise BackupError("Use a dedicated Storage Box subaccount, never the construct master account")
    parent = user.split("-sub")[0]
    # Hetzner supports the subaccount's own hostname as well as the parent Box hostname.
    # Neither an unrelated Box nor a different subaccount hostname may be substituted.
    if host not in {f"{parent}.your-storagebox.de", f"{user}.your-storagebox.de"}:
        raise BackupError("Storage Box hostname must belong to this exact subaccount or its parent")
    if values.get("BACKUP_SSH_PORT", "23") != "23":
        raise BackupError("The restricted SSH commands require Storage Box port 23")
    values.setdefault("BACKUP_SSH_IDENTITY_FILE", "/etc/endlessdrawing/backup/id_ed25519")
    values.setdefault("BACKUP_SSH_KNOWN_HOSTS_FILE", "/etc/endlessdrawing/backup/known_hosts")
    for key in ["BACKUP_SSH_IDENTITY_FILE", "BACKUP_SSH_KNOWN_HOSTS_FILE"]:
        file = Path(values[key])
        if not file.is_absolute() or file.is_symlink() or not file.is_file():
            raise BackupError("Dedicated SSH identity and verified known_hosts files are required")
    known_hosts = Path(values["BACKUP_SSH_KNOWN_HOSTS_FILE"]).stat()
    if known_hosts.st_uid != os.geteuid() or known_hosts.st_mode & 0o022:
        raise BackupError("Verified known_hosts must be operator-owned and not writable by others")
    identity = Path(values["BACKUP_SSH_IDENTITY_FILE"]).stat()
    if identity.st_uid != os.geteuid() or stat.S_IMODE(identity.st_mode) not in (0o400, 0o600):
        raise BackupError("SSH identity must be owned by this operator and mode 0600/0400")
    return values


class StorageBox:
    def __init__(self, config, log):
        self.config = config
        self.log = log
        self.target = f'{config["BACKUP_SSH_USER"]}@{config["BACKUP_SSH_HOST"]}'
        self.options = ["-F", "/dev/null", "-i", config["BACKUP_SSH_IDENTITY_FILE"],
                        "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
                        "-o", "StrictHostKeyChecking=yes", "-o", f'UserKnownHostsFile={config["BACKUP_SSH_KNOWN_HOSTS_FILE"]}',
                        "-o", "ConnectTimeout=20", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]

    def command(self, *args):
        # Exactly one remote command; Storage Box silently mishandles pipelines/&&/redirects.
        if not args or args[0] not in {"mkdir", "sha256sum", "mv", "cp", "ls", "rm"} or any(
            not re.fullmatch(r"[A-Za-z0-9_./-]+", arg) for arg in args
        ):
            raise BackupError("Invalid remote command")
        result = subprocess.run(["ssh", "-n", "-p", "23", *self.options, "--", self.target, " ".join(args)],
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=self.log, timeout=300)
        if result.returncode or len(result.stdout) > 1024 * 1024:
            raise BackupError("Storage Box command failed; inspect the private operation log")
        return result.stdout.decode("utf-8", errors="strict")

    def put(self, source, destination):
        result = subprocess.run(["scp", "-P", "23", *self.options, "-q", "--", str(source), f"{self.target}:{destination}"],
                                stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log, timeout=900)
        if result.returncode:
            raise BackupError("Encrypted upload failed; inspect the private operation log")

    def verify(self, path, digest):
        lines = self.command("sha256sum", path).strip().splitlines()
        if len(lines) != 1 or lines[0].split() != [digest, path]:
            raise BackupError("Remote ciphertext SHA-256 mismatch or missing file; nothing is rotated")

    def rotate(self, tier, keep):
        if tier not in ("daily", "weekly") or not 1 <= keep <= 366:
            raise BackupError("Invalid rotation scope")
        listing = self.command("ls", "-1", f"backups/{tier}")
        entries = sorted(set(name.rstrip("/") for name in listing.splitlines() if stamp_valid(name.rstrip("/"))))
        for name in entries[:-keep]:
            # No glob, shell pipeline, caller-controlled path, or deletion outside this namespace.
            self.command("rm", "-r", f"backups/{tier}/{name}")

    def upload(self, source, stamp, weekly=False):
        if not stamp_valid(stamp) or source.name != f"pg-{stamp}.tar.age":
            raise BackupError("Invalid snapshot name")
        digest = checksum(source)
        for path in ("backups/incoming", "backups/daily", "backups/weekly"):
            self.command("mkdir", "-p", path)
        pending, daily = f"backups/incoming/{stamp}", f"backups/daily/{stamp}"
        self.command("mkdir", pending)  # Refuse an existing destination instead of overwriting it.
        self.put(source, f"{pending}/{source.name}")
        self.verify(f"{pending}/{source.name}", digest)
        self.command("mv", pending, daily)
        self.verify(f"{daily}/{source.name}", digest)  # Detect a false-success/unsupported rename.
        if weekly:
            pending_week = f"backups/incoming/{stamp}-weekly"
            self.command("cp", "-r", daily, pending_week)
            self.verify(f"{pending_week}/{source.name}", digest)
            self.command("mv", pending_week, f"backups/weekly/{stamp}")
            self.verify(f"backups/weekly/{stamp}/{source.name}", digest)
        # Deletion is allowed only AFTER every required copy was independently verified.
        self.rotate("daily", self.config["BACKUP_KEEP_DAILY"])
        self.rotate("weekly", self.config["BACKUP_KEEP_WEEKLY"])
        return digest


def encrypt_bundle(bundle, destination, recipient, log):
    # Custom-format pg_dump already compresses its data. Tar adds only checksummed metadata.
    names = ["database.dump", "manifest.json", "SHA256SUMS"]
    manifest = json.loads((bundle / 'manifest.json').read_text()) if (bundle / 'manifest.json').is_file() else {}
    if manifest.get('totp_key_included') is True:
        key = bundle / 'totp-key.bin'
        if not key.is_file() or key.is_symlink() or key.stat().st_size != 32:
            raise BackupError('TOTP encryption key missing from verified bundle')
        names.append('totp-key.bin')
    if not (bundle / "RESTORE_VERIFIED").is_file():
        raise BackupError("A successful isolated restore drill is required before encryption")
    with destination.open("xb") as encrypted:
        tar = subprocess.Popen(["tar", "-C", str(bundle), "-cf", "-", *names], stdout=subprocess.PIPE, stderr=log)
        try:
            result = subprocess.run(["age", "-r", recipient], stdin=tar.stdout, stdout=encrypted, stderr=log, timeout=900)
        finally:
            tar.stdout.close()
            try:
                tar_status = tar.wait(timeout=30)
            except subprocess.TimeoutExpired:
                tar.kill()
                tar.wait()
                raise BackupError("Archive creation timed out") from None
        if result.returncode or tar_status:
            raise BackupError("Archive creation or age encryption failed")
    with destination.open("rb") as encrypted:
        if encrypted.read(22) != b"age-encryption.org/v1\n" or destination.stat().st_size <= 100:
            raise BackupError("Encrypted output is missing or invalid")


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path("/etc/endlessdrawing/backup.env"))
    parser.add_argument("--weekly", action="store_true", help="also create and verify a weekly copy")
    parser.add_argument("--local-only", action="store_true", help="verify and encrypt, but DO NOT claim off-site success")
    args = parser.parse_args()
    config = read_config(args.config, require_remote=not args.local_only)
    spool = Path(os.environ.get("ENDLESSDRAWING_BACKUP_DIR", "/var/lib/endlessdrawing-backup")).resolve()
    spool.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (spool / "upload.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise BackupError("Another backup upload is active") from None
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + secrets.token_hex(4)
        log_path = spool / f"storagebox-{stamp}.log"
        helper = os.environ.get("ENDLESSDRAWING_BACKUP_EXPORT", "/usr/local/sbin/endlessdrawing-backup-export")
        with log_path.open("xb") as log:
            exported = subprocess.run([helper, "export"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log, timeout=1800, check=True)
            bundle = Path(exported.stdout.decode().strip()).resolve()
            if bundle.parent != spool or not bundle.name.startswith("export-") or not bundle.is_dir():
                raise BackupError("Unexpected export location; no files were removed")
            subprocess.run([helper, "verify", str(bundle)], stdin=subprocess.DEVNULL, stdout=log, stderr=log, timeout=1900, check=True)
            cipher = spool / f"pg-{stamp}.tar.age"
            encrypt_bundle(bundle, cipher, config["BACKUP_AGE_RECIPIENT"], log)
            # Raw dump never goes to Storage Box. Keep ciphertext for retry if networking fails.
            shutil.rmtree(bundle)
            if args.local_only:
                print("LOCAL ONLY: encrypted export and restore drill passed; no off-site backup was made")
                return
            digest = StorageBox(config, log).upload(cipher, stamp, args.weekly or datetime.now(timezone.utc).weekday() == 6)
            state = {"last_success_utc": datetime.now(timezone.utc).isoformat(), "snapshot": stamp,
                     "ciphertext_sha256": digest, "restore_drill": True, "offsite": True}
            temporary = spool / "last-success.json.tmp"
            temporary.write_text(json.dumps(state) + "\n")
            temporary.replace(spool / "last-success.json")
            cipher.unlink()
        log_path.unlink()
        print(f"Off-site backup verified: {stamp}; rotation stays inside the dedicated backups namespace")


if __name__ == "__main__":
    try:
        main()
    except BackupError as error:
        # These messages are fixed local diagnostics, never child output or config values.
        print(f"Backup FAILED: {error}. Off-site success was not recorded.", file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, UnicodeError, subprocess.SubprocessError):
        # Do not echo commands, key material, config contents, SQL errors or remote banners.
        print("Backup FAILED; inspect the private backup operation logs. Off-site success was not recorded.", file=sys.stderr)
        sys.exit(1)
