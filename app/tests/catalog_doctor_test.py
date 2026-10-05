"""Explicit stopped-catalog reconciliation never launches a configured agent."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

class CatalogDoctorTests(unittest.TestCase):
    def test_explicit_reconciliation_refuses_active_guard_and_does_not_probe(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'source').mkdir()
            (root / 'runs').mkdir()
            config = root / 'config.json'
            config.write_text(json.dumps({
                'workspace_root': str(root / 'runs'),
                'repositories': {'fixture': str(root / 'source')},
                'agents': {'fake': {'program': '/bin/sh', 'args': ['-c', 'exit 97']}},
            }))
            binary = os.environ.get('RELAY_APP_BINARY', str(ROOT / 'target/debug/relay-app'))
            command = [binary, 'doctor', str(config), '--confirm-catalog-stopped']
            def run(args=command):
                return subprocess.run(args, capture_output=True, text=True, timeout=5)
            empty = run()
            self.assertEqual(empty.returncode, 0, empty.stderr)
            self.assertEqual(json.loads(empty.stdout), {
                'catalog_reconciled': True, 'guard_cleared': False, 'model_calls': False})
            marker = root / 'runs' / '.catalog-discovery-in-flight'
            marker.write_text('fixture: caller has inspected this stopped discovery\n')
            with marker.open('r+') as handle:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                busy = run()
                self.assertNotEqual(busy.returncode, 0)
                self.assertTrue(marker.exists())
            cleared = run()
            self.assertEqual(cleared.returncode, 0, cleared.stderr)
            self.assertTrue(json.loads(cleared.stdout)['guard_cleared'])
            self.assertFalse(marker.exists())
            self.assertEqual(list((root / 'runs').iterdir()), [])
            typo = run([binary, 'doctor', str(config), '--confirm-catalog-stop'])
            self.assertNotEqual(typo.returncode, 0)
            self.assertIn('usage:', typo.stderr)

if __name__ == '__main__':
    unittest.main()
