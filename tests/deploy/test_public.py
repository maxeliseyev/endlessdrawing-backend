import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('smoke_public', Path(__file__).with_name('smoke_public.py'))
public = importlib.util.module_from_spec(spec)
spec.loader.exec_module(public)


class PublicDatabaseSafetyTests(unittest.TestCase):
    def test_only_explicit_loopback_postgres_urls_without_overrides_are_accepted(self):
        for value in ['postgres://fixture:fixture@production/canvas', 'sqlite:///data',
                      'postgres://fixture:fixture@127.0.0.1/canvas?host=production',
                      'postgres://fixture:fixture@127.0.0.1/canvas#override',
                      'postgres://127.0.0.1/canvas']:
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                public.local_database_source(value)

    def test_fixture_url_is_parsed_without_printing_credentials(self):
        source = public.local_database_source('postgres://fixture:fixture@127.0.0.1:55432/canvas')
        self.assertEqual(source.hostname, '127.0.0.1')
        self.assertEqual(source.port, 55432)
        self.assertEqual(source.path, '/canvas')


if __name__ == '__main__':
    unittest.main()
