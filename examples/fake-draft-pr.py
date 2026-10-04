#!/usr/bin/env python3
"""Mock adapter. Deliberately performs no network, git push, or gh command."""
import json
from pathlib import Path

assert Path("relay-result.txt").exists()
print(json.dumps({"dry_run": True, "draft": True, "message": "Mock PR prepared; nothing published"}))
