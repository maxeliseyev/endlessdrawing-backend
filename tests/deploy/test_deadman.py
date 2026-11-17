import importlib.util
import os
import stat
import tempfile
import unittest
from pathlib import Path

DEADMAN = Path(__file__).resolve().parents[2] / 'deploy' / 'deadman-monitor.py'
spec = importlib.util.spec_from_file_location('deadman', DEADMAN)
deadman = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deadman)


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


class DeadmanConfTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.conf = self.root / 'deadman.conf'

    def tearDown(self):
        self.directory.cleanup()

    def write_conf(self, text):
        self.conf.write_text(text)
        os.chmod(self.conf, 0o600)

    def test_rejects_non_owner_or_world_readable_conf(self):
        self.write_conf('TELEGRAM_CHAT_ID=15494483\nCHECK_TIMEOUT_SECONDS=15\nFAILURE_THRESHOLD=3\n')
        self.conf.chmod(0o644)
        with self.assertRaises(deadman.DeadmanError):
            deadman.read_conf(self.conf)

    def test_rejects_missing_or_invalid_fields(self):
        for text in (
            '',
            'TELEGRAM_CHAT_ID=abc\nCHECK_TIMEOUT_SECONDS=15\nFAILURE_THRESHOLD=3\n',
            'TELEGRAM_CHAT_ID=15494483\nCHECK_TIMEOUT_SECONDS=15\n',
            'TELEGRAM_CHAT_ID=15494483\nCHECK_TIMEOUT_SECONDS=0\nFAILURE_THRESHOLD=3\n',
        ):
            with self.subTest(text=text):
                self.write_conf(text)
                with self.assertRaises(deadman.DeadmanError):
                    deadman.read_conf(self.conf)

    def test_accepts_valid_conf(self):
        self.write_conf('TELEGRAM_CHAT_ID=-10015494483\nCHECK_TIMEOUT_SECONDS=15\nFAILURE_THRESHOLD=3\n')
        self.assertEqual(deadman.read_conf(self.conf)['TELEGRAM_CHAT_ID'], '-10015494483')


if __name__ == '__main__':
    unittest.main()
