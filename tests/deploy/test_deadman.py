import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

DEADMAN = Path(__file__).resolve().parents[2] / 'deploy' / 'deadman-monitor.py'
spec = importlib.util.spec_from_file_location('deadman', DEADMAN)
deadman = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deadman)

TOKEN = '12345:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh'
GOOD = {'TELEGRAM_BOT_TOKEN': TOKEN, 'TELEGRAM_CHAT_ID': '15494483',
        'CHECK_TIMEOUT_SECONDS': '15', 'FAILURE_THRESHOLD': '3'}


class DeadmanDecisionTests(unittest.TestCase):
    def test_threshold_only_after_consecutive_failures_and_single_recovery(self):
        threshold = 3
        self.assertEqual(deadman.decide(True, 0, threshold), (None, 0))
        self.assertEqual(deadman.decide(False, 0, threshold), (None, 1))
        self.assertEqual(deadman.decide(False, 1, threshold), (None, 2))
        self.assertEqual(deadman.decide(False, 2, threshold), ('failure', 3))
        # Further failures while already down do not spam.
        self.assertEqual(deadman.decide(False, 3, threshold), (None, 4))
        # Recovery is sent exactly once, then the counter resets.
        self.assertEqual(deadman.decide(True, 4, threshold), ('recovery', 0))
        self.assertEqual(deadman.decide(True, 0, threshold), (None, 0))


class DeadmanValidateTests(unittest.TestCase):
    def test_accepts_valid_config(self):
        self.assertEqual(deadman.validate(dict(GOOD)), GOOD)

    def test_rejects_invalid_token_chat_id_and_thresholds(self):
        for patch, field in [
            ({'TELEGRAM_BOT_TOKEN': 'not-a-token'}, 'token'),
            ({'TELEGRAM_BOT_TOKEN': '123:short'}, 'token'),
            ({'TELEGRAM_CHAT_ID': 'abc'}, 'chat'),
            ({'CHECK_TIMEOUT_SECONDS': '0'}, 'timeout'),
            ({'FAILURE_THRESHOLD': '0'}, 'threshold'),
            ({'FAILURE_THRESHOLD': ''}, 'missing threshold'),
        ]:
            with self.subTest(field=field):
                config = dict(GOOD)
                config.update(patch)
                with self.assertRaises(deadman.DeadmanError):
                    deadman.validate(config)

    def test_rejects_unknown_or_missing_fields(self):
        with self.assertRaises(deadman.DeadmanError):
            deadman.validate({'TELEGRAM_BOT_TOKEN': TOKEN, 'TELEGRAM_CHAT_ID': '1',
                              'CHECK_TIMEOUT_SECONDS': '15', 'FAILURE_THRESHOLD': '3', 'EXTRA': 'x'})


class DeadmanReadConfigTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.conf = self.root / 'deadman.env'

    def tearDown(self):
        self.directory.cleanup()

    def write_conf(self, text):
        self.conf.write_text(text)
        os.chmod(self.conf, 0o600)

    def test_rejects_world_readable_config(self):
        self.write_conf('\n'.join(f'{k}={v}' for k, v in GOOD.items()) + '\n')
        self.conf.chmod(0o644)
        with self.assertRaises(deadman.DeadmanError):
            deadman.read_config(self.conf)

    def test_rejects_missing_token_or_threshold(self):
        lines = {k: v for k, v in GOOD.items() if k != 'TELEGRAM_BOT_TOKEN'}
        self.write_conf('\n'.join(f'{k}={v}' for k, v in lines.items()) + '\n')
        with self.assertRaises(deadman.DeadmanError):
            deadman.read_config(self.conf)

    def test_accepts_valid_private_config(self):
        self.write_conf('\n'.join(f'{k}={v}' for k, v in GOOD.items()) + '\n')
        self.assertEqual(deadman.read_config(self.conf), GOOD)


if __name__ == '__main__':
    unittest.main()
