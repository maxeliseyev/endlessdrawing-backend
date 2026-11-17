import contextlib
from datetime import datetime, timedelta, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

ROOT=Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('monitor',ROOT/'deploy/monitor.py')
monitor=importlib.util.module_from_spec(spec)
spec.loader.exec_module(monitor)
NOW=datetime(2026,11,17,8,0,tzinfo=timezone.utc)


class MonitorTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        # Constructed TEST fixture, not a real bot or credential.
        self.config={'TELEGRAM_BOT_TOKEN':'123456789'+':'+'A'*35,'TELEGRAM_CHAT_ID':'12345'}
        self.latest={'last_success_utc':NOW.isoformat(),'offsite':True,'restore_drill':True}

    def test_config_is_private_data_not_shell_and_symlinks_are_rejected(self):
        path=self.root/'alerts.env'
        path.write_text('\n'.join(key+'='+value for key,value in self.config.items())); path.chmod(0o600)
        self.assertEqual(monitor.read_config(path),self.config)
        path.chmod(0o644)
        with self.assertRaises(monitor.MonitorError): monitor.read_config(path)
        path.chmod(0o600)
        link=self.root/'link'; link.symlink_to(path)
        with self.assertRaises(monitor.MonitorError): monitor.read_config(link)
        path.write_text('TELEGRAM_BOT_TOKEN=$(untrusted)\nTELEGRAM_CHAT_ID=12345\n')
        with self.assertRaises(monitor.MonitorError): monitor.read_config(path)

    def test_chat_ids_and_tokens_are_validated(self):
        for bad in ['bad','https://example.test',self.config['TELEGRAM_BOT_TOKEN']+';command']:
            with self.assertRaises(monitor.MonitorError): monitor.validate({**self.config,'TELEGRAM_BOT_TOKEN':bad})
        for bad in ['@channel','1;command',str(2**63)]:
            with self.assertRaises(monitor.MonitorError): monitor.validate({**self.config,'TELEGRAM_CHAT_ID':bad})
        self.assertEqual(monitor.validate({**self.config,'TELEGRAM_CHAT_ID':'-10012345'})['TELEGRAM_CHAT_ID'],'-10012345')

    def test_quota_parser_checks_shape_and_bounds(self):
        self.assertEqual(monitor.parse_quota('Filesystem 1K-blocks Used Available Use% Mounted on\nbox 1000 100 900 10% /home\n'),(10,900*1024))
        for text in ['', 'welcome', 'header\nbox 1000 100 900 101% /home','header\nbox 1000 -1 900 10% /home','header\nbox 1000 100 9000 10% /home']:
            with self.assertRaises(ValueError): monitor.parse_quota(text)

    def test_quota_uses_only_readonly_command_with_dedicated_pinned_ssh(self):
        text='BACKUP_SSH_HOST=u12345-sub1.your-storagebox.de\nBACKUP_SSH_USER=u12345-sub1\nBACKUP_SSH_IDENTITY_FILE=/private/fixture-identity\nBACKUP_SSH_KNOWN_HOSTS_FILE=/private/fixture-pin\n'
        result=subprocess.CompletedProcess([],0,b'header\nbox 1000 100 900 10% /home\n',b'')
        with patch.object(monitor,'private_file',return_value=True), patch.object(Path,'read_text',return_value=text), \
             patch.object(monitor.subprocess,'run',return_value=result) as run:
            self.assertEqual(monitor.box_quota(),(10,900*1024))
        args=run.call_args.args[0]
        self.assertEqual(args[-1],'df -Pk backups')
        for option in ['StrictHostKeyChecking=yes','IdentitiesOnly=yes','IdentityAgent=none','UserKnownHostsFile=/private/fixture-pin']:
            self.assertIn(option,args)
        self.assertEqual(run.call_args.kwargs['stdin'],subprocess.DEVNULL)

    def test_failure_dedup_reminder_recovery_and_daily_rules(self):
        issues=['backup stale']
        text,kind=monitor.notification({},issues,['disk free'],None,NOW)
        self.assertEqual(kind,'failure'); self.assertIn('backup stale',text)
        sent={'issues':issues,'sent_at':NOW.isoformat()}
        self.assertIsNone(monitor.notification(sent,issues,[],None,NOW+timedelta(minutes=15)))
        self.assertEqual(monitor.notification(sent,issues,[],None,NOW+timedelta(hours=6))[1],'failure')
        self.assertEqual(monitor.notification(sent,['disk low'],[],None,NOW+timedelta(minutes=1))[1],'failure')
        self.assertEqual(monitor.notification(sent,[],[],self.latest,NOW)[1],'recovery')
        self.assertEqual(monitor.notification({},[],[],self.latest,NOW)[1],'daily')
        self.assertIsNone(monitor.notification({'daily_sent':'2026-11-17'},[],[],self.latest,NOW))
        self.assertIsNone(monitor.notification({},[],[],self.latest,NOW.replace(hour=4,minute=19)))

    def test_future_or_missing_backup_time_is_not_healthy(self):
        with self.assertRaises(ValueError): monitor.age_hours((NOW+timedelta(hours=1)).isoformat(),NOW)
        self.assertEqual(monitor.age_hours((NOW-timedelta(hours=37)).isoformat(),NOW),37)

    def test_telegram_errors_do_not_expose_token_url_or_provider_body(self):
        error=urllib.error.HTTPError('https://api.telegram.org/bot'+self.config['TELEGRAM_BOT_TOKEN']+'/sendMessage',401,'provider secret',{},None)
        with patch.object(monitor.urllib.request,'build_opener') as opener:
            opener.return_value.open.side_effect=error
            with self.assertRaises(monitor.MonitorError) as failure:
                monitor.send(self.config,'plain operational notice')
            self.assertNotIn(self.config['TELEGRAM_BOT_TOKEN'],str(failure.exception))
            self.assertNotIn('provider secret',str(failure.exception))
            request=opener.return_value.open.call_args.args[0]
            body=json.loads(request.data)
            self.assertEqual(body['chat_id'],self.config['TELEGRAM_CHAT_ID'])
            self.assertNotIn(self.config['TELEGRAM_BOT_TOKEN'],request.data.decode())
            self.assertNotIn('parse_mode',body)

    def test_redirects_are_never_followed(self):
        with self.assertRaises(monitor.MonitorError): monitor.NoRedirect().redirect_request(None,None,None,None,None,None)

    def test_failed_send_does_not_record_success_or_suppress_next_check(self):
        with patch.object(monitor,'SPOOL',self.root), patch.object(monitor,'read_config',return_value=self.config), \
             patch.object(monitor,'collect',return_value=(['backup failed'],[],None)), \
             patch.object(monitor,'send',side_effect=monitor.MonitorError('fixed failure')), \
             patch('sys.argv',['monitor','check']):
            with self.assertRaises(monitor.MonitorError): monitor.main()
        self.assertFalse((self.root/'state.json').exists())

    def test_success_state_does_not_contain_credentials(self):
        with patch.object(monitor,'SPOOL',self.root), patch.object(monitor,'read_config',return_value=self.config), \
             patch.object(monitor,'collect',return_value=(['backup failed'],['aggregate metric'],None)), \
             patch.object(monitor,'send') as send, patch('sys.argv',['monitor','check']), contextlib.redirect_stdout(io.StringIO()):
            monitor.main(); monitor.main()
            self.assertEqual(send.call_count,1)
        raw=(self.root/'state.json').read_text()
        self.assertNotIn(self.config['TELEGRAM_BOT_TOKEN'],raw)
        self.assertEqual((self.root/'state.json').stat().st_mode&0o777,0o600)

    def test_delivery_drill_is_marked_test_and_does_not_mutate_health_state(self):
        path=self.root/'state.json'; path.write_text('{"issues":["existing"]}')
        before=path.read_bytes()
        with patch.object(monitor,'SPOOL',self.root), patch.object(monitor,'read_config',return_value=self.config), \
             patch.object(monitor,'send') as send, patch('sys.argv',['monitor','drill']), contextlib.redirect_stdout(io.StringIO()):
            monitor.main()
        self.assertEqual(send.call_count,2)
        self.assertTrue(all(call.args[1].startswith('🧪 TEST') for call in send.call_args_list))
        self.assertEqual(path.read_bytes(),before)

    def test_configure_refuses_agent_or_ci_non_tty_before_reading_token(self):
        with patch.object(monitor.sys.stdin,'isatty',return_value=False), patch.object(monitor.getpass,'getpass') as prompt:
            with self.assertRaises(monitor.MonitorError): monitor.configure(self.root/'never-written','12345')
            prompt.assert_not_called()
        self.assertFalse((self.root/'never-written').exists())

    def test_collector_checks_current_and_failed_operational_states(self):
        from types import SimpleNamespace
        proof={'operator_age_decryption':True,'totp_key_restored':True,'verified_utc':NOW.isoformat()}
        with patch.object(monitor,'BACKUPS',self.root), patch.object(monitor,'read_json',side_effect=lambda path: self.latest if path.name=='last-success.json' else proof), \
             patch.object(monitor.urllib.request,'urlopen') as http, patch.object(monitor.socket,'create_connection'), \
             patch.object(monitor.ssl,'create_default_context') as tls, patch.object(monitor.shutil,'disk_usage') as disk, \
             patch.object(monitor.subprocess,'run') as systemd, patch.object(monitor,'box_quota') as box:
            http.return_value.__enter__.return_value.read.return_value=b'{"status":"ready"}'
            certificate=tls.return_value.wrap_socket.return_value.__enter__.return_value
            certificate.getpeercert.return_value={'notAfter':'Feb 15 08:06:08 2027 GMT'}
            disk.return_value=SimpleNamespace(free=90*2**30,total=100*2**30)
            systemd.return_value=subprocess.CompletedProcess([],0,'success\n','')
            box.return_value=(1,900*2**30)
            self.assertEqual(monitor.collect(NOW)[0],[])
            self.latest['last_success_utc']=(NOW-timedelta(hours=37)).isoformat()
            proof['verified_utc']=(NOW-timedelta(days=36)).isoformat()
            certificate.getpeercert.return_value={'notAfter':'Nov 18 00:00:00 2026 GMT'}
            disk.return_value=SimpleNamespace(free=4*2**30,total=100*2**30)
            systemd.return_value=subprocess.CompletedProcess([],0,'exit-code\n','')
            box.return_value=(95,4*2**30)
            http.side_effect=OSError('unavailable')
            (self.root/'totp-key-initialized').touch()
            issues,_,latest=monitor.collect(NOW)
            self.assertIsNone(latest)
            self.assertEqual(len(issues),8)
            self.assertNotIn(self.config['TELEGRAM_BOT_TOKEN'],' '.join(issues))
            # A missing filesystem / malformed probe must still produce an alert, not abort collection.
            disk.side_effect=OSError('missing filesystem')
            http.side_effect=None
            http.return_value.__enter__.return_value.read.return_value=b'[]'
            issues,_,_=monitor.collect(NOW)
            self.assertTrue(any('диск Docker' in issue for issue in issues))
            self.assertTrue(any('readyz' in issue for issue in issues))

    def test_manual_config_does_not_poll_or_modify_webhooks_and_needs_receipt_confirmation(self):
        class TtyCapture(io.StringIO):
            def isatty(self): return True
        with patch.object(monitor.os,'geteuid',return_value=0), patch.object(monitor.sys.stdin,'isatty',return_value=True), \
             patch.object(monitor.sys.stdout,'isatty',return_value=True), patch.object(monitor.getpass,'getpass',return_value=self.config['TELEGRAM_BOT_TOKEN']), \
             patch.object(monitor,'telegram',return_value={'username':'fixture_bot'}) as api, patch.object(monitor,'send') as send, \
             patch('builtins.input',return_value='no'), contextlib.redirect_stdout(TtyCapture()):
            with self.assertRaises(monitor.MonitorError): monitor.configure(self.root/'never-written',self.config['TELEGRAM_CHAT_ID'])
            self.assertEqual([call.args[1] for call in api.call_args_list],['getMe'])
            send.assert_called_once()
        self.assertFalse((self.root/'never-written').exists())


if __name__=='__main__': unittest.main()
