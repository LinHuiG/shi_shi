"""Offline release-boundary checks using only temporary synthetic files."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    'check_release', Path(__file__).resolve().parents[1] / 'scripts' / 'check_release.py')
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleasePrivacyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='shishi-release-check-')
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.repo = self.base / 'source'
        (self.repo / 'docs').mkdir(parents=True)
        self.root_patch = patch.object(release, 'ROOT', self.repo)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.env_patch = patch.dict(os.environ, {'SHISHI_PRIVATE_TERMS_FILE': ''})
        self.env_patch.start()
        self.addCleanup(self.env_patch.stop)

    def write(self, relative, content):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding='utf-8')
        return Path(relative)

    def test_default_rules_work_without_private_vocabulary(self):
        self.assertIsNone(release.load_private_terms())
        for name, text in [('.gitattributes', '* text=auto eol=lf'), ('rustfmt.toml', 'newline_style = "Unix"')]:
            self.assertEqual(release.check([self.write(name, text)]), [])
        path = self.write('docs/example.md', 'Generic public documentation.')
        self.assertEqual(release.check([path]), [])
        self.write(path, ' '.join(('ghp_' + 'a' * 36, '.'.join(('10', '20', '30', '40')))))
        errors = release.check([path])
        self.assertTrue(any('possible credential' in row for row in errors))
        self.assertTrue(any('network address' in row for row in errors))

    def test_external_terms_are_literal_case_insensitive_and_redacted(self):
        private = self.base / 'private-terms.txt'
        private.write_text('\ufeff# local-only synthetic rules\n\nLocal.Marker[demo]+\n', encoding='utf-8')
        os.environ['SHISHI_PRIVATE_TERMS_FILE'] = str(private)
        path = self.write('docs/example.md', 'LOCAL.MARKER[DEMO]+')
        errors = release.check([path])
        self.assertEqual(len(errors), 1)
        self.assertNotIn('LOCAL.MARKER', errors[0])
        self.write(path, 'LocalXMarkerdd')
        self.assertEqual(release.check([path]), [])

    def test_in_repository_dictionary_is_rejected(self):
        private = self.write('docs/local-terms.txt', 'SyntheticPrivateMarker')
        os.environ['SHISHI_PRIVATE_TERMS_FILE'] = str(self.repo / private)
        with self.assertRaisesRegex(RuntimeError, 'outside the repository'):
            release.load_private_terms()

    def test_unreadable_or_invalid_dictionary_fails_without_its_path(self):
        private = self.base / 'private-terms.txt'
        os.environ['SHISHI_PRIVATE_TERMS_FILE'] = str(private)
        for content in (None, b'\xff'):
            if content is not None:
                private.write_bytes(content)
            with self.assertRaises(RuntimeError) as raised:
                release.load_private_terms()
            self.assertEqual(str(raised.exception), 'Private terms file could not be read.')

    def test_empty_dictionary_does_not_match_every_line(self):
        private = self.base / 'private-terms.txt'
        private.write_text('# no rules\n\n', encoding='utf-8')
        os.environ['SHISHI_PRIVATE_TERMS_FILE'] = str(private)
        self.assertIsNone(release.load_private_terms())


if __name__ == '__main__':
    unittest.main()
