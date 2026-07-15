#!/usr/bin/env python3
"""Open the Ocean plugin pane in the workspace that invoked the action."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from typing import Mapping


def build_command(env: Mapping[str, str]) -> list[str]:
    herdr = env.get("HERDR_BIN_PATH") or "herdr"
    plugin_id = env.get("HERDR_PLUGIN_ID") or "risingtides.ocean"

    try:
        context = json.loads(env.get("HERDR_PLUGIN_CONTEXT_JSON", "{}"))
    except json.JSONDecodeError as error:
        raise ValueError(f"invalid HERDR_PLUGIN_CONTEXT_JSON: {error}") from error
    if not isinstance(context, dict):
        raise ValueError("HERDR_PLUGIN_CONTEXT_JSON must be a JSON object")

    cwd = context.get("focused_pane_cwd") or context.get("workspace_cwd")
    workspace_id = context.get("workspace_id") or env.get("HERDR_WORKSPACE_ID")
    if not isinstance(cwd, str) or not cwd.strip():
        raise ValueError("Herdr did not provide a workspace cwd")

    command = [
        herdr,
        "plugin",
        "pane",
        "open",
        "--plugin",
        plugin_id,
        "--entrypoint",
        "ocean",
        "--placement",
        "tab",
        "--cwd",
        cwd,
        "--focus",
    ]
    if isinstance(workspace_id, str) and workspace_id.strip():
        command.extend(["--workspace", workspace_id])

    # Preserve explicit Ocean launch overrides when the action opens the
    # managed pane. Herdr-managed variables remain host-authoritative.
    for key in ("OCEAN_BIN", "OCEAN_DAEMON_URL"):
        value = env.get(key)
        if value:
            command.extend(["--env", f"{key}={value}"])
    return command


def main() -> int:
    try:
        command = build_command(os.environ)
        completed = subprocess.run(command, check=False)
    except (OSError, ValueError) as error:
        print(f"ocean Herdr plugin: {error}", file=sys.stderr)
        return 1
    return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
