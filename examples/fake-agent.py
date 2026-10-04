#!/usr/bin/env python3
"""Credential-free fixture; accepts requirements via RELAY_REQUIREMENTS_FILE."""
import os
from pathlib import Path

requirements = Path(os.environ["RELAY_REQUIREMENTS_FILE"]).read_text()
Path("relay-result.txt").write_text("Implemented demo requirement:\n" + requirements)
print("Created relay-result.txt in the private workspace")
