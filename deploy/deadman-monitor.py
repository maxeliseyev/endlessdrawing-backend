#!/usr/bin/env python3
"""Off-VPS dead-man monitor. Run on a separate always-on host (not the app VPS).

The app VPS watchdog cannot report a fully down VPS or broken upstream network,
so this checks the public readiness endpoint from an independent host and alerts
the owner's Telegram chat after several consecutive failures, then once on
recovery. Token lives in an owner/root-owned private (0600) config, never in
argv, environment, CI or logs.

configure MUST run in the owner's trusted interactive TTY (like the VPS monitor).
"""
import argparse
import getpass
import json
import os
import re
import shlex
import ssl
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

SITE = 'https://endlessdrawing.com/readyz'
IS_MAC = sys.platform == 'darwin'
DEFAULT_CONF = (Path.home() / '.config' / 'endlessdrawing' / 'deadman.conf'
                if IS_MAC else '/etc/endlessdrawing/deadman.env')
SERVICE = 'endlessdrawing-deadman'


class DeadmanError(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise DeadmanError('Redirect refused')


def private_file(path):
    return (path.is_file() and not path.is_symlink() and path.stat().st_uid == os.geteuid()
            and not path.stat().st_mode & 0o077)


def validate(config):
    if set(config) != {'TELEGRAM_BOT_TOKEN', 'TELEGRAM_CHAT_ID', 'CHECK_TIMEOUT_SECONDS', 'FAILURE_THRESHOLD'}:
        raise DeadmanError('Invalid dead-man configuration')
    if not re.fullmatch(r'[0-9]{5,20}:[A-Za-z0-9_-]{20,100}', config['TELEGRAM_BOT_TOKEN']):
        raise DeadmanError('Invalid bot token format')
    if not re.fullmatch(r'-?[0-9]{1,19}', config['TELEGRAM_CHAT_ID']) or abs(int(config['TELEGRAM_CHAT_ID'])) >= 2**63:
        raise DeadmanError('Invalid Telegram chat ID')
    for key in ('CHECK_TIMEOUT_SECONDS', 'FAILURE_THRESHOLD'):
        if not re.fullmatch(r'[0-9]{1,3}', config.get(key, '')) or int(config[key]) < 1:
            raise DeadmanError('Invalid ' + key)
    return config


def read_config(path):
    if not private_file(path) or path.stat().st_size > 8192:
        raise DeadmanError('Config must be owner-owned private (0600)')
    result = {}
    for line in path.read_text().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        key, separator, raw = line.partition('=')
        fields = shlex.split(raw, comments=True)
        if not separator or key in result or len(fields) != 1:
            raise DeadmanError('Invalid dead-man configuration')
        result[key] = fields[0]
    return validate(result)


def telegram(token, method, data):
    if method not in {'getMe', 'sendMessage'}:
        raise DeadmanError('Unsupported Telegram operation')
    request = urllib.request.Request('https://api.telegram.org/bot' + token + '/' + method,
        data=json.dumps(data).encode(), headers={'Content-Type': 'application/json'}, method='POST')
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
    try:
        with opener.open(request, timeout=15) as reply:
            raw = reply.read(256 * 1024 + 1)
        if len(raw) > 256 * 1024:
            raise DeadmanError('Telegram response exceeded limit')
        result = json.loads(raw)
        if not isinstance(result, dict) or result.get('ok') is not True:
            raise DeadmanError('Telegram rejected the request')
        return result['result']
    except (OSError, ValueError, KeyError, urllib.error.URLError):
        raise DeadmanError('Telegram request failed; token/URL and provider response were not logged') from None


def send(token, chat_id, text):
    result = telegram(token, 'sendMessage', {
        'chat_id': chat_id, 'text': text,
        'link_preview_options': {'is_disabled': True}, 'disable_notification': False,
    })
    if not isinstance(result, dict) or not isinstance(result.get('message_id'), int):
        raise DeadmanError('Telegram did not confirm acceptance')


def check_ready(timeout):
    request = urllib.request.Request(SITE, headers={'User-Agent': 'endlessdrawing-deadman/1'})
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
    try:
        with opener.open(request, timeout=timeout) as reply:
            body = json.loads(reply.read(4096))
        return isinstance(body, dict) and body.get('status') == 'ready'
    except (OSError, ValueError, urllib.error.URLError):
        return False


def decide(ok, failures, threshold):
    """Return (action, next_failures) where action is None/'failure'/'recovery'."""
    if ok:
        return ('recovery' if failures >= threshold else None), 0
    failures += 1
    return ('failure' if failures == threshold else None), failures


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
        raise DeadmanError('Invalid state file')
    value = json.loads(path.read_text())
    return value if isinstance(value, dict) else {}


def state_path():
    return (Path.home() / '.local' / 'share' / 'endlessdrawing-deadman' / 'state.json'
            if IS_MAC else Path('/var/lib/endlessdrawing-deadman/state.json'))


def configure(path, chat_id=None):
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        raise DeadmanError('Configure only in your own trusted interactive TTY, not agent/CI')
    if not IS_MAC and os.geteuid() != 0:
        raise DeadmanError('Configure as root on this host')
    token = getpass.getpass('Telegram bot token (скрытый ввод, не отправляйте в чат): ').strip()
    validate({'TELEGRAM_BOT_TOKEN': token, 'TELEGRAM_CHAT_ID': '1', 'CHECK_TIMEOUT_SECONDS': '15', 'FAILURE_THRESHOLD': '3'})
    me = telegram(token, 'getMe', {})
    if not isinstance(me, dict) or not re.fullmatch(r'[A-Za-z0-9_]{5,64}', str(me.get('username', ''))):
        raise DeadmanError('Telegram bot identity was not validated')
    print('Бот: @' + me['username'])
    selected = chat_id if chat_id is not None else input('Chat ID для уведомлений: ').strip()
    config = validate({'TELEGRAM_BOT_TOKEN': token, 'TELEGRAM_CHAT_ID': selected,
                       'CHECK_TIMEOUT_SECONDS': '15', 'FAILURE_THRESHOLD': '3'})
    send(token, selected, '✅ Endless Drawing dead-man: тест с независимого хоста.')
    if input('Тестовое сообщение пришло в нужный чат? Введите yes: ').strip() != 'yes':
        raise DeadmanError('Подтверждение не получено; настройки не сохранены, timer не включён')
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', prefix='.deadman-', dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        output.write('TELEGRAM_BOT_TOKEN=' + token + '\n'
                     'TELEGRAM_CHAT_ID=' + selected + '\n'
                     'CHECK_TIMEOUT_SECONDS=15\n'
                     'FAILURE_THRESHOLD=3\n')
    temporary.chmod(0o600)
    temporary.replace(path)
    token = ''
    atomic_json(state_path(), {'test_confirmed_at': datetime.now(timezone.utc).isoformat()})
    if not IS_MAC:
        subprocess.run(['systemctl', 'enable', '--now', SERVICE + '.timer'], check=True, stdout=subprocess.DEVNULL)
        subprocess.run(['systemctl', 'start', SERVICE + '.service'], check=True, stdout=subprocess.DEVNULL)
        print('Dead-man настроен; systemd timer включён. Bot token не выведен.')
    else:
        print('Dead-man настроен. Убедитесь, что launchd agent загружен (см. docs/monitoring.md).')


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['configure', 'check', 'test', 'probe'])
    parser.add_argument('--config', type=Path, default=DEFAULT_CONF)
    parser.add_argument('--chat-id', help='non-secret destination ID, configure only')
    args = parser.parse_args()
    if args.operation == 'configure':
        configure(args.config, args.chat_id)
        return
    config = read_config(args.config)
    token = config['TELEGRAM_BOT_TOKEN']
    chat_id = config['TELEGRAM_CHAT_ID']
    if args.operation == 'test':
        send(token, chat_id, '✅ Endless Drawing dead-man: ручная проверка с независимого хоста.')
        print('Telegram accepted the dead-man test message.')
        return
    timeout = int(config['CHECK_TIMEOUT_SECONDS'])
    threshold = int(config['FAILURE_THRESHOLD'])
    state_path_current = state_path()
    if args.operation == 'probe':
        ok = check_ready(timeout)
        print(json.dumps({'site': SITE, 'ready': ok}, ensure_ascii=False))
        return
    ok = check_ready(timeout)
    now = datetime.now(timezone.utc).isoformat()
    state = read_state(state_path_current) if state_path_current.exists() else {}
    failures = int(state.get('failures', 0))
    action, failures = decide(ok, failures, threshold)
    if action == 'recovery':
        send(token, chat_id, '✅ Endless Drawing: сайт снова доступен (независимый dead-man).')
    elif action == 'failure':
        send(token, chat_id, '🚨 Endless Drawing: сайт недоступен с независимого хоста.')
    state.update(failures=failures, checked_at=now)
    atomic_json(state_path_current, state)
    print('Dead-man check:', 'ready' if ok else 'unreachable', '| consecutive failures:', failures)


if __name__ == '__main__':
    try:
        main()
    except DeadmanError as error:
        print('Dead-man: ' + str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, TypeError, KeyError, urllib.error.URLError, subprocess.SubprocessError, EOFError):
        print('Dead-man failed; token, URLs and provider responses were not logged.', file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print('Cancelled; no secrets printed.', file=sys.stderr)
        sys.exit(1)
