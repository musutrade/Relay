#!/usr/bin/env python3
from pathlib import Path

assert Path("relay-result.txt").read_text().startswith("Implemented demo requirement:")
print("Demo acceptance test passed")
