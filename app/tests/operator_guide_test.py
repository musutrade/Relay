"""Exercise the documented backup snippet, including committed WAL content."""
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class BackupGuideTest(unittest.TestCase):
    def test_backup_is_independent_and_contains_committed_wal(self):
        guide = (ROOT / "docs/operator-guide.md").read_text()
        snippet = guide.split("<<'PY'\n", 1)[1].split("\nPY\n", 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.db"
            backup = root / "backup.db"
            # Keep an idle connection to retain WAL; no writer runs during backup.
            connection = sqlite3.connect(source)
            try:
                connection.execute("PRAGMA journal_mode=WAL")
                connection.execute("PRAGMA wal_autocheckpoint=0")
                connection.execute("CREATE TABLE evidence (value TEXT)")
                connection.execute("INSERT INTO evidence VALUES ('committed in WAL')")
                connection.commit()
                self.assertGreater(Path(str(source) + "-wal").stat().st_size, 0)
                subprocess.run([sys.executable, "-", str(source), str(backup)],
                               input=snippet, text=True, check=True, timeout=10)
            finally:
                connection.close()
            # No source or source sidecars are available to the restored database.
            source.unlink()
            with sqlite3.connect(backup) as restored:
                self.assertEqual(restored.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
                self.assertEqual(restored.execute("SELECT value FROM evidence").fetchall(),
                                 [("committed in WAL",)])
            rejected = subprocess.run([sys.executable, "-", str(backup), str(backup)],
                                      input=snippet, text=True, capture_output=True, timeout=10)
            self.assertNotEqual(rejected.returncode, 0)


if __name__ == "__main__":
    unittest.main()
