#!/usr/bin/env python3
"""Private monitoring check; never a public HTTP endpoint or proof of decryptability."""
import json
from datetime import datetime, timezone
from pathlib import Path
import sys


def healthy(path, now=None):
    try:
        state = json.loads(path.read_text())
        completed = datetime.fromisoformat(state["last_success_utc"])
        age = ((now or datetime.now(timezone.utc)) - completed).total_seconds()
        return state.get("offsite") is True and state.get("restore_drill") is True and 0 <= age <= 36 * 3600
    except (OSError, ValueError, TypeError, KeyError):
        return False


if __name__ == "__main__":
    ok = healthy(Path("/var/lib/endlessdrawing-backup/last-success.json"))
    print("Backup upload/restore drill is recent" if ok else "Backup missing, stale, invalid or not off-site verified")
    sys.exit(0 if ok else 1)
