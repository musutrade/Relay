#!/usr/bin/env python3
"""Publish only a host-pinned, tested and reviewed Git candidate as a draft.

The default is a no-subprocess dry run. Actual publication requires explicit
trusted configuration. Authentication is owned by Git/gh; this adapter neither
installs nor saves credentials. An ambiguous write is never retried.
"""
import hashlib
import json
import os
import re
import stat
import subprocess
from pathlib import Path

MAX_CAPTURE = 64 * 1024
MAX_INVENTORY_BYTES = 4 * 1024 * 1024
MAX_INVENTORY_ENTRIES = 50_000
MAX_INVENTORY_PATH_BYTES = 4096
# The longest supported metadata prefix is "100755 blob <64-byte SHA>\t".
MAX_INVENTORY_ENTRY_BYTES = 77 + MAX_INVENTORY_PATH_BYTES


def sha(value):
    if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", value):
        raise ValueError("a full lowercase Git SHA is required")
    return value


def plan(env):
    repository = env.get("RELAY_GITHUB_REPOSITORY", "")
    base = env.get("RELAY_GITHUB_BASE", "main")
    task = env.get("RELAY_TASK_ID", "")
    generation = env.get("RELAY_GENERATION", "")
    if (len(repository) > 201 or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository)
            or any(part in (".", "..") for part in repository.split("/"))):
        raise ValueError("RELAY_GITHUB_REPOSITORY must be an explicit owner/repository")
    if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_./-]*", base)
            or ".." in base or "//" in base
            or any(part.startswith(".") or part.endswith((".", ".lock")) for part in base.split("/"))
            or base.endswith("/")):
        raise ValueError("invalid base branch")
    if not re.fullmatch(r"[1-9][0-9]*", task) or not re.fullmatch(r"[1-9][0-9]*", generation):
        raise ValueError("positive task and generation are required")
    base_sha = sha(env.get("RELAY_BASE_SHA", ""))
    candidate = sha(env.get("RELAY_CANDIDATE_SHA", ""))
    if candidate != sha(env.get("RELAY_REVIEWED_SHA", "")):
        raise ValueError("candidate must equal the successfully reviewed SHA")
    if env.get("RELAY_REVIEW_VERDICT") != "approved" or env.get("RELAY_TEST_OUTCOME") != "success":
        raise ValueError("successful tests and explicit review approval are required")
    branch = f"relay/task-{task}-g{generation}"
    git = env.get("RELAY_GIT_PROGRAM", "/usr/bin/git")
    gh = env.get("RELAY_GH_PROGRAM", "/usr/bin/gh")
    if not Path(git).is_absolute() or not Path(gh).is_absolute():
        raise ValueError("git and gh programs must be absolute configured paths")
    title = f"Relay task {task}"
    body = (f"Development result for Relay task {task}, generation {generation}.\n\n"
            f"Base SHA: {base_sha}\nCandidate SHA: {candidate}\n"
            f"Test result: success\nReview: approved for {candidate}\n\n"
            "Created as a draft for human review. No merge or deployment is performed.")
    url = f"https://github.com/{repository}.git"
    commands = [
        [git, "push", "--no-follow-tags", url, f"{candidate}:refs/heads/{branch}"],
        [gh, "pr", "create", "--repo", repository, "--base", base, "--head", branch,
         "--draft", "--title", title, "--body", body],
    ]
    return {"repository": repository, "base": base, "branch": branch,
            "base_sha": base_sha, "candidate_sha": candidate, "commands": commands}


def capture(command, env):
    """Drain untrusted CLI output while retaining at most 64 KiB, including errors.

    The trusted Relay supervisor owns the shared deadline and process tree.
    stderr goes straight to its bounded capture, rather than a second pipe.
    """
    with subprocess.Popen(command, stdout=subprocess.PIPE, env=env) as process:
        data = bytearray()
        oversized = False
        while True:
            chunk = process.stdout.read(4096)
            if not chunk:
                break
            remaining = MAX_CAPTURE - len(data)
            data.extend(chunk[:remaining])
            oversized |= len(chunk) > remaining
        code = process.wait()
    if code != 0:
        raise RuntimeError(f"configured command exited with status {code}")
    if oversized:
        raise RuntimeError("configured command output exceeded the bounded protocol limit")
    return data.decode("utf-8", errors="strict").strip()


def parse_inventory_entry(entry):
    metadata, separator, path_bytes = entry.partition(b"\t")
    if not separator:
        raise ValueError("malformed Git tree inventory entry")
    if len(path_bytes) > MAX_INVENTORY_PATH_BYTES:
        raise ValueError("Git tree inventory path exceeds 4096 UTF-8 bytes")
    fields = metadata.decode("utf-8", errors="strict").split(" ")
    if len(fields) != 3 or fields[0] not in ("100644", "100755") or fields[1] != "blob":
        raise ValueError("candidate tree requires regular Git blobs")
    expected = sha(fields[2])
    filename = path_bytes.decode("utf-8", errors="strict")
    if ("\ufffd" in filename or any(part in ("", ".", "..") for part in filename.split("/"))
            or any(ord(character) < 32 or 127 <= ord(character) <= 159 for character in filename)):
        raise ValueError("candidate tree contains an unsupported path")
    return fields[0], expected, filename


def capture_inventory(command, env):
    """Read an exact, bounded NUL-framed tree inventory without trimming paths.

    Validate records incrementally and retain at most the inventory budgets.
    After a protocol failure, discard all entries but keep draining the child;
    the trusted Relay supervisor still owns deadlines and process lifecycle.
    """
    entries = []
    pending = bytearray()
    size = 0
    failure = None
    with subprocess.Popen(command, stdout=subprocess.PIPE, env=env) as process:
        while True:
            chunk = process.stdout.read(4096)
            if not chunk:
                break
            if failure is not None:
                continue
            try:
                size += len(chunk)
                if size > MAX_INVENTORY_BYTES:
                    raise ValueError("Git tree inventory exceeds the 4 MiB byte limit")
                pending.extend(chunk)
                while (end := pending.find(b"\0")) >= 0:
                    if len(entries) >= MAX_INVENTORY_ENTRIES:
                        raise ValueError("Git tree inventory exceeds the 50000 entry limit")
                    if end > MAX_INVENTORY_ENTRY_BYTES:
                        raise ValueError("Git tree inventory entry exceeds its bounded size")
                    entries.append(parse_inventory_entry(bytes(pending[:end])))
                    del pending[:end + 1]
                if len(pending) > MAX_INVENTORY_ENTRY_BYTES:
                    raise ValueError("Git tree inventory entry exceeds its bounded size")
            except (ValueError, UnicodeError) as error:
                failure = error
                entries.clear()
                pending.clear()
        code = process.wait()
    if code != 0:
        raise RuntimeError(f"configured command exited with status {code}")
    if failure is not None:
        raise failure
    if pending:
        raise ValueError("Git tree inventory is truncated or missing its final NUL")
    return entries


def git_environment(env):
    # Do not let inherited Git control variables redirect validation or push.
    clean = {key: value for key, value in env.items() if not key.startswith("GIT_")}
    clean["GH_HOST"] = "github.com"
    clean.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
                  "GIT_NO_REPLACE_OBJECTS": "1", "GIT_DIR": str(Path.cwd() / ".git"),
                  "GIT_WORK_TREE": str(Path.cwd()), "GIT_CONFIG_COUNT": "3",
                  "GIT_CONFIG_KEY_0": "core.hooksPath", "GIT_CONFIG_VALUE_0": "/dev/null",
                  "GIT_CONFIG_KEY_1": "core.fsmonitor", "GIT_CONFIG_VALUE_1": "false",
                  "GIT_CONFIG_KEY_2": "http.followRedirects", "GIT_CONFIG_VALUE_2": "false"})
    return clean


def verify_private_git():
    metadata = Path(".git")
    if not metadata.is_dir() or metadata.is_symlink():
        raise ValueError("publication requires a private Git-backed workflow workspace")
    for name in ["commondir", "gitdir", "config.worktree", "objects/info/alternates", "objects/info/http-alternates"]:
        path = metadata / name
        if path.exists() or path.is_symlink():
            raise ValueError("Git metadata redirection is unsupported for private publication")
    for name in ["remotes", "branches"]:
        path = metadata / name
        if path.is_symlink() or (path.exists() and (not path.is_dir() or next(path.iterdir(), None) is not None)):
            raise ValueError("legacy Git remote files are unsupported for private publication")


def verify_candidate(plan_data, env):
    verify_private_git()
    git = plan_data["commands"][0][0]
    run = lambda *args: capture([git, *args], env)
    candidate = plan_data["candidate_sha"]
    if run("rev-parse", "--verify", "HEAD") != candidate:
        raise ValueError("HEAD no longer matches the reviewed candidate")
    tree = run("rev-parse", "--verify", candidate + "^{tree}")
    if run("write-tree") != tree:
        raise ValueError("candidate index changed")
    entries = capture_inventory([git, "ls-tree", "-r", "-z", "--full-tree", candidate], env)
    for mode, expected, filename in entries:
        path = Path(filename)
        # Never read through symlinked parents or a changed special file.
        for parent in [path, *path.parents]:
            if parent.is_symlink():
                raise ValueError("candidate contains a symlink")
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, "rb") as source:
            info = os.fstat(source.fileno())
            if not stat.S_ISREG(info.st_mode) or bool(info.st_mode & stat.S_IXUSR) != (mode == "100755"):
                raise ValueError("candidate file type or executable mode changed")
            digest = hashlib.sha1() if len(expected) == 40 else hashlib.sha256()
            digest.update(f"blob {info.st_size}\0".encode())
            while chunk := source.read(64 * 1024):
                digest.update(chunk)
            if digest.hexdigest() != expected:
                raise ValueError("candidate file bytes differ from the reviewed commit")
    if run("ls-files", "--others", "--exclude-standard", "--"):
        raise ValueError("candidate has untracked files")


def verify_routing(git, env, repository_url):
    # Keep existing credential helpers, but reject both fetch and push URL
    # rewrites. Query names only: configuration may contain credential values.
    keys = capture([git, "config", "--null", "--name-only", "--list"], env)
    for key in keys.split("\0"):
        key = key.lower()
        if (key.startswith("url.") and key.endswith((".insteadof", ".pushinsteadof"))) or key.startswith("remote."):
            raise ValueError("Git URL rewrites and named remotes are unsupported for exact-target publication")
    if capture([git, "ls-remote", "--get-url", repository_url], env) != repository_url:
        raise ValueError("effective Git fetch target differs from the configured repository")
    # With URL rewrites, named remotes, and legacy remote files rejected, the
    # explicit HTTPS URL has no separate push mapping to consult.


def verify(plan_data, env):
    git = plan_data["commands"][0][0]
    run = lambda *args: capture([git, *args], env)
    verify_routing(git, env, f"https://github.com/{plan_data['repository']}.git")
    verify_candidate(plan_data, env)
    run("merge-base", "--is-ancestor", plan_data["base_sha"], plan_data["candidate_sha"])
    remote = run("ls-remote", "--exit-code", f"https://github.com/{plan_data['repository']}.git",
                 f"refs/heads/{plan_data['base']}")
    expected = f"{plan_data['base_sha']}\trefs/heads/{plan_data['base']}"
    if remote != expected:
        raise ValueError("target base does not match the pinned source commit")
    verify_candidate(plan_data, env)


def main(env=None):
    env = dict(os.environ if env is None else env)
    data = plan(env)
    if env.get("RELAY_GITHUB_EXECUTE") != "1":
        print(json.dumps({"dry_run": True, "draft": True, "reconciliation_required": False, **data}))
        return 0
    if env.get("RELAY_DRAFT_PR") != "true":
        raise ValueError("publication must be invoked in Relay's draft PR phase")
    verify_private_git()
    env = git_environment(env)
    verify(data, env)
    # Once a write is attempted, any failure is ambiguous. Never retry it here.
    try:
        capture(data["commands"][0], env)
        output = capture(data["commands"][1], env)
        expected = rf"https://github\.com/{re.escape(data['repository'])}/pull/[1-9][0-9]*"
        if len(output) > 512 or not re.fullmatch(expected, output):
            raise RuntimeError("GitHub did not return a verified draft PR URL")
    except (OSError, RuntimeError, UnicodeError) as error:
        print(json.dumps({"dry_run": False, "draft": True, "repository": data["repository"],
                          "branch": data["branch"], "candidate_sha": data["candidate_sha"],
                          "reconciliation_required": True, "error": str(error)[:512]}))
        return 1
    print(json.dumps({"dry_run": False, "draft": True, "repository": data["repository"],
                      "branch": data["branch"], "candidate_sha": data["candidate_sha"],
                      "url": output, "reconciliation_required": False}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, RuntimeError, UnicodeError) as error:
        print(json.dumps({"error": str(error)[:512], "reconciliation_required": False}))
        raise SystemExit(1)
