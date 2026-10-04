"""Offline tests for the optional trusted GitHub adapter."""
import contextlib
import importlib.util
import io
import json
import pathlib
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("adapter", pathlib.Path(__file__).resolve().parents[2] / "examples/github-draft-pr.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)


class AdapterTests(unittest.TestCase):
    def setUp(self):
        self.env = {"RELAY_GITHUB_REPOSITORY": "example/project", "RELAY_TASK_ID": "1", "RELAY_GENERATION": "2", "RELAY_DRAFT_PR": "true"}

    def test_defaults_to_dry_run_without_spawning_anything(self):
        output = io.StringIO()
        with patch.object(subprocess, "run") as run, contextlib.redirect_stdout(output):
            adapter.main(self.env)
        run.assert_not_called()
        result = json.loads(output.getvalue())
        self.assertTrue(result["dry_run"])
        self.assertIn("--draft", result["commands"][-1])
        self.assertIn("HEAD:refs/heads/relay/task-1-g2", result["commands"][-2])

    def test_explicit_publication_uses_draft_and_never_force(self):
        with patch.object(pathlib.Path, "exists", return_value=False), patch.object(subprocess, "run") as run, contextlib.redirect_stdout(io.StringIO()):
            adapter.main({**self.env, "RELAY_GITHUB_EXECUTE": "1"})
        self.assertEqual(run.call_count, 8)
        commands = [call.args[0] for call in run.call_args_list]
        self.assertIn("--draft", commands[-1])
        self.assertFalse(any("--force" in command for command in commands))

    def test_failure_does_not_continue_to_publication(self):
        with patch.object(pathlib.Path, "exists", return_value=False), patch.object(subprocess, "run", side_effect=subprocess.CalledProcessError(1, "git")) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                adapter.main({**self.env, "RELAY_GITHUB_EXECUTE": "1"})
        self.assertEqual(run.call_count, 1)

    def test_invalid_repository_and_missing_phase_are_rejected(self):
        with self.assertRaises(ValueError):
            adapter.plan({**self.env, "RELAY_GITHUB_REPOSITORY": "--bad"})
        with self.assertRaises(ValueError):
            adapter.main({**self.env, "RELAY_GITHUB_EXECUTE": "1", "RELAY_DRAFT_PR": "false"})


if __name__ == "__main__":
    unittest.main()
