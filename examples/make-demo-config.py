#!/usr/bin/env python3
"""Print an absolute-path, local demo config. Does not create credentials."""
import json
import sys
from pathlib import Path

root = Path(__file__).resolve().parent
workspace = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else root.parent / ".relay" / "workspaces"
print(json.dumps({
    "workspace_root": str(workspace),
    "repositories": {"demo": str(root / "demo-repository")},
    "agents": {"fake": {"program": sys.executable, "args": [str(root / "fake-agent.py")]}},
    "tests": {"demo": {"program": sys.executable, "args": [str(root / "fake-test.py")]}},
    "draft_pr_adapters": {"mock": {"program": sys.executable, "args": [str(root / "fake-draft-pr.py")]}},
    "timeout_seconds": 30,
    "output_limit_bytes": 2048
}, indent=2))
