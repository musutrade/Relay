"""The doctor probes CLI interfaces without invoking a model or claiming login."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
HELP = ('--print -p --output-format --verbose --permission-prompts --model --effort '
        '--max-budget-usd --restricted --tools --allowedTools --disallowedTools '
        '--disable-slash-commands --no-session-persistence --bare --strict-mcp-config '
        '--mcp-config --permission-mode')
MISSING = "error: option '--max-turns <turns>' argument missing"


class DoctorTests(unittest.TestCase):
    def doctor(self, version="2.1.281 (Claude Code)", mode="advertised", help_text=HELP, limits=True):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "source").mkdir()
            log = root / "calls"
            cli = root / "fake-claude"
            cli.write_text(f'''#!/usr/bin/env python3
import json, os, signal, sys, time
args = sys.argv[1:]
with open(os.environ['PROBE_LOG'], 'a') as f: f.write(json.dumps(args)+'\\n')
mode = {mode!r}
help_text = {help_text!r}
if args == ['--version']:
    print({version!r})
elif args == ['--help']:
    print(help_text + (' --max-turns' if mode == 'advertised' else ''))
elif args == ['--help', '--max-turns']:
    if mode == 'help_first':
        print(help_text)
    elif mode == 'unknown':
        print("error: unknown option '--max-turns'", file=sys.stderr)
        sys.exit(1)
    elif mode == 'generic_failure':
        print('CLI initialization failed', file=sys.stderr)
        sys.exit(1)
    elif mode == 'malformed_diagnostic':
        print("error: option '--max-turns-fake <turns>' argument missing", file=sys.stderr)
        sys.exit(1)
    else:
        if mode == 'timeout': time.sleep(20)
        if mode == 'signaled': os.kill(os.getpid(), signal.SIGTERM)
        if mode == 'workspace_failure':
            with open('oversized', 'w') as f: f.write('x' * 2048)
        print({MISSING!r}, file=sys.stderr)
        if mode == 'oversized_missing': print('x' * 70000)
        sys.exit(2 if mode == 'wrong_exit' else 1)
elif args == ['--max-turns', '8', '--help']:
    if mode == 'reject_value':
        print('configured value is unsupported', file=sys.stderr)
        sys.exit(1)
    if mode == 'oversized_valid': print('x' * 70000)
    print(help_text)
else:
    # Includes every auth or model invocation: none is allowed by this fixture.
    sys.exit(97)
''')
            cli.chmod(0o700)
            config = {
                "workspace_root": str(root / "workspaces"),
                "max_snapshot_bytes": 1024,
                "repositories": {"fixture": str(root / "source")},
                "agents": {},
                "native_agents": {"claude": {
                    "provider": "claude_cli", "program": str(cli),
                    "max_turns": 8, "max_budget_usd": 2,
                    "env": {"PROBE_LOG": str(log)}}}}
            if not limits:
                del config["native_agents"]["claude"]["max_turns"]
            path = root / "config.json"
            path.write_text(json.dumps(config))
            binary = os.environ.get("RELAY_APP_BINARY", str(ROOT / "target/debug/relay-app"))
            result = subprocess.run([binary, "doctor", str(path)], capture_output=True, text=True, timeout=15)
            # Doctor's exit status reports command completion, not compatibility.
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(calls[:2], [["--version"], ["--help"]])
            allowed = [["--version"], ["--help"], ["--help", "--max-turns"],
                       ["--max-turns", "8", "--help"]]
            self.assertTrue(all(call in allowed for call in calls), calls)
            self.assertFalse(report["model_calls"])
            self.assertEqual(report["authentication"], "unknown")
            self.assertEqual(report["profiles"][0]["authentication"], "unknown")
            self.assertEqual(report["profiles"][0]["model_access"], "unknown")
            return report["profiles"][0], calls

    def test_advertised_flag_needs_no_fallback_or_account_readiness_claim(self):
        profile, calls = self.doctor("2.1.259 (Claude Code)")
        self.assertTrue(profile["compatible"])
        self.assertEqual(len(calls), 2)

    def test_unconfigured_hidden_flag_needs_no_fallback(self):
        profile, calls = self.doctor(mode="hidden", limits=False)
        self.assertTrue(profile["compatible"])
        self.assertEqual(len(calls), 2)

    def test_fallback_keeps_shared_probe_deadline(self):
        profile, calls = self.doctor(mode="timeout")
        self.assertFalse(profile["compatible"])
        self.assertIn("TimedOut", profile["error"])
        self.assertEqual(len(calls), 3)

    def test_hidden_known_flag_is_verified_for_developer_and_reviewer(self):
        profile, calls = self.doctor(mode="hidden")
        self.assertTrue(profile["compatible"], profile)
        self.assertTrue(profile["probe"]["read_only_supported"])
        self.assertEqual(profile["probe"]["cli_version"], "2.1.281")
        self.assertEqual(calls[2:], [["--help", "--max-turns"], ["--max-turns", "8", "--help"]])

    def test_old_unknown_or_prerelease_version_fails_before_fallback(self):
        for version in ["2.1.258 (Claude Code)", "unrecognized", "2.1.281-beta"]:
            with self.subTest(version=version):
                profile, calls = self.doctor(version, mode="hidden")
                self.assertFalse(profile["compatible"])
                self.assertEqual(len(calls), 2)

    def test_positive_help_alone_unknown_and_malformed_probes_fail_closed(self):
        for mode in ["help_first", "unknown", "generic_failure", "malformed_diagnostic",
                     "wrong_exit", "reject_value", "oversized_missing", "oversized_valid",
                     "signaled", "workspace_failure"]:
            with self.subTest(mode=mode):
                profile, _ = self.doctor(mode=mode)
                self.assertFalse(profile["compatible"], profile)

    def test_other_required_flags_cannot_use_hidden_flag_exception(self):
        profile, calls = self.doctor(mode="hidden", help_text=HELP.replace('--max-budget-usd', ''))
        self.assertFalse(profile["compatible"])
        self.assertIn('--max-budget-usd', profile['error'])
        self.assertEqual(len(calls), 2)

    def test_hidden_turns_do_not_establish_missing_reviewer_restriction(self):
        profile, _ = self.doctor(mode="hidden", help_text=HELP.replace('--restricted', ''))
        self.assertTrue(profile["compatible"])
        self.assertFalse(profile["probe"]["read_only_supported"])


if __name__ == "__main__":
    unittest.main()
