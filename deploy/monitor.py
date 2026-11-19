#!/usr/bin/env python3
"""Private VPS watchdog + Telegram. No log bodies, credentials, QR or drawing data.

configure MUST run in an owner's trusted TTY; never via agent/CI capture.
This is not an independent external dead-man monitor for a completely down VPS.
"""
import argparse
from datetime import datetime, timezone
import fcntl
import getpass
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request

CONFIG = Path('/etc/endlessdrawing/alerts.env')
SPOOL = Path('/var/lib/endlessdrawing-monitor')
BACKUPS = Path('/var/lib/endlessdrawing-backup')
SITE = 'endlessdrawing.com'


class MonitorError(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise MonitorError('Telegram redirect refused')


def private_file(path):
    return (path.is_file() and not path.is_symlink() and path.stat().st_uid == os.geteuid()
            and not path.stat().st_mode & 0o077)


def validate(config):
    if set(config) != {'TELEGRAM_BOT_TOKEN', 'TELEGRAM_CHAT_ID'}:
        raise MonitorError('Invalid notification configuration')
    if not re.fullmatch(r'[0-9]{5,20}:[A-Za-z0-9_-]{20,100}', config['TELEGRAM_BOT_TOKEN']):
        raise MonitorError('Invalid bot token format')
    if not re.fullmatch(r'-?[0-9]{1,19}', config['TELEGRAM_CHAT_ID']) or abs(int(config['TELEGRAM_CHAT_ID'])) >= 2**63:
        raise MonitorError('Invalid Telegram chat ID')
    return config


def read_config(path):
    if not private_file(path) or path.stat().st_size > 8192:
        raise MonitorError('Notification config must be operator-owned, bounded and private (0600)')
    result = {}
    for line in path.read_text().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        key, separator, raw = line.partition('=')
        fields = shlex.split(raw, comments=True)
        if not separator or key in result or len(fields) != 1:
            raise MonitorError('Invalid notification configuration')
        result[key] = fields[0]
    return validate(result)


def telegram(token, method, data):
    if method not in {'getMe', 'getUpdates', 'sendMessage'}:
        raise MonitorError('Unsupported Telegram operation')
    # Telegram requires its token in this HTTPS API path. Never log URL/exceptions/bodies.
    request = urllib.request.Request('https://api.telegram.org/bot' + token + '/' + method,
        data=json.dumps(data).encode(), headers={'Content-Type': 'application/json'}, method='POST')
    opener = urllib.request.build_opener(NoRedirect(), urllib.request.HTTPSHandler(context=ssl.create_default_context()))
    try:
        with opener.open(request, timeout=15) as reply:
            raw = reply.read(256 * 1024 + 1)
        if len(raw) > 256 * 1024:
            raise MonitorError('Telegram response exceeded limit')
        result = json.loads(raw)
        if not isinstance(result, dict) or result.get('ok') is not True:
            raise MonitorError('Telegram rejected the request')
        return result['result']
    except (OSError, ValueError, KeyError, urllib.error.URLError):
        raise MonitorError('Telegram request failed; token/URL and provider response were not logged') from None


def send(config, text):
    result = telegram(config['TELEGRAM_BOT_TOKEN'], 'sendMessage', {
        'chat_id': config['TELEGRAM_CHAT_ID'], 'text': text,
        'link_preview_options': {'is_disabled': True}, 'disable_notification': False,
    })
    if not isinstance(result, dict) or not isinstance(result.get('message_id'), int):
        raise MonitorError('Telegram did not confirm acceptance')


def atomic_json(path, value):
    with tempfile.NamedTemporaryFile(mode='w', prefix='.state-', dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        json.dump(value, output)
        output.write('\n')
    temporary.chmod(0o600)
    temporary.replace(path)


def read_json(path):
    if path.is_symlink() or path.stat().st_size > 65536:
        raise ValueError('Invalid state file')
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ValueError('Invalid state object')
    return value


def age_hours(value, now):
    elapsed = (now - datetime.fromisoformat(value)).total_seconds()
    if elapsed < -60:
        raise ValueError('Future timestamp')
    return max(0, elapsed) / 3600


def parse_quota(text):
    rows = text.strip().splitlines()
    if len(rows) != 2:
        raise ValueError('Invalid quota response')
    fields = rows[1].split()
    if len(fields) != 6 or not re.fullmatch(r'[0-9]+%', fields[4]):
        raise ValueError('Invalid quota fields')
    total, used, available = map(int, fields[1:4])
    percent = int(fields[4][:-1])
    if min(total, used, available) < 0 or total == 0 or max(used, available) > total or not 0 <= percent <= 100:
        raise ValueError('Invalid quota numbers')
    return percent, available * 1024


def box_quota():
    # Only public SSH destination/path fields are read; no password or private-key bytes.
    values = {}
    allowed = {'BACKUP_SSH_HOST', 'BACKUP_SSH_USER', 'BACKUP_SSH_IDENTITY_FILE', 'BACKUP_SSH_KNOWN_HOSTS_FILE'}
    path = Path('/etc/endlessdrawing/backup.env')
    if not private_file(path):
        raise MonitorError('Backup SSH configuration unavailable')
    for line in path.read_text().splitlines():
        key, separator, raw = line.partition('=')
        if separator and key in allowed:
            fields = shlex.split(raw, comments=True)
            if len(fields) != 1 or key in values:
                raise MonitorError('Invalid backup SSH destination')
            values[key] = fields[0]
    user = values.get('BACKUP_SSH_USER', '')
    host = values.get('BACKUP_SSH_HOST', '')
    if not re.fullmatch(r'u[0-9]+-sub[0-9]+', user) or host not in {user + '.your-storagebox.de', user.split('-sub')[0] + '.your-storagebox.de'}:
        raise MonitorError('Dedicated Storage Box subaccount required')
    args = ['ssh', '-n', '-F', '/dev/null', '-p', '23', '-i', values['BACKUP_SSH_IDENTITY_FILE'],
            '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'IdentityAgent=none',
            '-o', 'StrictHostKeyChecking=yes', '-o', 'UserKnownHostsFile=' + values['BACKUP_SSH_KNOWN_HOSTS_FILE'],
            '-o', 'ConnectTimeout=10', user + '@' + host, 'df -Pk backups']
    result = subprocess.run(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=20)
    if result.returncode or len(result.stdout) > 16384:
        raise MonitorError('Storage Box quota check failed')
    return parse_quota(result.stdout.decode())


def collect(now):
    issues = []
    details = []
    try:
        with urllib.request.urlopen('http://127.0.0.1:8080/readyz', timeout=5) as reply:
            body = json.loads(reply.read(4096))
            if not isinstance(body, dict) or body.get('status') != 'ready':
                raise ValueError('Not ready')
    except (OSError, ValueError, urllib.error.URLError):
        issues.append('Приложение или PostgreSQL не готовы (readyz).')
    try:
        with socket.create_connection(('127.0.0.1', 443), timeout=8) as connection:
            with ssl.create_default_context().wrap_socket(connection, server_hostname=SITE) as tls:
                remaining = (ssl.cert_time_to_seconds(tls.getpeercert()['notAfter']) - now.timestamp()) / 86400
        if remaining < 14:
            issues.append('TLS certificate истекает менее чем через 14 дней.')
    except (OSError, ValueError, KeyError, ssl.SSLError):
        issues.append('Не удалось проверить доверенный HTTPS certificate.')
    try:
        disk = shutil.disk_usage('/var/lib/docker')
        if disk.total <= 0: raise OSError('Invalid filesystem size')
        free_gib = disk.free / 2**30
        if free_gib < 5 or disk.free / disk.total < 0.15:
            issues.append('Мало диска на VPS: менее 5 GiB или 15% свободно.')
        details.append(f'VPS disk free: {free_gib:.1f} GiB')
    except OSError:
        issues.append('Не удалось проверить диск Docker на VPS.')
    latest = None
    try:
        latest = read_json(BACKUPS / 'last-success.json')
        if latest.get('offsite') is not True or latest.get('restore_drill') is not True or age_hours(latest['last_success_utc'], now) > 36:
            raise ValueError('Stale or unverified')
        details.append('Off-site backup: ' + latest['last_success_utc'][:19] + ' UTC')
    except (OSError, ValueError, KeyError, TypeError):
        issues.append('Нет подтверждённого внешнего backup за 36 часов.')
        latest = None
    try:
        result = subprocess.run(['systemctl', 'show', 'endlessdrawing-backup.service', '--property=Result', '--value'],
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=5)
        if result.returncode or result.stdout.strip() != 'success':
            issues.append('Systemd backup job завершился с ошибкой или недоступен.')
    except (OSError, subprocess.SubprocessError):
        issues.append('Не удалось проверить systemd backup job.')
    try:
        proof = read_json(BACKUPS / 'last-operator-restore.json')
        if (proof.get('operator_age_decryption') is not True or proof.get('totp_key_restored') is not True
                or age_hours(proof['verified_utc'], now) > 35 * 24):
            raise ValueError('Restore overdue')
    except (OSError, ValueError, KeyError, TypeError):
        issues.append('Нужен operator decrypt/restore drill (нет proof или старше 35 дней).')
    if (BACKUPS / 'totp-key-initialized').exists() and not Path('/opt/endlessdrawing/admin-secrets/totp.key').is_file():
        issues.append('Потерян private server TOTP encryption file; не генерируйте новый вместо него.')
    try:
        percent, available = box_quota()
        details.append(f'Storage Box: {percent}% used, {available / 2**30:.1f} GiB free')
        if percent >= 85 or available < 5 * 2**30:
            issues.append('Мало места на Storage Box (85% used или менее 5 GiB свободно).')
    except (OSError, ValueError, KeyError, MonitorError, subprocess.SubprocessError):
        issues.append('Не удалось проверить квоту Storage Box.')
    return sorted(set(issues)), details, latest


def notification(state, issues, details, latest, now):
    """At most one same-state reminder / 6h; recover once, daily green after 04:20 UTC."""
    old_issues = state.get('issues', [])
    if issues:
        try:
            due = age_hours(state['sent_at'], now) >= 6
        except (ValueError, KeyError, TypeError):
            due = True
        if old_issues != issues or due:
            return '🚨 Endless Drawing\n' + '\n'.join('• ' + item for item in issues + details), 'failure'
        return None
    if old_issues:
        return '✅ Endless Drawing: восстановлено\n' + '\n'.join(details), 'recovery'
    day = now.date().isoformat()
    if (latest and latest['last_success_utc'].startswith(day) and (now.hour, now.minute) >= (4, 20)
            and state.get('daily_sent') != day):
        return '✅ Endless Drawing: ежедневная проверка\nBackup upload SHA-256 + local pg_restore: OK\n' + '\n'.join(details), 'daily'
    return None


REASONS = {'sexual': 'сексуальное', 'violence': 'насилие', 'hate': 'ненависть/травля',
           'illegal': 'незаконное', 'spam': 'спам', 'other': 'другое'}
OVERDUE_S = 20 * 3600


def moderation_summary():
    """Counts from the app's loopback-only endpoint; never strokes, ids or digests."""
    with urllib.request.urlopen('http://127.0.0.1:8080/internal/moderation/summary', timeout=5) as reply:
        body = json.loads(reply.read(8192))
    keys = ('open_reports', 'held', 'latest_report_seq', 'oldest_open_age_s')
    if not isinstance(body, dict) or not all(isinstance(body.get(key), int) and body[key] >= 0 for key in keys):
        raise ValueError('Invalid moderation summary')
    reasons = body.get('open_by_reason', {})
    if not isinstance(reasons, dict) or not all(key in REASONS and isinstance(value, int) for key, value in reasons.items()):
        raise ValueError('Invalid moderation summary')
    return body


def report_notice(state, summary, now):
    """One message per batch of new reports; a reminder when one waits >20 h (at most every 6 h).
    Returns (text, new_state) or (None, new_state). The first run only records the position,
    unless something is already open."""
    seen = state.get('latest_report_seq')
    latest = summary['latest_report_seq']
    new_state = dict(state, latest_report_seq=latest)
    lines = []
    if (seen is None and summary['open_reports']) or (seen is not None and latest > seen):
        fresh = latest - seen if seen is not None else summary['open_reports']
        lines.append(f'🚩 Endless Drawing: новых жалоб — {fresh}.')
    overdue = summary['open_reports'] and summary['oldest_open_age_s'] >= OVERDUE_S
    reminded = state.get('overdue_at')
    if overdue and not lines and (not reminded or age_hours(reminded, now) >= 6):
        lines.append('⏰ Endless Drawing: жалоба ждёт решения больше 20 часов (обещано ≤ 24 ч).')
        new_state['overdue_at'] = now.isoformat()
    if not lines:
        return None, new_state
    reasons = ', '.join(f'{REASONS[key]} {value}' for key, value in sorted(summary['open_by_reason'].items()))
    lines.append(f'Открыто: {summary["open_reports"]}' + (f' ({reasons})' if reasons else '') + '.')
    if summary['held']:
        lines.append(f'Скрыто порогом до решения: {summary["held"]}.')
    lines.append(f'https://{SITE}/admin/')
    return '\n'.join(lines), new_state


def configure(path, chat_id=None):
    if os.geteuid() != 0 or not sys.stdin.isatty() or not sys.stdout.isatty():
        raise MonitorError('Configure only as root in your own trusted interactive TTY, not agent/CI')
    token = getpass.getpass('Telegram bot token (скрытый ввод, не отправляйте в чат): ').strip()
    validate({'TELEGRAM_BOT_TOKEN': token, 'TELEGRAM_CHAT_ID': '1'})
    me = telegram(token, 'getMe', {})
    if not isinstance(me, dict) or not re.fullmatch(r'[A-Za-z0-9_]{5,64}', str(me.get('username', ''))):
        raise MonitorError('Telegram bot identity was not validated')
    print('Бот: @' + me['username'])
    if chat_id is not None:
        selected = chat_id
        print('Назначение: chat_id ' + selected)
    else:
        updates = telegram(token, 'getUpdates', {'timeout': 0, 'allowed_updates': ['message']})
        chats = {}
        for update in updates if isinstance(updates, list) else []:
            if not isinstance(update, dict): continue
            chat = update.get('message', {}).get('chat', {})
            if chat.get('type') in {'private', 'group', 'supergroup'} and isinstance(chat.get('id'), int):
                chats[str(chat['id'])] = chat['type']
        if not chats:
            raise MonitorError('Откройте своего бота в Telegram, нажмите Start / отправьте /start и повторите настройку. Webhooks/чужие bots не меняются.')
        print('Доступные чаты (ID → тип):')
        for chat, kind in chats.items(): print(chat + ' → ' + kind)
        selected = input('ID чата для уведомлений: ').strip()
        if selected not in chats: raise MonitorError('Выберите один из обнаруженных чатов')
    config = validate({'TELEGRAM_BOT_TOKEN': token, 'TELEGRAM_CHAT_ID': selected})
    send(config, '✅ Endless Drawing: тест уведомлений. Секреты и данные рисунков сюда не отправляются.')
    if input('Тестовое сообщение пришло в нужный чат? Введите yes: ').strip() != 'yes':
        raise MonitorError('Подтверждение не получено; настройки не сохранены, timer не включён')
    if path.is_symlink():
        raise MonitorError('Refusing a symlink config')
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', prefix='.alerts-', dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        output.write('TELEGRAM_BOT_TOKEN=' + token + '\nTELEGRAM_CHAT_ID=' + selected + '\n')
    temporary.chmod(0o600); temporary.replace(path)
    token = ''; config.clear()
    SPOOL.mkdir(mode=0o700, parents=True, exist_ok=True)
    if args.operation == 'reports':
        with (SPOOL / 'reports.lock').open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return
            path = SPOOL / 'reports.json'
            try:
                state = read_json(path)
            except (OSError, ValueError):
                state = {}
            try:
                summary = moderation_summary()
            except (OSError, ValueError, urllib.error.URLError):
                return  # The 15-minute health check reports an unavailable app.
            text, new_state = report_notice(state, summary, now)
            if text:
                send(config, text)  # Persist only after Telegram accepted it, so a failure retries.
            atomic_json(path, new_state)
        return
    with (SPOOL / 'monitor.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        atomic_json(SPOOL / 'state.json', {'test_confirmed_at': datetime.now(timezone.utc).isoformat()})
    subprocess.run(['systemctl', 'enable', '--now', 'endlessdrawing-monitor.timer'], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(['systemctl', 'start', 'endlessdrawing-monitor.service'], check=True, stdout=subprocess.DEVNULL)
    print('Telegram настроен; timer включён. Bot token не выведен и не передан приложению.')


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['configure', 'check', 'test', 'drill', 'probe', 'reports'])
    parser.add_argument('--config', type=Path, default=CONFIG)
    parser.add_argument('--chat-id', help='non-secret destination ID, configure only')
    args = parser.parse_args()
    if args.operation == 'configure':
        configure(args.config, args.chat_id); return
    now = datetime.now(timezone.utc)
    if args.operation == 'probe':
        issues, details, _ = collect(now)
        print(json.dumps({'issues': issues, 'details': details}, ensure_ascii=False))
        return
    config = read_config(args.config)
    if args.operation == 'test':
        send(config, '✅ Endless Drawing: ручная проверка Telegram (без секретов и log bodies).')
        print('Telegram accepted the test message.'); return
    if args.operation == 'drill':
        prefix = '🧪 TEST — это проверка доставки, реального сбоя нет.\n'
        issues = ['Тест: подтверждённый backup условно старше 36 часов.']
        failed = notification({}, issues, [], None, now)
        recovered = notification({'issues': issues}, [], [], None, now)
        send(config, prefix + failed[0])
        send(config, prefix + recovered[0])
        print('Telegram accepted simulated failure and recovery. Actual monitoring state was not changed.'); return
    SPOOL.mkdir(mode=0o700, parents=True, exist_ok=True)
    if args.operation == 'reports':
        with (SPOOL / 'reports.lock').open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return
            path = SPOOL / 'reports.json'
            try:
                state = read_json(path)
            except (OSError, ValueError):
                state = {}
            try:
                summary = moderation_summary()
            except (OSError, ValueError, urllib.error.URLError):
                return  # The 15-minute health check reports an unavailable app.
            text, new_state = report_notice(state, summary, now)
            if text:
                send(config, text)  # Persist only after Telegram accepted it, so a failure retries.
            atomic_json(path, new_state)
        return
    with (SPOOL / 'monitor.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        path = SPOOL / 'state.json'
        try:
            state = read_json(path)
        except (OSError, ValueError):
            state = {}
        issues, details, latest = collect(now)
        action = notification(state, issues, details, latest, now)
        if action:
            text, kind = action
            send(config, text)  # A failed send MUST NOT persist success/suppress the retry.
            state.update(issues=issues, sent_at=now.isoformat())
            if kind == 'daily' or (kind == 'recovery' and latest and latest['last_success_utc'].startswith(now.date().isoformat())):
                state['daily_sent'] = now.date().isoformat()
            atomic_json(path, state)
            print('Telegram notification accepted: ' + kind)
        else:
            print('Monitoring checked; no notification due.')


if __name__ == '__main__':
    try:
        main()
    except MonitorError as error:
        print('Monitor: ' + str(error), file=sys.stderr)
        sys.exit(1)
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError, EOFError):
        print('Monitor failed; configuration values, URLs and provider responses were not logged.', file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print('Cancelled; no secrets printed.', file=sys.stderr)
        sys.exit(1)
