import importlib.util
import json
import tempfile
import threading
import tomllib
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).parent
SPEC = importlib.util.spec_from_file_location("ocean_subagents", ROOT / "ocean-subagents.py")
module = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(module)


class FakeDaemon:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = {}
        self.sessions = {}
        self.permissions = {}
        self.decisions = []
        self.payloads = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def read_json(self):
                length = int(self.headers.get("Content-Length", "0"))
                return json.loads(self.rfile.read(length) or b"{}")

            def reply(self, status, body):
                encoded = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_POST(self):
                if self.path == "/v1/agent/turns":
                    payload = self.read_json()
                    with owner.lock:
                        turn_id = str(uuid.uuid4())
                        session_id = payload.get("session_id") or str(uuid.uuid4())
                        owner.payloads.append(payload)
                        owner.requests[turn_id] = {
                            "request_id": turn_id,
                            "session_id": session_id,
                            "state": "running",
                            "message": "agent turn running",
                        }
                        owner.sessions.setdefault(session_id, [])
                    self.reply(
                        202,
                        {
                            "ok": True,
                            "turn_id": turn_id,
                            "session_id": session_id,
                            "status": "running",
                            "event_id_prefix": turn_id[:8],
                        },
                    )
                    return
                if self.path.startswith("/v1/requests/") and self.path.endswith("/cancel"):
                    request_id = self.path.split("/")[3]
                    with owner.lock:
                        owner.requests[request_id]["state"] = "cancelled"
                        owner.requests[request_id]["message"] = "cancelled"
                    self.reply(200, {"ok": True})
                    return
                if self.path.startswith("/v1/permissions/") and self.path.endswith("/decision"):
                    permission_id = self.path.split("/")[3]
                    payload = self.read_json()
                    with owner.lock:
                        permission = owner.permissions.get(permission_id)
                        if permission is None:
                            self.reply(404, {"ok": False})
                            return
                        request = owner.requests[permission["request_id"]]
                        expected = next(
                            item["decision_token"]
                            for item in owner.payloads
                            if item.get("session_id", request["session_id"])
                            == request["session_id"]
                        )
                        if payload.get("decision_token") != expected:
                            self.reply(403, {"ok": False})
                            return
                        owner.decisions.append(payload)
                        del owner.permissions[permission_id]
                    self.reply(200, {"ok": True, "message": "permission resolved"})
                    return
                self.reply(404, {"ok": False})

            def do_GET(self):
                if self.path == "/v1/requests":
                    with owner.lock:
                        requests = list(owner.requests.values())
                    self.reply(200, {"ok": True, "requests": requests})
                    return
                if self.path == "/v1/permissions":
                    with owner.lock:
                        permissions = list(owner.permissions.values())
                    self.reply(200, {"ok": True, "permissions": permissions})
                    return
                if self.path.startswith("/v1/sessions/"):
                    session_id = self.path.rsplit("/", 1)[1]
                    with owner.lock:
                        transcript = list(owner.sessions.get(session_id, []))
                    self.reply(
                        200,
                        {"ok": True, "session": {"id": session_id, "transcript": transcript}},
                    )
                    return
                self.reply(404, {"ok": False})

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self):
        host, port = self.server.server_address
        return f"http://{host}:{port}"

    def add_permission(self, request_id, tool="bash", args=None):
        permission_id = str(uuid.uuid4())
        with self.lock:
            request = self.requests[request_id]
            self.permissions[permission_id] = {
                "permission_id": permission_id,
                "request_id": request_id,
                "session_id": request["session_id"],
                "tool": tool,
                "reason": "test permission",
                "args": args or {"command": "true"},
                "created_at": "2026-07-29T12:00:00Z",
            }
            request["state"] = "waiting_for_permission"
        return permission_id

    def complete(self, request_id, output="worker result"):
        with self.lock:
            request = self.requests[request_id]
            request.update(
                state="completed",
                message="prompt completed",
                finished_at="2026-07-29T12:00:00Z",
            )
            self.sessions[request["session_id"]].append(
                {"role": "assistant", "text": output}
            )

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class OceanSubagentTests(unittest.TestCase):
    def manager(self, root, daemon):
        return module.Subagents(
            module.DaemonClient(daemon.url), module.JsonStore(Path(root) / "state")
        )

    def test_manifest_and_live_tools_match(self):
        manifest = tomllib.loads((ROOT / "plugin.toml").read_text())
        declared = {tool["name"]: tool["input_schema"] for tool in manifest["tool"]}
        live = {tool["name"]: tool["inputSchema"] for tool in module.TOOLS}
        self.assertEqual(declared, live)

    def test_spawn_is_real_child_turn_and_completion_returns_output(self):
        with tempfile.TemporaryDirectory() as root, FakeDaemon() as daemon:
            manager = self.manager(root, daemon)
            run = manager.spawn(
                {
                    "task": "Inspect the repository",
                    "role": "reviewer",
                    "cwd": root,
                    "model": "deepseek-v4-pro",
                }
            )
            self.assertEqual(run["status"], "running")
            payload = daemon.payloads[-1]
            self.assertEqual(payload["agent"], module.WORKER_AGENT)
            self.assertEqual(payload["client_type"], "ocean-subagent")
            self.assertEqual(payload["model_id"], "deepseek-v4-pro")
            self.assertGreaterEqual(len(payload["decision_token"]), 48)
            self.assertIn("Do not delegate", payload["prompt"])

            daemon.complete(run["turn_id"], "review complete")
            finished = manager.refresh(run["run_id"])
            self.assertEqual(finished["status"], "completed")
            self.assertEqual(finished["output"], "review complete")
            durable = json.loads((Path(root) / "state/runs.json").read_text())
            self.assertEqual(durable["runs"][run["run_id"]]["session_id"], run["session_id"])

    def test_follow_up_reuses_session_and_cancel_uses_turn_id(self):
        with tempfile.TemporaryDirectory() as root, FakeDaemon() as daemon:
            manager = self.manager(root, daemon)
            run = manager.spawn({"task": "first", "cwd": root})
            daemon.complete(run["turn_id"])
            manager.refresh(run["run_id"])

            followed = manager.send({"run_id": run["run_id"], "message": "second"})
            self.assertEqual(followed["session_id"], run["session_id"])
            self.assertEqual(daemon.payloads[-1]["session_id"], run["session_id"])
            cancelling = manager.cancel({"run_id": run["run_id"]})
            self.assertEqual(cancelling["status"], "cancelling")
            cancelled = manager.refresh(run["run_id"])
            self.assertEqual(cancelled["status"], "cancelled")

    def test_child_permissions_are_scoped_and_token_bound(self):
        with tempfile.TemporaryDirectory() as root, FakeDaemon() as daemon:
            manager = self.manager(root, daemon)
            run = manager.spawn({"task": "permission test", "cwd": root})
            permission_id = daemon.add_permission(run["turn_id"], "bash")
            pending = manager.permissions({"run_id": run["run_id"]})
            self.assertEqual(pending["status"], "waiting_for_permission")
            self.assertEqual(pending["permissions"][0]["permission_id"], permission_id)
            with self.assertRaisesRegex(module.PluginError, "expected_tool"):
                manager.decide(
                    {
                        "run_id": run["run_id"],
                        "permission_id": permission_id,
                        "expected_tool": "write",
                        "decision": "allow",
                    }
                )
            resolved = manager.decide(
                {
                    "run_id": run["run_id"],
                    "permission_id": permission_id,
                    "expected_tool": "bash",
                    "decision": "allow_session",
                }
            )
            self.assertTrue(resolved["ok"])
            self.assertEqual(daemon.decisions[0]["decision"], "allow_session")
            stored = json.loads((Path(root) / "state/runs.json").read_text())
            self.assertIn("decision_token", stored["runs"][run["run_id"]])

    def test_wait_refreshes_terminal_state(self):
        with tempfile.TemporaryDirectory() as root, FakeDaemon() as daemon:
            manager = self.manager(root, daemon)
            run = manager.spawn({"task": "wait test", "cwd": root})
            daemon.complete(run["turn_id"], "done")
            result = manager.wait({"run_id": run["run_id"], "timeout_seconds": 1})
            self.assertEqual(result["status"], "completed")
            self.assertEqual(result["output"], "done")

    def test_concurrency_is_bounded(self):
        with tempfile.TemporaryDirectory() as root, FakeDaemon() as daemon:
            manager = self.manager(root, daemon)
            for number in range(module.MAX_ACTIVE):
                manager.spawn({"task": f"task {number}", "cwd": root})
            with self.assertRaisesRegex(module.PluginError, "concurrency limit"):
                manager.spawn({"task": "one too many", "cwd": root})


if __name__ == "__main__":
    unittest.main()
