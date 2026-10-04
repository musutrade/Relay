"""Offline exact-candidate publication tests; no model or remote GitHub calls."""
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("adapter", pathlib.Path(__file__).resolve().parents[2] / "examples/github-draft-pr.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
BASE = "a" * 40
CANDIDATE = "b" * 40
TREE = "c" * 40


class AdapterTests(unittest.TestCase):
    def setUp(self):
        # Publication fixtures must not inherit the checkout's Git metadata.
        # Hosted checkout actions may leave config.worktree or other files that
        # are correctly forbidden in a private workflow publication workspace.
        temporary = tempfile.TemporaryDirectory()
        previous = pathlib.Path.cwd()
        self.addCleanup(temporary.cleanup)
        self.addCleanup(os.chdir, previous)
        os.chdir(temporary.name)
        pathlib.Path(".git").mkdir()
        self.env = {"RELAY_GITHUB_REPOSITORY": "example/project", "RELAY_TASK_ID": "1",
                    "RELAY_GENERATION": "2", "RELAY_DRAFT_PR": "true", "RELAY_BASE_SHA": BASE,
                    "RELAY_CANDIDATE_SHA": CANDIDATE, "RELAY_REVIEWED_SHA": CANDIDATE,
                    "RELAY_REVIEW_VERDICT": "approved", "RELAY_TEST_OUTCOME": "success"}

    def execute(self, side_effect=None):
        calls = []
        def capture(command, env):
            calls.append(command)
            if side_effect:
                result = side_effect(command)
                if result is not None:
                    return result
            if "rev-parse" in command:
                return TREE if command[-1].endswith("^{tree}") else CANDIDATE
            if "write-tree" in command:
                return TREE
            if "--get-url" in command or "get-url" in command:
                return "https://github.com/example/project.git"
            if "ls-remote" in command:
                return BASE + "\trefs/heads/main"
            if command[1:3] == ["pr", "create"]:
                return "https://github.com/example/project/pull/123"
            return ""
        output = io.StringIO()
        with patch.object(adapter, "capture", side_effect=capture), contextlib.redirect_stdout(output):
            code = adapter.main({**self.env, "RELAY_GITHUB_EXECUTE": "1"})
        return code, json.loads(output.getvalue()), calls

    def test_defaults_to_dry_run_without_spawning_anything(self):
        output = io.StringIO()
        with patch.object(subprocess, "Popen") as run, contextlib.redirect_stdout(output):
            self.assertEqual(adapter.main(self.env), 0)
        run.assert_not_called()
        result = json.loads(output.getvalue())
        self.assertTrue(result["dry_run"])
        self.assertIn("--draft", result["commands"][-1])
        self.assertIn(CANDIDATE + ":refs/heads/relay/task-1-g2", result["commands"][0])
        self.assertEqual(result["candidate_sha"], CANDIDATE)

    def test_exact_candidate_and_draft_without_recommit_force_or_merge(self):
        code, result, calls = self.execute()
        self.assertEqual(code, 0)
        self.assertFalse(result["reconciliation_required"])
        push = next(command for command in calls if command[1] == "push")
        self.assertEqual(push, ["/usr/bin/git", "push", "--no-follow-tags", "https://github.com/example/project.git", CANDIDATE + ":refs/heads/relay/task-1-g2"])
        self.assertIn("--draft", calls[-1])
        for forbidden in ["--force", "commit", "reset", "init", "fetch", "merge"]:
            self.assertFalse(any(forbidden in command for command in calls), forbidden)
        body = calls[-1][calls[-1].index("--body") + 1]
        self.assertIn(BASE, body)
        self.assertIn(CANDIDATE, body)

    def test_missing_review_wrong_sha_and_missing_phase_rejected(self):
        for changes in [{"RELAY_REVIEWED_SHA": BASE}, {"RELAY_REVIEW_VERDICT": "changes_requested"},
                        {"RELAY_TEST_OUTCOME": "failure"}, {"RELAY_BASE_SHA": ""},
                        {"RELAY_GITHUB_REPOSITORY": "--bad"}, {"RELAY_TASK_ID": "0"}]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                adapter.plan({**self.env, **changes})
        with self.assertRaises(ValueError):
            adapter.main({**self.env, "RELAY_GITHUB_EXECUTE": "1", "RELAY_DRAFT_PR": "false"})

    def test_changed_head_dirty_tree_index_and_remote_base_never_publish(self):
        for flag, returned in [("rev-parse", BASE), ("write-tree", BASE),
                               ("ls-files", "new.txt"),
                               ("ls-remote", CANDIDATE + "\trefs/heads/main")]:
            calls = []
            def capture(command, env):
                calls.append(command)
                if flag in command:
                    return returned
                if "rev-parse" in command:
                    return TREE if command[-1].endswith("^{tree}") else CANDIDATE
                if "write-tree" in command:
                    return TREE
                if "--get-url" in command or "get-url" in command:
                    return "https://github.com/example/project.git"
                return ""
            with self.subTest(flag=flag, returned=returned), patch.object(adapter, "capture", side_effect=capture):
                with self.assertRaises(ValueError):
                    adapter.main({**self.env, "RELAY_GITHUB_EXECUTE": "1"})
                self.assertFalse(any("push" in command or "create" in command for command in calls))

    def test_push_or_pr_failure_requires_reconciliation_without_retry(self):
        for fail in ["push", "create"]:
            def effect(command):
                if fail in command:
                    raise RuntimeError("network result unknown")
            code, result, calls = self.execute(effect)
            self.assertEqual(code, 1)
            self.assertTrue(result["reconciliation_required"])
            self.assertEqual(sum(fail in command for command in calls), 1)
            if fail == "push":
                self.assertFalse(any("create" in command for command in calls))

    def test_unverified_pr_url_requires_reconciliation(self):
        code, result, _ = self.execute(lambda command: "https://github.com/other/project/pull/1" if "create" in command else None)
        self.assertEqual(code, 1)
        self.assertTrue(result["reconciliation_required"])

    def test_capture_bounds_output_and_checks_exit(self):
        with self.assertRaises(RuntimeError):
            adapter.capture([sys.executable, "-c", "print('x' * 70000)"], os.environ.copy())
        with self.assertRaises(RuntimeError):
            adapter.capture([sys.executable, "-c", "raise SystemExit(3)"], os.environ.copy())
        self.assertEqual(adapter.capture([sys.executable, "-c", "print('ok')"], os.environ.copy()), "ok")

    def test_git_environment_scrubs_redirects_but_keeps_existing_auth(self):
        env = adapter.git_environment({"GIT_DIR": "/wrong", "GIT_WORK_TREE": "/wrong", "GIT_CONFIG_COUNT": "99", "GH_TOKEN": "fixture"})
        self.assertEqual(env["GIT_DIR"], str(pathlib.Path.cwd() / ".git"))
        self.assertEqual(env["GIT_WORK_TREE"], str(pathlib.Path.cwd()))
        self.assertEqual(env["GIT_NO_REPLACE_OBJECTS"], "1")
        self.assertEqual(env["GIT_CONFIG_COUNT"], "3")
        self.assertEqual(env["GIT_CONFIG_VALUE_0"], "/dev/null")
        self.assertEqual(env["GH_TOKEN"], "fixture")
        self.assertEqual(env["GH_HOST"], "github.com")

    def test_real_local_git_guard_rejects_hidden_worktree_and_index_changes(self):
        previous = pathlib.Path.cwd()
        with tempfile.TemporaryDirectory() as directory:
            try:
                os.chdir(directory)
                def git(*args):
                    return subprocess.check_output(["/usr/bin/git", "-c", "core.hooksPath=/dev/null",
                                                    "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                                    *args], text=True, stderr=subprocess.DEVNULL).strip()
                git("init", "--initial-branch=main")
                pathlib.Path("tracked.txt").write_text("original\n")
                git("add", "tracked.txt")
                git("commit", "-m", "fixture")
                candidate = git("rev-parse", "HEAD")
                data = adapter.plan({**self.env, "RELAY_BASE_SHA": candidate,
                                     "RELAY_CANDIDATE_SHA": candidate, "RELAY_REVIEWED_SHA": candidate})
                env = adapter.git_environment(os.environ.copy())
                adapter.verify_candidate(data, env)
                adapter.verify_routing("/usr/bin/git", env, "https://github.com/example/project.git")
                git("update-index", "--assume-unchanged", "tracked.txt")
                pathlib.Path("tracked.txt").write_text("hidden change\n")
                with self.assertRaises(ValueError):
                    adapter.verify_candidate(data, env)
                adapter.verify_routing("/usr/bin/git", env, "https://github.com/example/project.git")
                git("update-index", "--no-assume-unchanged", "tracked.txt")
                git("add", "tracked.txt")
                pathlib.Path("tracked.txt").write_text("original\n")
                with self.assertRaises(ValueError):
                    adapter.verify_candidate(data, env)
                git("reset", "--hard", "HEAD")
                git("config", "filter.freeze.clean", "git show HEAD:tracked.txt")
                pathlib.Path(".git/info/attributes").write_text("tracked.txt filter=freeze\n")
                pathlib.Path("tracked.txt").write_text("filter-hidden bytes\n")
                with self.assertRaises(ValueError):
                    adapter.verify_candidate(data, env)
                for setting in ["insteadOf", "pushInsteadOf"]:
                    key = "url.https://other.invalid/." + setting
                    git("config", key, "https://github.com/")
                    with self.assertRaises(ValueError):
                        adapter.verify_routing("/usr/bin/git", env, "https://github.com/example/project.git")
                    git("config", "--unset", key)
                for setting in ["url", "pushurl"]:
                    key = "remote.https://github.com/example/project.git." + setting
                    git("config", key, "https://other.invalid/repo.git")
                    with self.assertRaises(ValueError):
                        adapter.verify_routing("/usr/bin/git", env, "https://github.com/example/project.git")
                    git("config", "--unset", key)
                for name in ["commondir", "objects/info/alternates"]:
                    path = pathlib.Path(".git") / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("/must-not-use\n")
                    with self.assertRaises(ValueError):
                        adapter.verify_private_git()
                    path.unlink()
            finally:
                os.chdir(previous)


if __name__ == "__main__":
    unittest.main()
