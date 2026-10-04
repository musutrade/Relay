#!/usr/bin/env python3
"""Optional trusted Git/gh adapter, run only after successful agent and tests.

Default mode is dry-run. Set RELAY_GITHUB_EXECUTE=1 in trusted host configuration
only after authorizing publication to the configured repository. No credentials
are created, stored, or printed; git and gh must already be authenticated.
"""
import json
import os
import re
import subprocess
from pathlib import Path


def plan(env):
    repository = env.get("RELAY_GITHUB_REPOSITORY", "")
    base = env.get("RELAY_GITHUB_BASE", "main")
    task = env.get("RELAY_TASK_ID", "")
    generation = env.get("RELAY_GENERATION", "")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("RELAY_GITHUB_REPOSITORY must be an explicit owner/repository")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_./-]*", base) or ".." in base or "//" in base or base.endswith("/"):
        raise ValueError("invalid base branch")
    if not task.isdecimal() or not generation.isdecimal():
        raise ValueError("positive task and generation are required")
    branch = f"relay/task-{task}-g{generation}"
    git = env.get("RELAY_GIT_PROGRAM", "/usr/bin/git")
    gh = env.get("RELAY_GH_PROGRAM", "/usr/bin/gh")
    if not Path(git).is_absolute() or not Path(gh).is_absolute():
        raise ValueError("git and gh programs must be absolute configured paths")
    title = f"Relay task {task}"
    body = f"Development result for Relay task {task}, generation {generation}.\n\nCreated as a draft for human review."
    commands = [
        [git, "init", "--initial-branch", branch],
        [git, "remote", "add", "origin", f"https://github.com/{repository}.git"],
        [git, "fetch", "--depth", "1", "origin", base],
        [git, "reset", "--mixed", "FETCH_HEAD"],
        [git, "add", "--all"],
        [git, "commit", "-m", title],
        [git, "push", "--set-upstream", "origin", f"HEAD:refs/heads/{branch}"],
        [gh, "pr", "create", "--repo", repository, "--base", base, "--head", branch, "--draft", "--title", title, "--body", body],
    ]
    return repository, branch, commands


def main(env=None):
    env = dict(os.environ if env is None else env)
    repository, branch, commands = plan(env)
    execute = env.get("RELAY_GITHUB_EXECUTE") == "1"
    if not execute:
        print(json.dumps({"dry_run": True, "draft": True, "repository": repository, "branch": branch, "commands": commands}))
        return
    if env.get("RELAY_DRAFT_PR") != "true":
        raise ValueError("publication must be invoked in Relay's draft PR phase")
    if Path(".git").exists():
        raise ValueError("workspace already has Git metadata; inspect external effects before retrying")
    # CLI stdout/stderr pass straight to the host's bounded draining capture.
    # Stop on every failed step; no fallback publication or force-push.
    for command in commands:
        subprocess.run(command, check=True, env=env)
    print(json.dumps({"dry_run": False, "draft": True, "repository": repository, "branch": branch}))


if __name__ == "__main__":
    main()
