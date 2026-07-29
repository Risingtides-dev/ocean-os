import importlib.util
import json
import tempfile
import threading
import time
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("crew.py")
SPEC = importlib.util.spec_from_file_location("ocean_crew_demo", MODULE_PATH)
crew = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(crew)


class FakeOcean:
    def __init__(self, fail_first_for=None):
        self.fail_first_for = fail_first_for
        self.attempts = {}
        self.prompts = {}
        self.active = 0
        self.max_active = 0
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                if self.path != "/v1/prompt":
                    self.send_error(404)
                    return
                length = int(self.headers.get("Content-Length", "0"))
                payload = json.loads(self.rfile.read(length))
                prompt = payload["prompt"]
                task_id = prompt.split("Crew task id: ", 1)[1].splitlines()[0]
                with owner.lock:
                    owner.attempts[task_id] = owner.attempts.get(task_id, 0) + 1
                    attempt = owner.attempts[task_id]
                    owner.prompts[task_id] = prompt
                    owner.active += 1
                    owner.max_active = max(owner.max_active, owner.active)
                time.sleep(0.05)
                should_fail = owner.fail_first_for == task_id and attempt == 1
                body = {
                    "ok": not should_fail,
                    "request_id": payload["request_id"],
                    "session_id": payload.get("session_id") or str(uuid.uuid4()),
                    "wall_ms": 50,
                    "stdout": "" if should_fail else f"result from {task_id}",
                    "stderr": "transient fake failure" if should_fail else "",
                    "cwd": payload["cwd"],
                    "usage": {},
                }
                encoded = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)
                with owner.lock:
                    owner.active -= 1

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self):
        host, port = self.server.server_address
        return f"http://{host}:{port}"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class CrewDemoTests(unittest.TestCase):
    def write_workflow(self, root, max_attempts=2):
        workflow = {
            "name": "test-crew",
            "tasks": [
                {
                    "id": "alpha",
                    "role": "worker",
                    "objective": "alpha",
                    "depends_on": [],
                    "max_attempts": max_attempts,
                },
                {
                    "id": "beta",
                    "role": "worker",
                    "objective": "beta",
                    "depends_on": [],
                    "max_attempts": max_attempts,
                },
                {
                    "id": "lead",
                    "role": "lead",
                    "objective": "combine",
                    "depends_on": ["alpha", "beta"],
                    "max_attempts": max_attempts,
                },
            ],
        }
        path = Path(root) / "workflow.json"
        path.write_text(json.dumps(workflow))
        return path

    def test_parallel_workers_feed_dependent_lead_and_persist(self):
        with tempfile.TemporaryDirectory() as root, FakeOcean() as ocean:
            workflow = self.write_workflow(root)
            state_path = Path(root) / "state.json"
            state = crew.run_workflow(
                workflow, state_path, ocean.url, root, max_workers=2, timeout=5
            )
            self.assertEqual(state["status"], "succeeded")
            self.assertGreaterEqual(ocean.max_active, 2)
            self.assertIn("## alpha\nresult from alpha", ocean.prompts["lead"])
            self.assertIn("## beta\nresult from beta", ocean.prompts["lead"])
            durable = json.loads(state_path.read_text())
            self.assertTrue(all(task["status"] == "succeeded" for task in durable["tasks"]))
            self.assertTrue(all(task["session_id"] for task in durable["tasks"]))

    def test_transient_failure_retries_durably(self):
        with tempfile.TemporaryDirectory() as root, FakeOcean("alpha") as ocean:
            workflow = self.write_workflow(root)
            state_path = Path(root) / "state.json"
            state = crew.run_workflow(
                workflow, state_path, ocean.url, root, max_workers=2, timeout=5
            )
            self.assertEqual(state["status"], "succeeded")
            alpha = next(task for task in state["tasks"] if task["id"] == "alpha")
            self.assertEqual(alpha["attempts"], 2)
            self.assertEqual(ocean.attempts["alpha"], 2)

    def test_resume_requeues_interrupted_running_task(self):
        with tempfile.TemporaryDirectory() as root, FakeOcean() as ocean:
            workflow_path = self.write_workflow(root)
            workflow = crew.validate_workflow(crew.load_json(workflow_path))
            state_path = Path(root) / "state.json"
            state = crew.initial_state(workflow, workflow_path)
            alpha = next(task for task in state["tasks"] if task["id"] == "alpha")
            alpha["status"] = "running"
            alpha["attempts"] = 1
            state_path.write_text(json.dumps(state))

            resumed = crew.run_workflow(
                workflow_path, state_path, ocean.url, root, max_workers=2, timeout=5
            )
            alpha = next(task for task in resumed["tasks"] if task["id"] == "alpha")
            self.assertEqual(resumed["status"], "succeeded")
            self.assertEqual(alpha["attempts"], 2)

    def test_resume_never_exceeds_attempt_limit(self):
        with tempfile.TemporaryDirectory() as root, FakeOcean() as ocean:
            workflow_path = self.write_workflow(root)
            workflow = crew.validate_workflow(crew.load_json(workflow_path))
            state_path = Path(root) / "state.json"
            state = crew.initial_state(workflow, workflow_path)
            alpha = next(task for task in state["tasks"] if task["id"] == "alpha")
            alpha["status"] = "running"
            alpha["attempts"] = alpha["max_attempts"]
            state_path.write_text(json.dumps(state))

            resumed = crew.run_workflow(
                workflow_path, state_path, ocean.url, root, max_workers=2, timeout=5
            )
            alpha = next(task for task in resumed["tasks"] if task["id"] == "alpha")
            lead = next(task for task in resumed["tasks"] if task["id"] == "lead")
            self.assertEqual(resumed["status"], "failed")
            self.assertEqual(alpha["status"], "failed")
            self.assertEqual(alpha["attempts"], alpha["max_attempts"])
            self.assertEqual(lead["status"], "blocked")
            self.assertNotIn("alpha", ocean.attempts)

    def test_cycle_is_rejected(self):
        workflow = {
            "name": "cycle",
            "tasks": [
                {
                    "id": "a",
                    "role": "worker",
                    "objective": "a",
                    "depends_on": ["b"],
                },
                {
                    "id": "b",
                    "role": "worker",
                    "objective": "b",
                    "depends_on": ["a"],
                },
            ],
        }
        with self.assertRaisesRegex(ValueError, "dependency cycle"):
            crew.validate_workflow(workflow)


if __name__ == "__main__":
    unittest.main()
