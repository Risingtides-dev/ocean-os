#!/usr/bin/env python3
"""Durable local Crew demo over Ocean's existing /v1/prompt agent loop."""

from __future__ import annotations

import argparse
import concurrent.futures
import copy
import datetime as dt
import hashlib
import json
import os
import secrets
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
TERMINAL = {"succeeded", "failed", "blocked"}
OUTPUT_LIMIT = 12_000
DEPENDENCY_CONTEXT_LIMIT = 24_000


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def log(status: str, message: str) -> None:
    stamp = dt.datetime.now().strftime("%H:%M:%S")
    print(f"[{stamp}] {status:<10} {message}", flush=True)


def canonical_digest(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return "sha256:" + hashlib.sha256(encoded).hexdigest()


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text())
    except FileNotFoundError as error:
        raise ValueError(f"file not found: {path}") from error
    except json.JSONDecodeError as error:
        raise ValueError(f"invalid JSON in {path}: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def validate_workflow(workflow: dict[str, Any]) -> dict[str, Any]:
    name = workflow.get("name")
    tasks = workflow.get("tasks")
    if not isinstance(name, str) or not name.strip():
        raise ValueError("workflow.name must be a non-empty string")
    if not isinstance(tasks, list) or not tasks:
        raise ValueError("workflow.tasks must be a non-empty array")

    normalized: list[dict[str, Any]] = []
    ids: set[str] = set()
    for raw in tasks:
        if not isinstance(raw, dict):
            raise ValueError("every task must be an object")
        task_id = raw.get("id")
        role = raw.get("role")
        objective = raw.get("objective")
        depends_on = raw.get("depends_on", [])
        max_attempts = raw.get("max_attempts", 2)
        if not isinstance(task_id, str) or not task_id.strip():
            raise ValueError("every task.id must be a non-empty string")
        if task_id in ids:
            raise ValueError(f"duplicate task id: {task_id}")
        if not isinstance(role, str) or not role.strip():
            raise ValueError(f"task {task_id}: role must be a non-empty string")
        if not isinstance(objective, str) or not objective.strip():
            raise ValueError(f"task {task_id}: objective must be a non-empty string")
        if not isinstance(depends_on, list) or not all(
            isinstance(dep, str) and dep for dep in depends_on
        ):
            raise ValueError(f"task {task_id}: depends_on must contain task ids")
        if not isinstance(max_attempts, int) or not 1 <= max_attempts <= 5:
            raise ValueError(f"task {task_id}: max_attempts must be between 1 and 5")
        ids.add(task_id)
        normalized.append(
            {
                "id": task_id,
                "role": role.strip(),
                "objective": objective.strip(),
                "depends_on": list(dict.fromkeys(depends_on)),
                "max_attempts": max_attempts,
            }
        )

    by_id = {task["id"]: task for task in normalized}
    for task in normalized:
        for dependency in task["depends_on"]:
            if dependency not in by_id:
                raise ValueError(f"task {task['id']}: unknown dependency {dependency}")
            if dependency == task["id"]:
                raise ValueError(f"task {task['id']}: self dependency")

    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(task_id: str) -> None:
        if task_id in visiting:
            raise ValueError(f"dependency cycle includes {task_id}")
        if task_id in visited:
            return
        visiting.add(task_id)
        for dependency in by_id[task_id]["depends_on"]:
            visit(dependency)
        visiting.remove(task_id)
        visited.add(task_id)

    for task_id in by_id:
        visit(task_id)
    return {"name": name.strip(), "tasks": normalized}


def initial_state(workflow: dict[str, Any], workflow_path: Path) -> dict[str, Any]:
    now = utc_now()
    return {
        "schema_version": SCHEMA_VERSION,
        "workflow_id": str(uuid.uuid4()),
        "workflow_name": workflow["name"],
        "workflow_path": str(workflow_path.resolve()),
        "workflow_digest": canonical_digest(workflow),
        "status": "pending",
        "created_at": now,
        "updated_at": now,
        "tasks": [
            {
                **copy.deepcopy(task),
                "status": "pending",
                "attempts": 0,
                "session_id": None,
                "request_id": None,
                "output": None,
                "error": None,
                "started_at": None,
                "finished_at": None,
            }
            for task in workflow["tasks"]
        ],
    }


class StateStore:
    def __init__(self, path: Path):
        self.path = path
        self.lock = threading.Lock()
        self.state: dict[str, Any] | None = None

    def load_or_create(
        self, workflow: dict[str, Any], workflow_path: Path
    ) -> dict[str, Any]:
        with self.lock:
            if self.path.exists():
                state = load_json(self.path)
                if state.get("schema_version") != SCHEMA_VERSION:
                    raise ValueError("unsupported state schema")
                if state.get("workflow_digest") != canonical_digest(workflow):
                    raise ValueError(
                        "state belongs to a different workflow; choose another --state path"
                    )
                for task in state.get("tasks", []):
                    if task.get("status") == "running":
                        task["error"] = "interrupted before durable completion"
                        task["finished_at"] = utc_now()
                        task["status"] = (
                            "pending"
                            if task.get("attempts", 0) < task.get("max_attempts", 1)
                            else "failed"
                        )
                self.state = state
            else:
                self.state = initial_state(workflow, workflow_path)
            self._write_locked()
            return self.state

    def mutate(self, callback: Any) -> dict[str, Any]:
        with self.lock:
            if self.state is None:
                raise RuntimeError("state not loaded")
            callback(self.state)
            self.state["updated_at"] = utc_now()
            self._write_locked()
            return copy.deepcopy(self.state)

    def snapshot(self) -> dict[str, Any]:
        with self.lock:
            if self.state is None:
                raise RuntimeError("state not loaded")
            return copy.deepcopy(self.state)

    def _write_locked(self) -> None:
        if self.state is None:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        encoded = (json.dumps(self.state, indent=2, sort_keys=True) + "\n").encode()
        fd, temporary = tempfile.mkstemp(
            prefix=f".{self.path.name}.", dir=self.path.parent
        )
        try:
            with os.fdopen(fd, "wb") as output:
                output.write(encoded)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, self.path)
            directory_fd = os.open(self.path.parent, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        finally:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass


def task_by_id(state: dict[str, Any], task_id: str) -> dict[str, Any]:
    return next(task for task in state["tasks"] if task["id"] == task_id)


def dependency_context(state: dict[str, Any], task: dict[str, Any]) -> str:
    sections: list[str] = []
    used = 0
    for dependency_id in task["depends_on"]:
        dependency = task_by_id(state, dependency_id)
        output = (dependency.get("output") or "")[:OUTPUT_LIMIT]
        section = f"## {dependency_id}\n{output}\n"
        remaining = DEPENDENCY_CONTEXT_LIMIT - used
        if remaining <= 0:
            break
        sections.append(section[:remaining])
        used += len(sections[-1])
    return "\n".join(sections)


def build_prompt(state: dict[str, Any], task: dict[str, Any]) -> str:
    dependencies = dependency_context(state, task)
    dependency_block = (
        f"\nCompleted dependency reports:\n{dependencies}" if dependencies else ""
    )
    return (
        "You are a bounded Ocean Crew demo worker. Do not call tools, modify files, "
        "or start other agents. Complete only the assigned reasoning task.\n\n"
        f"Crew workflow: {state['workflow_name']}\n"
        f"Crew task id: {task['id']}\n"
        f"Role: {task['role']}\n"
        f"Objective: {task['objective']}\n"
        f"{dependency_block}\n\nReturn only the requested result."
    )


def post_json(url: str, payload: dict[str, Any], timeout: float) -> dict[str, Any]:
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json", "Accept": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
    except urllib.error.HTTPError as error:
        body = error.read().decode(errors="replace")
        raise RuntimeError(f"HTTP {error.code}: {body[:500]}") from error
    except (urllib.error.URLError, TimeoutError) as error:
        raise RuntimeError(f"daemon request failed: {error}") from error
    try:
        value = json.loads(body)
    except json.JSONDecodeError as error:
        raise RuntimeError("daemon returned invalid JSON") from error
    if not isinstance(value, dict):
        raise RuntimeError("daemon returned a non-object response")
    return value


def execute_once(
    state: dict[str, Any],
    task: dict[str, Any],
    base_url: str,
    cwd: str,
    timeout: float,
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "prompt": build_prompt(state, task),
        "request_id": str(uuid.uuid4()),
        "create_if_missing": task.get("session_id") is None,
        "max_turns": 1,
        "yolo": False,
        "cwd": cwd,
        "client_type": "crew-demo",
        "decision_token": secrets.token_urlsafe(32),
    }
    if task.get("session_id"):
        payload["session_id"] = task["session_id"]
    response = post_json(base_url.rstrip("/") + "/v1/prompt", payload, timeout)
    if not response.get("ok"):
        error = response.get("stderr") or response.get("error") or "Ocean turn failed"
        raise RuntimeError(str(error)[:1000])
    output = response.get("stdout")
    if not isinstance(output, str) or not output.strip():
        raise RuntimeError("Ocean turn returned no output")
    return {
        "output": output.strip()[:OUTPUT_LIMIT],
        "session_id": response.get("session_id"),
        "request_id": response.get("request_id"),
        "wall_ms": response.get("wall_ms"),
    }


def mark_blocked_tasks(store: StateStore) -> None:
    def mutate(state: dict[str, Any]) -> None:
        changed = True
        while changed:
            changed = False
            for task in state["tasks"]:
                if task["status"] != "pending":
                    continue
                failed = [
                    dep
                    for dep in task["depends_on"]
                    if task_by_id(state, dep)["status"] in {"failed", "blocked"}
                ]
                if failed:
                    task["status"] = "blocked"
                    task["error"] = "blocked by: " + ", ".join(failed)
                    task["finished_at"] = utc_now()
                    changed = True
                    log("blocked", f"{task['id']} <- {', '.join(failed)}")

    store.mutate(mutate)


def finalize_workflow(store: StateStore) -> dict[str, Any]:
    def mutate(state: dict[str, Any]) -> None:
        statuses = {task["status"] for task in state["tasks"]}
        if statuses == {"succeeded"}:
            state["status"] = "succeeded"
        elif statuses.issubset(TERMINAL):
            state["status"] = "failed"
        else:
            state["status"] = "running"

    return store.mutate(mutate)


def run_workflow(
    workflow_path: Path,
    state_path: Path,
    base_url: str,
    cwd: str,
    max_workers: int,
    timeout: float,
) -> dict[str, Any]:
    workflow = validate_workflow(load_json(workflow_path))
    store = StateStore(state_path)
    state = store.load_or_create(workflow, workflow_path)
    log("workflow", f"{state['workflow_name']} id={state['workflow_id']}")
    log("state", str(state_path.resolve()))

    def start_task(task_id: str) -> tuple[dict[str, Any], dict[str, Any]]:
        snapshot = store.snapshot()
        task = task_by_id(snapshot, task_id)
        return snapshot, task

    futures: dict[concurrent.futures.Future[dict[str, Any]], str] = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=max_workers) as pool:
        while True:
            mark_blocked_tasks(store)
            snapshot = store.snapshot()
            unfinished = [task for task in snapshot["tasks"] if task["status"] not in TERMINAL]
            if not unfinished and not futures:
                break

            running_ids = set(futures.values())
            ready = [
                task
                for task in snapshot["tasks"]
                if task["status"] == "pending"
                and task["id"] not in running_ids
                and all(
                    task_by_id(snapshot, dep)["status"] == "succeeded"
                    for dep in task["depends_on"]
                )
            ]
            for task in ready[: max(0, max_workers - len(futures))]:
                task_id = task["id"]

                def mark_running(state: dict[str, Any], current_id: str = task_id) -> None:
                    current = task_by_id(state, current_id)
                    current["status"] = "running"
                    current["attempts"] += 1
                    current["started_at"] = utc_now()
                    current["finished_at"] = None
                    current["error"] = None
                    state["status"] = "running"

                updated = store.mutate(mark_running)
                running = task_by_id(updated, task_id)
                log(
                    "running",
                    f"{task_id} attempt={running['attempts']}/{running['max_attempts']}",
                )
                execution_state, execution_task = start_task(task_id)
                future = pool.submit(
                    execute_once,
                    execution_state,
                    execution_task,
                    base_url,
                    cwd,
                    timeout,
                )
                futures[future] = task_id

            if not futures:
                snapshot = store.snapshot()
                pending = [task["id"] for task in snapshot["tasks"] if task["status"] == "pending"]
                if pending:
                    raise RuntimeError("scheduler stalled with pending tasks: " + ", ".join(pending))
                break

            done, _ = concurrent.futures.wait(
                futures, return_when=concurrent.futures.FIRST_COMPLETED
            )
            for future in done:
                task_id = futures.pop(future)
                try:
                    result = future.result()
                except Exception as error:  # worker failures become durable state
                    message = str(error)

                    def mark_failure(state: dict[str, Any]) -> None:
                        task = task_by_id(state, task_id)
                        task["error"] = message
                        task["finished_at"] = utc_now()
                        if task["attempts"] < task["max_attempts"]:
                            task["status"] = "pending"
                        else:
                            task["status"] = "failed"

                    updated = store.mutate(mark_failure)
                    failed = task_by_id(updated, task_id)
                    if failed["status"] == "pending":
                        log("retry", f"{task_id}: {message}")
                        time.sleep(min(0.25 * failed["attempts"], 1.0))
                    else:
                        log("failed", f"{task_id}: {message}")
                else:
                    def mark_success(state: dict[str, Any]) -> None:
                        task = task_by_id(state, task_id)
                        task["status"] = "succeeded"
                        task["output"] = result["output"]
                        task["session_id"] = result.get("session_id")
                        task["request_id"] = result.get("request_id")
                        task["wall_ms"] = result.get("wall_ms")
                        task["error"] = None
                        task["finished_at"] = utc_now()

                    store.mutate(mark_success)
                    log("completed", task_id)

    result = finalize_workflow(store)
    log(result["status"], result["workflow_name"])
    return result


def print_status(state: dict[str, Any]) -> None:
    print(f"workflow: {state.get('workflow_name')} ({state.get('status')})")
    print(f"id: {state.get('workflow_id')}")
    for task in state.get("tasks", []):
        suffix = f" attempts={task.get('attempts', 0)}/{task.get('max_attempts')}"
        if task.get("session_id"):
            suffix += f" session={task['session_id']}"
        print(f"- {task.get('id')}: {task.get('status')}{suffix}")
        if task.get("error"):
            print(f"  error: {task['error']}")
        if task.get("output"):
            preview = task["output"].replace("\n", " ")[:160]
            print(f"  output: {preview}")


def default_demo_path() -> Path:
    return Path(__file__).with_name("demo-workflow.json")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser("validate", help="validate a workflow graph")
    validate.add_argument("--workflow", type=Path, required=True)

    status = subparsers.add_parser("status", help="show durable workflow state")
    status.add_argument("--state", type=Path, required=True)

    for name in ("run", "demo"):
        run = subparsers.add_parser(name, help="run or resume Ocean Crew tasks")
        run.add_argument(
            "--workflow",
            type=Path,
            default=default_demo_path() if name == "demo" else None,
            required=name == "run",
        )
        run.add_argument(
            "--state",
            type=Path,
            default=Path.home() / ".local/state/ocean/crew-demo-state.json",
        )
        run.add_argument("--url", default=os.environ.get("OCEAN_URL", "http://127.0.0.1:4780"))
        run.add_argument("--cwd", default=os.getcwd())
        run.add_argument("--max-workers", type=int, default=2)
        run.add_argument("--timeout", type=float, default=180.0)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "validate":
            workflow = validate_workflow(load_json(args.workflow))
            print(f"valid: {workflow['name']} ({len(workflow['tasks'])} tasks)")
            return 0
        if args.command == "status":
            print_status(load_json(args.state))
            return 0
        if not 1 <= args.max_workers <= 8:
            raise ValueError("--max-workers must be between 1 and 8")
        if args.timeout <= 0:
            raise ValueError("--timeout must be positive")
        state = run_workflow(
            args.workflow,
            args.state,
            args.url,
            str(Path(args.cwd).resolve()),
            args.max_workers,
            args.timeout,
        )
        print_status(state)
        return 0 if state["status"] == "succeeded" else 1
    except (ValueError, RuntimeError) as error:
        print(f"crew-demo: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
