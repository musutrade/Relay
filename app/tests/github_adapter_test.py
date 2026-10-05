"""Offline exact-candidate publication tests; no model or remote GitHub calls."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import MagicMock, patch

spec = importlib.util.spec_from_file_location("adapter", pathlib.Path(__file__).resolve().parents[2] / "examples/github-draft-pr.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
BASE = "a" * 40
CANDIDATE = "b" * 40
TREE = "c" * 40


def inventory_entry(filename, expected=CANDIDATE, mode="100644", kind="blob"):
    if isinstance(filename, str):
        filename = filename.encode("utf-8")
    return f"{mode} {kind} {expected}\t".encode() + filename + b"\0"


def inventory_process(data, code=0):
    process = MagicMock()
    process.__enter__.return_value = process
    process.stdout = io.BytesIO(data)
    process.wait.return_value = code
    return process


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

    def execute(self, side_effect=None, inventory=b"", calls=None):
        calls = [] if calls is None else calls
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
        def popen(command, **kwargs):
            calls.append(command)
            self.assertEqual(command[1], "ls-tree")
            return inventory_process(inventory)
        output = io.StringIO()
        with patch.object(adapter, "capture", side_effect=capture), \
                patch.object(subprocess, "Popen", side_effect=popen), contextlib.redirect_stdout(output):
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
            with self.subTest(flag=flag, returned=returned), patch.object(adapter, "capture", side_effect=capture), \
                    patch.object(adapter, "capture_inventory", return_value=[]):
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

    def read_inventory(self, data, code=0):
        process = inventory_process(data, code)
        with patch.object(subprocess, "Popen", return_value=process):
            try:
                return adapter.capture_inventory(["/usr/bin/git", "ls-tree"], self.env)
            finally:
                self.assertEqual(process.stdout.tell(), len(data))
                process.wait.assert_called_once_with()

    def test_inventory_preserves_supported_paths_and_metadata_across_chunks(self):
        names = [" leading.txt", "trailing.txt ", " name with spaces ", "-option",
                 'quotes"and\\backslash', "路径/é😀.txt", "nonbreaking\u00a0space"]
        data = b"".join(inventory_entry(name, "d" * 64, "100755") for name in names)
        process = inventory_process(data)
        # A split inside a UTF-8 codepoint or metadata must not change its value.
        original_read = process.stdout.read
        process.stdout.read = lambda size: original_read(min(size, 1))
        with patch.object(subprocess, "Popen", return_value=process):
            parsed = adapter.capture_inventory(["/usr/bin/git", "ls-tree"], self.env)
        self.assertEqual(parsed, [("100755", "d" * 64, name) for name in names])
        self.assertEqual(self.read_inventory(b""), [])
        with self.assertRaisesRegex(RuntimeError, "status 3"):
            self.read_inventory(data, code=3)

    def test_inventory_exact_bounds_and_one_beyond(self):
        self.assertEqual(adapter.MAX_CAPTURE, 64 * 1024)
        self.assertEqual(adapter.MAX_INVENTORY_BYTES, 4 * 1024 * 1024)
        self.assertEqual(adapter.MAX_INVENTORY_ENTRIES, 50_000)
        self.assertEqual(adapter.MAX_INVENTORY_PATH_BYTES, 4096)
        path = "é" * 2048
        self.assertEqual(self.read_inventory(inventory_entry(path))[0][2], path)
        self.assertEqual(self.read_inventory(inventory_entry("x" * 4096, "d" * 64))[0][1], "d" * 64)
        for oversized in ["x" * 4097, path + "é"]:
            with self.subTest(path_bytes=len(oversized.encode())), self.assertRaises(ValueError):
                self.read_inventory(inventory_entry(oversized))
        record = inventory_entry("a")
        self.assertEqual(len(self.read_inventory(record * 50_000)), 50_000)
        with self.assertRaisesRegex(ValueError, "50000 entry"):
            self.read_inventory(record * 50_001)
        record = inventory_entry("x" * 4096)
        count, remainder = divmod(4 * 1024 * 1024, len(record))
        tail = b"y" * (remainder - len(inventory_entry(b"")))
        exact = record * count + inventory_entry(tail)
        self.assertEqual(len(exact), 4 * 1024 * 1024)
        self.assertEqual(len(self.read_inventory(exact)), count + 1)
        with self.assertRaisesRegex(ValueError, "4 MiB byte"):
            self.read_inventory(record * count + inventory_entry(tail + b"y"))

    def test_malformed_inventory_rejected_without_reading_files_or_publishing(self):
        malformed = [
            b"\0", inventory_entry("tracked.txt") + b"\0",
            inventory_entry("tracked.txt")[:-1], b"not-a-record\0",
            inventory_entry("tracked.txt", "A" * 40), inventory_entry("tracked.txt", "b" * 39),
            inventory_entry("tracked.txt", "b" * 41), inventory_entry("tracked.txt", "g" * 64),
            inventory_entry("tracked.txt", mode="120000"), inventory_entry("tracked.txt", mode="040000"),
            inventory_entry("tracked.txt", mode="160000", kind="commit"),
            inventory_entry("tracked.txt", kind="tree"),
            b"100644  blob " + CANDIDATE.encode() + b"\ttracked.txt\0",
            b"100644 blob " + CANDIDATE.encode() + b" extra\ttracked.txt\0",
        ]
        paths = [b"", b"/absolute", b"//absolute", b"a//b", b"a/", b".", b"..",
                 b"./a", b"a/./b", b"a/../b", b"a\tb", b"a\nb", b"a\rb",
                 b"a\x00b", b"a\x1fb", b"a\x7fb", "a\u0085b", "a\u009fb", "a\ufffdb",
                 b"a\xffb", b"a\xc0\x80b", b"a\xed\xa0\x80b"]
        malformed.extend(inventory_entry(path) for path in paths)
        pathlib.Path("tracked.txt").write_bytes(b"preserve this work\n")
        for data in malformed:
            calls = []
            with self.subTest(inventory=data), patch.object(adapter.os, "open") as open_file:
                with self.assertRaises((ValueError, UnicodeError)):
                    self.execute(inventory=data, calls=calls)
                open_file.assert_not_called()
            self.assertFalse(any("push" in command or "create" in command for command in calls))
            self.assertEqual(pathlib.Path("tracked.txt").read_bytes(), b"preserve this work\n")

    def test_inventory_overflows_fail_closed_and_preserve_files(self):
        pathlib.Path("tracked.txt").write_bytes(b"preserve this work\n")
        overflows = [inventory_entry("x" * 4096) * 1011,
                     inventory_entry("x") * 50_001,
                     inventory_entry("x" * 4097),
                     b"x" * (4 * 1024 * 1024 + 1)]
        for data in overflows:
            calls = []
            with self.subTest(inventory_bytes=len(data)), patch.object(adapter.os, "open") as open_file:
                with self.assertRaises(ValueError):
                    self.execute(inventory=data, calls=calls)
                open_file.assert_not_called()
            self.assertFalse(any("push" in command or "create" in command for command in calls))
            self.assertEqual(pathlib.Path("tracked.txt").read_bytes(), b"preserve this work\n")

    def test_inventory_checks_all_records_before_reading_candidate_files(self):
        content = b"keep\n"
        pathlib.Path("tracked.txt").write_bytes(content)
        expected = hashlib.sha1(b"blob 5\0" + content).hexdigest()
        valid = inventory_entry("tracked.txt", expected)
        with patch.object(adapter.os, "open") as open_file:
            with self.assertRaises(ValueError):
                self.execute(inventory=valid + inventory_entry("../escape"))
            open_file.assert_not_called()
        code, _, _ = self.execute(inventory=valid)
        self.assertEqual(code, 0)
        self.assertEqual(pathlib.Path("tracked.txt").read_bytes(), content)

    def test_git_environment_scrubs_redirects_but_keeps_existing_auth(self):
        env = adapter.git_environment({"GIT_DIR": "/wrong", "GIT_WORK_TREE": "/wrong", "GIT_CONFIG_COUNT": "99", "GH_TOKEN": "fixture"})
        self.assertEqual(env["GIT_DIR"], str(pathlib.Path.cwd() / ".git"))
        self.assertEqual(env["GIT_WORK_TREE"], str(pathlib.Path.cwd()))
        self.assertEqual(env["GIT_NO_REPLACE_OBJECTS"], "1")
        self.assertEqual(env["GIT_CONFIG_COUNT"], "3")
        self.assertEqual(env["GIT_CONFIG_VALUE_0"], "/dev/null")
        self.assertEqual(env["GH_TOKEN"], "fixture")
        self.assertEqual(env["GH_HOST"], "github.com")

    def test_actual_git_inventory_above_observed_size_passes_offline_publication_guards(self):
        def git(*args):
            return subprocess.check_output(["/usr/bin/git", "-c", "core.hooksPath=/dev/null",
                                            "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                            *args], stderr=subprocess.DEVNULL)
        git("init", "--initial-branch=main")
        content = b"tracked bytes\n"
        for index in range(5000):
            pathlib.Path(f"file-{index:05d}-{'x' * 32}.txt").write_bytes(content)
        names = [" leading", "trailing ", "both edges ", "路径-é😀", 'quote"and\\backslash', "-option"]
        for name in names:
            pathlib.Path(name).write_bytes(content)
        git("add", "--all")
        git("commit", "-m", "large inventory fixture")
        candidate = git("rev-parse", "HEAD").decode().strip()
        raw = git("ls-tree", "-r", "-z", "--full-tree", candidate)
        self.assertGreater(len(raw), 367_835)
        self.assertLess(len(raw), adapter.MAX_INVENTORY_BYTES)
        self.assertEqual(len(self.read_inventory(raw)), 5000 + len(names))
        env = {**self.env, "RELAY_BASE_SHA": candidate, "RELAY_CANDIDATE_SHA": candidate,
               "RELAY_REVIEWED_SHA": candidate, "RELAY_GITHUB_EXECUTE": "1"}
        original_capture = adapter.capture
        calls = []
        def offline_capture(command, env):
            calls.append(command)
            if command[1] == "push":
                return ""
            if command[1:3] == ["pr", "create"]:
                return "https://github.com/example/project/pull/123"
            if command[1] == "ls-remote" and "--get-url" not in command:
                return candidate + "\trefs/heads/main"
            return original_capture(command, env)
        output = io.StringIO()
        with patch.object(adapter, "capture", side_effect=offline_capture), contextlib.redirect_stdout(output):
            self.assertEqual(adapter.main(env), 0)
        self.assertFalse(json.loads(output.getvalue())["reconciliation_required"])
        self.assertEqual(sum(command[1] == "push" for command in calls), 1)
        self.assertEqual(sum(command[1:3] == ["pr", "create"] for command in calls), 1)
        for name in names:
            self.assertEqual(pathlib.Path(name).read_bytes(), content)

        # Real Git emits control characters literally with -z; reject them
        # without silently stripping the final filename or modifying work.
        control_name = pathlib.Path("trailing-newline\n")
        control_name.write_bytes(content)
        git("add", "--all")
        git("commit", "-m", "unsupported control path fixture")
        candidate = git("rev-parse", "HEAD").decode().strip()
        data = adapter.plan({**env, "RELAY_BASE_SHA": candidate, "RELAY_CANDIDATE_SHA": candidate,
                             "RELAY_REVIEWED_SHA": candidate})
        with self.assertRaisesRegex(ValueError, "unsupported path"):
            adapter.verify_candidate(data, adapter.git_environment(os.environ.copy()))
        self.assertEqual(control_name.read_bytes(), content)

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
