#!/usr/bin/env python3
"""Exercise the evidence exceptions with real Gitleaks 8.21.2 (Python 3.11+).

All credential-shaped controls are generated, unissued samples. Scanner output
is redacted and assertions expose only rule IDs and file/line metadata.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
FILES = (
    'docs/verification/codex-context-2026-10-28.json',
    'docs/verification/setup-prompt-acceptance-2026-10-28.json',
)
MARKER = '# Recorded MCP evidence:'
# Deterministic, unissued detection controls; never authentic credentials.
CONTROL = '9f4c6d2a8b1e7053c6a29d4f8e3b1075d2c49a86'
PAT_CONTROL = 'ghp_' + '7rN4sL8pQ2vW6xY9zB3cD5fG1hJ0kM2nP4qR'


class EvidenceExceptions(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.scanner = os.environ.get('GITLEAKS_BIN') or shutil.which('gitleaks')
        if not cls.scanner:
            raise RuntimeError('Install the checksum-verified Gitleaks 8.21.2 used by secret-scan CI')
        version = subprocess.run([cls.scanner, 'version'], capture_output=True, text=True, check=True)
        if version.stdout.strip() != '8.21.2':
            raise RuntimeError('These regression tests require the CI-pinned Gitleaks 8.21.2')
        cls.config = (ROOT / '.gitleaks.toml').read_text()
        parsed = tomllib.loads(cls.config)
        rule = next(r for r in parsed['rules'] if r['id'] == 'generic-api-key')
        cls.exceptions = rule['allowlists'][1:]  # Keep the older public constant exception.
        cls.values = {
            path: [expression[1:-1] for expression in entry['regexes']]
            for path, entry in zip(FILES, cls.exceptions)
        }

    def scan(self, files, baseline=False, history=False):
        with tempfile.TemporaryDirectory(prefix='shogun-gitleaks-') as tmp:
            root = Path(tmp)
            corpus = root / 'corpus'
            corpus.mkdir()
            config = self.config.split(MARKER)[0] if baseline else self.config
            config_path = root / 'config.toml'
            config_path.write_text(config)
            for name, text in files.items():
                dest = corpus / name
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(text)
            mode = 'dir'
            if history:
                def git(*args):
                    subprocess.run(['git', *args], cwd=corpus, check=True, capture_output=True)
                git('init', '-q')
                git('add', '.')
                git('-c', 'user.name=Scanner regression', '-c', 'user.email=scanner@example.invalid',
                    '-c', 'commit.gpgsign=false', 'commit', '-qm', 'Synthetic scanner corpus')
                mode = 'git'
            report = root / 'report.json'
            result = subprocess.run([
                self.scanner, mode, '.', '--config', str(config_path), '--redact',
                '--no-banner', '--exit-code', '1', '--report-format', 'json',
                '--report-path', str(report),
            ], cwd=corpus, capture_output=True, text=True)
            self.assertIn(result.returncode, (0, 1), 'Scanner execution failed')
            self.assertTrue(report.exists(), 'Scanner produced no report')
            findings = json.loads(report.read_text())
            self.assertEqual(result.returncode, int(bool(findings)))
            return [(r['RuleID'], r['File'], r['StartLine']) for r in findings]

    def test_exact_path_and_secret_conditions(self):
        self.assertEqual(len(self.exceptions), 2)
        for path, entry in zip(FILES, self.exceptions):
            self.assertEqual(entry['condition'], 'AND')
            self.assertNotIn('regexTarget', entry)  # Match detected secret, never whole line.
            self.assertEqual(set(entry), {'description', 'condition', 'paths', 'regexes'})
            self.assertEqual(entry['paths'], ['^' + path.replace('.', r'\.') + '$'])
            for value in entry['regexes']:
                self.assertRegex(value, r'^\^[A-Za-z0-9-]+\$$')

    def test_original_evidence_and_baseline(self):
        files = {path: (ROOT / path).read_text() for path in FILES}
        findings = self.scan(files, baseline=True)
        self.assertEqual(len(findings), 11)
        self.assertEqual({r[0] for r in findings}, {'generic-api-key'})
        self.assertEqual(self.scan(files), [])
        self.assertEqual(self.scan(files, history=True), [])

    def test_same_line_and_adjacent_credentials_remain_detected(self):
        for path in FILES:
            with self.subTest(path=path):
                text = json.dumps({'thread_key': self.values[path][0], 'api_key': CONTROL}) + '\n'
                text += json.dumps({'api_key': CONTROL[::-1]}) + '\n'
                findings = self.scan({path: text})
                self.assertEqual([(r[0], r[2]) for r in findings],
                                 [('generic-api-key', 1), ('generic-api-key', 2)])

    def test_other_detector_in_the_same_evidence_file(self):
        for path in FILES:
            findings = self.scan({path: json.dumps({'value': PAT_CONTROL})})
            self.assertIn('github-pat', [r[0] for r in findings])

    def test_value_outside_its_exact_file_still_detected(self):
        for index, path in enumerate(FILES):
            for target in ['docs/verification/another.json', 'prefix/' + path,
                           path + '.copy', FILES[1-index]]:
                with self.subTest(path=path, target=target):
                    findings = self.scan({target: json.dumps({'api_key': self.values[path][0]})})
                    self.assertEqual([r[0] for r in findings], ['generic-api-key'])

    def test_value_matching_is_anchored(self):
        for path in FILES:
            for value in self.values[path]:
                for changed in ['f9Q8r2T6v4W1-' + value, value + '-m3L7n5P0s9X2']:
                    self.assertEqual(len(self.scan({path: json.dumps({'api_key': changed})}, baseline=True)), 1)
                    findings = self.scan({path: json.dumps({'api_key': changed})})
                    self.assertEqual([r[0] for r in findings], ['generic-api-key'])

    def test_history_scan_does_not_hide_new_credentials(self):
        files = {path: (ROOT / path).read_text() + '\n' + json.dumps({'api_key': CONTROL})
                 for path in FILES}
        findings = self.scan(files, history=True)
        self.assertEqual(len(findings), 2)
        self.assertEqual({r[0] for r in findings}, {'generic-api-key'})


if __name__ == '__main__':
    unittest.main()
