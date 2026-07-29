#!/usr/bin/env python3
"""Real stdio JSON-RPC smoke test for the installed plugin executable."""

import json
import os
import subprocess
import tempfile
from pathlib import Path

root = Path(__file__).parent
with tempfile.TemporaryDirectory() as state:
    env = dict(os.environ, OCEAN_SUBAGENT_STATE_DIR=state)
    process = subprocess.Popen(
        [str(root / "ocean-subagents.py")],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    request = {"jsonrpc": "2.0", "id": 1, "method": "list_tools"}
    assert process.stdin is not None and process.stdout is not None
    process.stdin.write(json.dumps(request) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    names = [tool["name"] for tool in response["result"]["tools"]]
    assert response["id"] == 1
    assert names == [
        "spawn",
        "status",
        "wait",
        "send",
        "permissions",
        "decide",
        "cancel",
        "list",
    ], names

    unknown = {"jsonrpc": "2.0", "id": 2, "method": "nope"}
    process.stdin.write(json.dumps(unknown) + "\n")
    process.stdin.flush()
    response = json.loads(process.stdout.readline())
    assert response["error"]["code"] == -32601

    process.stdin.close()
    assert process.wait(timeout=5) == 0

print("ocean-subagents wire: PASS")
