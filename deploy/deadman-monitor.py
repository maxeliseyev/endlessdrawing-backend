#!/usr/bin/env python3
"""Off-VPS dead-man monitor for the owner's always-on device. Complements the VPS watchdog.

The VPS-local watchdog cannot report a fully down VPS or a broken upstream
network, so this checks the public readiness endpoint from an independent
device. It alerts the owner's Telegram chat only after several consecutive
failures and sends one recovery message when the site returns.

The bot token is read from the macOS Keychain and never from this file, argv,
environment, CI, or logs. The chat ID is not secret and lives in a 0600 conf.
"""
import argparse
import json
import os
import re
import ssl
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

CONF = Path(os.environ.get('ENDLESSDRAWING_DEADMAN_CONF', Path.home() / '.config' / 'endlessdrawing' / 'deadman.conf'))
STATE = Path(os.environ.get('ENDLESSDRAWING_DEADMAN_STATE', Path.home() / '.local' / 'share' / 'endlessdrawing-deadman' / 'state.json'))
KEYCHAIN_SERVICE = 'endlessdrawing-deadman-token'
SITE = 'https://endlessdrawing.com/readyz'


class DeadmanError(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise DeadmanError('Redirect refused')


def read_conf(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o077:
        raise DeadmanError('Conf must be owner-owned private (0600)')
    result = {}
    for line in path.read_text().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        key, separator, raw = line.partition('=')
        if not separator or key in result:
            raise DeadmanError('Invalid conf')
        result[key] = raw.strip()
    if not re.fullmatch(r'-?[0-9]{1,19}', result.get('TELEGRAM_CHAT_ID', '')):
        raise DeadmanError('Missing/invalid TELEGRAM_CHAT_ID')
    for key in ('CHECK_TIMEOUT_SECONDS', 'FAILURE_THRESHOLD'):
        if not re.fullmatch(r'[0-9]{1,3}', result.get(key, '')):
            raise DeadmanError('Missing/invalid ' + key)
    if int(result['CHECK_TIMEOUT_SECONDS']) < 1 or int(result['FAILURE_THRESHOLD']) < 1:
        raise DeadmanError('CHECK_TIMEOUT_SECONDS and FAILURE_THRESHOLD must be at least 1')
    return result


def keychain_token():
    result = subprocess.run(['security', 'find-generic-password', '-s', KEYCHAIN_SERVICE, '-w'],
                            capture_output=True, text=True)
    if result.returncode != 0:
        raise DeadmanError('Store the bot token in Keychain first (docs/monitoring.md); token was not printed')
    token = result.stdout.strip()
    if not re.fullmatch(r'[0-9]{5,20}:[A-Za-z0-9_-]{20,100}', token):
        raise DeadmanError('Invalid bot token in Keychain')
    return token


def check_ready(timeout):
    request = urllib.request.Request(SITE, headers={'User-Agent': 'endlessdrawing-deadman/1'})
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
    try:
        with opener.open(request, timeout=timeout) as reply:
            raw = reply.read(4096)
            body = json.loads(raw)
        return isinstance(body, dict) and body.get('status') == 'ready'
    except (OSError, ValueError, urllib.error.URLError):
        return False


def send(token, chat_id, text):
    data = json.dumps({'chat_id': chat_id, 'text': text,
                       'link_preview_options': {'is_disabled': True}}).encode()
    request = urllib.request.Request('https://api.telegram.org/bot' + token + '/sendMessage',
                                     data=data, headers={'Content-Type': 'application/json'}, method='POST')
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
    with opener.open(request, timeout=15) as reply:
        result = json.loads(reply.read(256 * 1024 + 1))
    if not isinstance(result, dict) or result.get('ok') is not True:
        raise DeadmanError('Telegram rejected the request')


def atomic_json(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', prefix='.state-', dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        json.dump(value, output)
        output.write('\n')
    temporary.chmod(0o600)
    temporary.replace(path)


def read_state(path):
    if path.is_symlink() or path.stat().st_size > 4096:
        raise DeadmanError('Invalid state')
    value = json.loads(path.read_text())
    return value if isinstance(value, dict) else {}


def decide(ok, failures, threshold):
    """Return (action, next_failures) where action is None/'failure'/'recovery'."""
    if ok:
        return ('recovery' if failures >= threshold else None), 0
    failures += 1
    return ('failure' if failures == threshold else None), failures


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['check', 'test'])
    args = parser.parse_args()
    conf = read_conf(CONF)
    token = keychain_token()
    chat_id = conf['TELEGRAM_CHAT_ID']
    if args.operation == 'test':
        send(token, chat_id, '✅ Endless Drawing dead-man: тест с независимого устройства.')
        print('Telegram accepted the dead-man test message.')
        return
    timeout = int(conf['CHECK_TIMEOUT_SECONDS'])
    threshold = int(conf['FAILURE_THRESHOLD'])
    ok = check_ready(timeout)
    now = datetime.now(timezone.utc).isoformat()
    state = read_state(STATE) if STATE.exists() else {}
    failures = int(state.get('failures', 0))
    action, failures = decide(ok, failures, threshold)
    if action == 'recovery':
        send(token, chat_id, '✅ Endless Drawing: сайт снова доступен (независимый dead-man).')
    elif action == 'failure':
        send(token, chat_id, '🚨 Endless Drawing: сайт недоступен с независимого устройства.')
    state.update(failures=failures, checked_at=now)
    atomic_json(STATE, state)
    print('Dead-man check:', 'ready' if ok else 'unreachable', '| consecutive failures:', failures)


if __name__ == '__main__':
    try:
        main()
    except DeadmanError as error:
        print('Dead-man: ' + str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, TypeError, KeyError, urllib.error.URLError, subprocess.SubprocessError):
        print('Dead-man failed; token, URLs and provider responses were not logged.', file=sys.stderr)
        sys.exit(1)
