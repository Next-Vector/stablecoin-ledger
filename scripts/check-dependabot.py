#!/usr/bin/env python3
"""Validate .github/dependabot.yml against the official Dependabot JSON schema (SchemaStore).

Usage: ./scripts/check-dependabot.py   (needs: pip install jsonschema pyyaml)
"""
import json
import pathlib
import sys
import urllib.request

import jsonschema
import yaml

SCHEMA_URL = "https://json.schemastore.org/dependabot-2.0.json"
CONFIG = pathlib.Path(__file__).resolve().parent.parent / ".github" / "dependabot.yml"


def main() -> int:
    if not CONFIG.is_file():
        print(f"FAIL: {CONFIG} does not exist")
        return 1
    with urllib.request.urlopen(SCHEMA_URL, timeout=30) as response:
        schema = json.load(response)
    config = yaml.safe_load(CONFIG.read_text())
    errors = sorted(jsonschema.Draft7Validator(schema).iter_errors(config), key=lambda e: list(e.path))
    for error in errors:
        print(f"FAIL: {'/'.join(map(str, error.path)) or '<root>'}: {error.message}")
    if errors:
        return 1
    print(f"OK: {CONFIG.name} is valid ({len(config['updates'])} update entries)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
