"""The doctor probes CLI interfaces without invoking a model or claiming login."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class DoctorTests(unittest.TestCase):
    def doctor(self, version):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "source").mkdir()
            log = root / "calls"
            cli = root / "fake-claude"
            cli.write_text("#!/usr/bin/env python3\n"
                           "import os,sys\n"
                           "with open(os.environ['PROBE_LOG'], 'a') as f: f.write(' '.join(sys.argv[1:])+'\\n')\n"
                           f"if sys.argv[1:] == ['--version']: print({version!r})\n"
                           "elif sys.argv[1:] == ['--help']: print('--print -p --output-format --verbose --permission-prompts --model --effort --max-turns --max-budget-usd --restricted --tools --allowedTools --disallowedTools --no-session-persistence --bare --strict-mcp-config --mcp-config --permission-mode')\n"
                           "else: sys.exit(97)\n")
            cli.chmod(0o700)
            config = {"workspace_root": str(root / "workspaces"), "repositories": {"fixture": str(root / "source")},
                      "agents": {}, "native_agents": {"claude": {"provider": "claude_cli", "program": str(cli), "env": {"PROBE_LOG": str(log)}}}}
            path = root / "config.json"
            path.write_text(json.dumps(config))
            binary = os.environ.get("RELAY_APP_BINARY", str(ROOT / "target/debug/relay-app"))
            result = subprocess.run([binary, "doctor", str(path)], capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            calls = log.read_text().splitlines()
            self.assertTrue(calls)
            self.assertTrue(all(call in ["--version", "--help"] for call in calls), calls)
            self.assertFalse(report["model_calls"])
            self.assertEqual(report["authentication"], "unknown")
            self.assertEqual(report["profiles"][0]["authentication"], "unknown")
            self.assertEqual(report["profiles"][0]["model_access"], "unknown")
            return report

    def test_supported_version_does_not_claim_account_readiness(self):
        self.assertTrue(self.doctor("2.1.259 (Claude Code)")["profiles"][0]["compatible"])

    def test_old_version_fails_closed(self):
        self.assertFalse(self.doctor("2.1.258 (Claude Code)")["profiles"][0]["compatible"])


if __name__ == "__main__":
    unittest.main()
