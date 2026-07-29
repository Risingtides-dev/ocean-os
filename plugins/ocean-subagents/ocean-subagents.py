#!/usr/bin/env python3
"""Permission-gated Ocean subagents as an ocean-plugin subprocess."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import secrets
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
MAX_ACTIVE = 4
MAX_OUTPUT_BYTES = 24_000
MAX_TASK_BYTES = 64_000
DEFAULT_TIMEOUT = 600
MAX_TIMEOUT = 1800
WORKER_AGENT = "ocean-subagent-worker"
ACTIVE_STATES = {"queued", "running", "waiting_for_permission", "cancelling"}
TERMINAL_STATES = {"completed", "failed", "cancelled"}
REQUEST_STATE_MAP = {
    "queued": "queued",
    "running": "running",
    "waiting_for_permission": "waiting_for_permission",
    "cancelling": "cancelling",
    "completed": "completed",
    "errored": "failed",
    "cancelled": "cancelled",
}

TOOLS = [
    {
        "name": "spawn",
        "description": "Start a bounded Ocean subagent in a new durable child session. Returns immediately with run_id, turn_id, and session_id. Use status or wait to collect its result.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The complete task for the subagent."},
                "role": {"type": "string", "description": "Short specialist role, for example reviewer or researcher."},
                "cwd": {"type": "string", "description": "Absolute working directory. Defaults to the configured subagent working directory."},
                "model": {"type": "string", "description": "Optional Ocean model alias for this child turn."},
                "timeout_seconds": {"type": "integer", "minimum": 30, "maximum": 1800, "description": "Elapsed-time ceiling; default 600 seconds."},
            },
            "required": ["task"],
        },
    },
    {
        "name": "status",
        "description": "Refresh one Ocean subagent from daemon request/session truth and return its status and result when complete.",
        "inputSchema": {
            "type": "object",
            "properties": {"run_id": {"type": "string"}},
            "required": ["run_id"],
        },
    },
    {
        "name": "wait",
        "description": "Wait up to 20 seconds for one Ocean subagent, then return its current status and result. Call again when still running or waiting for permission.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "timeout_seconds": {"type": "number", "minimum": 0, "maximum": 20},
            },
            "required": ["run_id"],
        },
    },
    {
        "name": "send",
        "description": "Send a follow-up task to a completed Ocean subagent's existing durable session. Returns immediately with the new turn id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "message": {"type": "string"},
            },
            "required": ["run_id", "message"],
        },
    },
    {
        "name": "permissions",
        "description": "List pending permission requests for one Ocean subagent. Inspect these before calling decide.",
        "inputSchema": {
            "type": "object",
            "properties": {"run_id": {"type": "string"}},
            "required": ["run_id"],
        },
    },
    {
        "name": "decide",
        "description": "Resolve one pending child permission after operator review. The permission must belong to this run and expected_tool must match daemon truth.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "run_id": {"type": "string"},
                "permission_id": {"type": "string"},
                "expected_tool": {"type": "string"},
                "decision": {"type": "string", "enum": ["allow", "allow_session", "deny"]},
                "reason": {"type": "string"},
            },
            "required": ["run_id", "permission_id", "expected_tool", "decision"],
        },
    },
    {
        "name": "cancel",
        "description": "Cancel the active turn for an Ocean subagent run.",
        "inputSchema": {
            "type": "object",
            "properties": {"run_id": {"type": "string"}},
            "required": ["run_id"],
        },
    },
    {
        "name": "list",
        "description": "List Ocean subagent runs persisted by this plugin, newest first.",
        "inputSchema": {
            "type": "object",
            "properties": {"active_only": {"type": "boolean"}},
        },
    },
]


class PluginError(Exception):
    pass


def now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def parse_iso(value: str) -> dt.datetime:
    return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))


def bounded_text(value: Any, field: str, maximum: int, required: bool = True) -> str | None:
    if value is None and not required:
        return None
    if not isinstance(value, str):
        raise PluginError(f"{field} must be a string")
    text = value.strip()
    if required and not text:
        raise PluginError(f"{field} must not be empty")
    if len(text.encode()) > maximum:
        raise PluginError(f"{field} exceeds {maximum} bytes")
    return text


def state_root() -> Path:
    configured = os.environ.get("OCEAN_SUBAGENT_STATE_DIR")
    if configured:
        return Path(configured).expanduser()
    xdg = os.environ.get("XDG_STATE_HOME")
    if xdg:
        return Path(xdg).expanduser() / "ocean/subagents"
    return Path.home() / ".local/state/ocean/subagents"


def default_cwd() -> str:
    configured = os.environ.get("OCEAN_SUBAGENT_DEFAULT_CWD")
    return configured or str(Path.home())


def validate_cwd(raw: Any) -> str:
    text = bounded_text(raw if raw is not None else default_cwd(), "cwd", 4096)
    assert text is not None
    path = Path(text).expanduser()
    if not path.is_absolute():
        raise PluginError("cwd must be an absolute path")
    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise PluginError(f"cwd is unavailable: {error}") from error
    if not resolved.is_dir():
        raise PluginError("cwd must be a directory")
    return str(resolved)


def validate_timeout(value: Any, default: int = DEFAULT_TIMEOUT) -> int:
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise PluginError("timeout_seconds must be a number")
    seconds = int(value)
    if seconds < 30 or seconds > MAX_TIMEOUT:
        raise PluginError(f"timeout_seconds must be between 30 and {MAX_TIMEOUT}")
    return seconds


class JsonStore:
    def __init__(self, root: Path):
        self.root = root
        self.path = root / "runs.json"
        self.lock = threading.RLock()
        self.data: dict[str, Any] = {"schema_version": SCHEMA_VERSION, "runs": {}}
        self._load()

    def _load(self) -> None:
        with self.lock:
            if not self.path.exists():
                return
            try:
                loaded = json.loads(self.path.read_text())
            except (OSError, json.JSONDecodeError) as error:
                raise PluginError(f"subagent state is unreadable: {error}") from error
            if loaded.get("schema_version") != SCHEMA_VERSION or not isinstance(
                loaded.get("runs"), dict
            ):
                raise PluginError("unsupported subagent state schema")
            self.data = loaded

    def get(self, run_id: str) -> dict[str, Any]:
        with self.lock:
            run = self.data["runs"].get(run_id)
            if run is None:
                raise PluginError(f"unknown run_id: {run_id}")
            return json.loads(json.dumps(run))

    def all(self) -> list[dict[str, Any]]:
        with self.lock:
            return json.loads(json.dumps(list(self.data["runs"].values())))

    def put(self, run: dict[str, Any]) -> dict[str, Any]:
        with self.lock:
            run = json.loads(json.dumps(run))
            run["updated_at"] = now_iso()
            self.data["runs"][run["run_id"]] = run
            self._write_locked()
            return json.loads(json.dumps(run))

    def update(self, run_id: str, **fields: Any) -> dict[str, Any]:
        with self.lock:
            run = self.data["runs"].get(run_id)
            if run is None:
                raise PluginError(f"unknown run_id: {run_id}")
            run.update(fields)
            run["updated_at"] = now_iso()
            self._write_locked()
            return json.loads(json.dumps(run))

    def _write_locked(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        encoded = (json.dumps(self.data, sort_keys=True, indent=2) + "\n").encode()
        fd, temporary = tempfile.mkstemp(prefix=".runs.", dir=self.root)
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "wb") as output:
                output.write(encoded)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, self.path)
            directory_fd = os.open(self.root, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        finally:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass


class DaemonClient:
    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")

    def request(
        self, method: str, path: str, body: dict[str, Any] | None = None, timeout: float = 10
    ) -> dict[str, Any]:
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.base_url + path,
            data=data,
            method=method,
            headers={"Accept": "application/json", "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
        except urllib.error.HTTPError as error:
            detail = error.read().decode(errors="replace")[:1000]
            raise PluginError(f"Ocean daemon HTTP {error.code}: {detail}") from error
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise PluginError(f"Ocean daemon unavailable: {error}") from error
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as error:
            raise PluginError("Ocean daemon returned invalid JSON") from error
        if not isinstance(value, dict):
            raise PluginError("Ocean daemon returned a non-object response")
        return value

    def start_turn(
        self,
        prompt: str,
        cwd: str,
        session_id: str | None = None,
        model: str | None = None,
        decision_token: str | None = None,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "prompt": prompt,
            "cwd": cwd,
            "client_type": "ocean-subagent",
            "agent": WORKER_AGENT,
            "advisor": {"enabled": False},
        }
        if session_id:
            payload["session_id"] = session_id
        if model:
            payload["model_id"] = model
        if decision_token:
            payload["decision_token"] = decision_token
        response = self.request("POST", "/v1/agent/turns", payload)
        if not response.get("ok"):
            raise PluginError(str(response.get("error") or "subagent turn was rejected"))
        turn_id = response.get("turn_id")
        returned_session = response.get("session_id")
        if not isinstance(turn_id, str) or not isinstance(returned_session, str):
            raise PluginError("subagent turn response omitted identifiers")
        return response

    def request_status(self, request_id: str) -> dict[str, Any] | None:
        response = self.request("GET", "/v1/requests")
        for item in response.get("requests", []):
            if isinstance(item, dict) and item.get("request_id") == request_id:
                return item
        return None

    def output(self, session_id: str) -> str | None:
        quoted = urllib.parse.quote(session_id, safe="")
        response = self.request("GET", f"/v1/sessions/{quoted}")
        transcript = response.get("session", {}).get("transcript", [])
        for row in reversed(transcript if isinstance(transcript, list) else []):
            if isinstance(row, dict) and row.get("role") == "assistant":
                text = row.get("text")
                if isinstance(text, str) and text.strip():
                    encoded = text.strip().encode()
                    if len(encoded) > MAX_OUTPUT_BYTES:
                        encoded = encoded[:MAX_OUTPUT_BYTES]
                        text = encoded.decode(errors="ignore") + "\n[output truncated]"
                    return text
        return None

    def pending_permissions(self) -> list[dict[str, Any]]:
        response = self.request("GET", "/v1/permissions")
        permissions = response.get("permissions", [])
        return [item for item in permissions if isinstance(item, dict)]

    def decide_permission(
        self,
        permission_id: str,
        decision: str,
        decision_token: str,
        reason: str | None = None,
    ) -> dict[str, Any]:
        quoted = urllib.parse.quote(permission_id, safe="")
        body: dict[str, Any] = {
            "permission_id": permission_id,
            "decision": decision,
            "decision_token": decision_token,
        }
        if decision == "deny" and reason:
            body["reason"] = reason
        return self.request("POST", f"/v1/permissions/{quoted}/decision", body)

    def cancel(self, request_id: str) -> dict[str, Any]:
        quoted = urllib.parse.quote(request_id, safe="")
        return self.request("POST", f"/v1/requests/{quoted}/cancel", {})


class Subagents:
    def __init__(self, client: DaemonClient, store: JsonStore):
        self.client = client
        self.store = store
        self.watchdogs: set[tuple[str, str]] = set()
        self.watchdog_lock = threading.Lock()
        for run in store.all():
            if run.get("status") in ACTIVE_STATES:
                self._start_watchdog(run["run_id"])

    def _prompt(self, task: str, role: str) -> str:
        return (
            "You are a bounded Ocean subagent working for a parent Ocean session.\n"
            f"Role: {role}\n\n"
            f"Task:\n{task}\n\n"
            "Work directly on this task. Use only the tools exposed in this child session. "
            "Do not delegate, spawn subagents, or call ocean-subagent tools. Respect repository "
            "instructions and normal permission gates. When finished, return a concise result "
            "with evidence, changed files if any, verification performed, and residual risks."
        )

    def spawn(self, args: dict[str, Any]) -> dict[str, Any]:
        task = bounded_text(args.get("task"), "task", MAX_TASK_BYTES)
        role = bounded_text(args.get("role") or "general worker", "role", 200)
        model = bounded_text(args.get("model"), "model", 200, required=False)
        cwd = validate_cwd(args.get("cwd"))
        timeout_seconds = validate_timeout(args.get("timeout_seconds"))
        for existing in self.store.all():
            if existing.get("status") in ACTIVE_STATES:
                try:
                    self.refresh(existing["run_id"])
                except PluginError:
                    pass
        active = [run for run in self.store.all() if run.get("status") in ACTIVE_STATES]
        if len(active) >= MAX_ACTIVE:
            raise PluginError(f"subagent concurrency limit reached ({MAX_ACTIVE})")
        assert task is not None and role is not None
        decision_token = secrets.token_urlsafe(48)
        response = self.client.start_turn(
            self._prompt(task, role), cwd, model=model, decision_token=decision_token
        )
        now = now_iso()
        run = {
            "run_id": str(uuid.uuid4()),
            "task": task,
            "role": role,
            "cwd": cwd,
            "model": model,
            "status": "running",
            "turn_id": response["turn_id"],
            "request_id": response["turn_id"],
            "session_id": response["session_id"],
            "output": None,
            "error": None,
            "created_at": now,
            "updated_at": now,
            "started_at": now,
            "finished_at": None,
            "timeout_seconds": timeout_seconds,
            "decision_token": decision_token,
            "turns": [
                {
                    "turn_id": response["turn_id"],
                    "request_id": response["turn_id"],
                    "started_at": now,
                }
            ],
        }
        self.store.put(run)
        self._start_watchdog(run["run_id"])
        return self._public(run)

    def refresh(self, run_id: str) -> dict[str, Any]:
        run_id = bounded_text(run_id, "run_id", 100)
        assert run_id is not None
        run = self.store.get(run_id)
        request = self.client.request_status(run["request_id"])
        if request is None:
            return self._public(run)
        status = REQUEST_STATE_MAP.get(str(request.get("state")), run["status"])
        fields: dict[str, Any] = {"status": status}
        message = request.get("message")
        if status == "failed":
            fields["error"] = str(message or "subagent turn failed")[:2000]
        if status in TERMINAL_STATES:
            fields["finished_at"] = request.get("finished_at") or now_iso()
        if status == "completed":
            fields["output"] = self.client.output(run["session_id"])
            fields["error"] = None
        run = self.store.update(run_id, **fields)
        return self._public(run)

    def wait(self, args: dict[str, Any]) -> dict[str, Any]:
        run_id = bounded_text(args.get("run_id"), "run_id", 100)
        raw_timeout = args.get("timeout_seconds", 20)
        if isinstance(raw_timeout, bool) or not isinstance(raw_timeout, (int, float)):
            raise PluginError("timeout_seconds must be a number")
        timeout = max(0.0, min(float(raw_timeout), 20.0))
        deadline = time.monotonic() + timeout
        assert run_id is not None
        while True:
            result = self.refresh(run_id)
            if result["status"] in TERMINAL_STATES or time.monotonic() >= deadline:
                return result
            time.sleep(min(0.5, max(0.0, deadline - time.monotonic())))

    def send(self, args: dict[str, Any]) -> dict[str, Any]:
        run_id = bounded_text(args.get("run_id"), "run_id", 100)
        message = bounded_text(args.get("message"), "message", MAX_TASK_BYTES)
        assert run_id is not None and message is not None
        current = self.refresh(run_id)
        if current["status"] not in TERMINAL_STATES:
            raise PluginError("subagent still has an active turn")
        run = self.store.get(run_id)
        decision_token = secrets.token_urlsafe(48)
        response = self.client.start_turn(
            self._prompt(message, run["role"]),
            run["cwd"],
            session_id=run["session_id"],
            model=run.get("model"),
            decision_token=decision_token,
        )
        now = now_iso()
        turns = run.get("turns", [])
        turns.append(
            {
                "turn_id": response["turn_id"],
                "request_id": response["turn_id"],
                "started_at": now,
            }
        )
        run = self.store.update(
            run_id,
            task=message,
            status="running",
            turn_id=response["turn_id"],
            request_id=response["turn_id"],
            output=None,
            error=None,
            started_at=now,
            finished_at=None,
            decision_token=decision_token,
            turns=turns,
        )
        self._start_watchdog(run_id)
        return self._public(run)

    def permissions(self, args: dict[str, Any]) -> dict[str, Any]:
        run_id = bounded_text(args.get("run_id"), "run_id", 100)
        assert run_id is not None
        run = self.store.get(run_id)
        pending = [
            permission
            for permission in self.client.pending_permissions()
            if permission.get("request_id") == run["request_id"]
            and permission.get("session_id") == run["session_id"]
        ]
        return {
            "run_id": run_id,
            "status": self.refresh(run_id)["status"],
            "permissions": pending,
        }

    def decide(self, args: dict[str, Any]) -> dict[str, Any]:
        run_id = bounded_text(args.get("run_id"), "run_id", 100)
        permission_id = bounded_text(args.get("permission_id"), "permission_id", 100)
        expected_tool = bounded_text(args.get("expected_tool"), "expected_tool", 300)
        decision = bounded_text(args.get("decision"), "decision", 30)
        reason = bounded_text(args.get("reason"), "reason", 1000, required=False)
        assert run_id is not None and permission_id is not None
        assert expected_tool is not None and decision is not None
        if decision not in {"allow", "allow_session", "deny"}:
            raise PluginError("decision must be allow, allow_session, or deny")
        run = self.store.get(run_id)
        permission = next(
            (
                item
                for item in self.client.pending_permissions()
                if item.get("permission_id") == permission_id
            ),
            None,
        )
        if permission is None:
            raise PluginError("permission is not pending")
        if permission.get("request_id") != run["request_id"] or permission.get(
            "session_id"
        ) != run["session_id"]:
            raise PluginError("permission does not belong to this subagent run")
        if permission.get("tool") != expected_tool:
            raise PluginError("expected_tool does not match daemon permission truth")
        response = self.client.decide_permission(
            permission_id, decision, run["decision_token"], reason
        )
        return {
            "run_id": run_id,
            "permission_id": permission_id,
            "decision": decision,
            "ok": bool(response.get("ok")),
            "message": response.get("message"),
        }

    def cancel(self, args: dict[str, Any]) -> dict[str, Any]:
        run_id = bounded_text(args.get("run_id"), "run_id", 100)
        assert run_id is not None
        run = self.store.get(run_id)
        if run["status"] in TERMINAL_STATES:
            return self._public(run)
        self.client.cancel(run["request_id"])
        run = self.store.update(run_id, status="cancelling")
        return self._public(run)

    def list_runs(self, args: dict[str, Any]) -> dict[str, Any]:
        active_only = args.get("active_only", False)
        if not isinstance(active_only, bool):
            raise PluginError("active_only must be a boolean")
        runs = self.store.all()
        for run in list(runs):
            if run.get("status") in ACTIVE_STATES:
                try:
                    self.refresh(run["run_id"])
                except PluginError:
                    pass
        runs = self.store.all()
        if active_only:
            runs = [run for run in runs if run.get("status") in ACTIVE_STATES]
        runs.sort(key=lambda run: run.get("created_at", ""), reverse=True)
        return {
            "runs": [
                {
                    key: run.get(key)
                    for key in (
                        "run_id",
                        "role",
                        "status",
                        "turn_id",
                        "session_id",
                        "created_at",
                        "updated_at",
                    )
                }
                for run in runs[:100]
            ]
        }

    def _start_watchdog(self, run_id: str) -> None:
        run = self.store.get(run_id)
        request_id = run["request_id"]
        key = (run_id, request_id)
        with self.watchdog_lock:
            if key in self.watchdogs:
                return
            self.watchdogs.add(key)

        def watch() -> None:
            try:
                started = parse_iso(run["started_at"])
                deadline = started + dt.timedelta(seconds=run["timeout_seconds"])
                remaining = (deadline - dt.datetime.now(dt.timezone.utc)).total_seconds()
                if remaining > 0:
                    time.sleep(remaining)
                current_run = self.store.get(run_id)
                if (
                    current_run.get("request_id") == request_id
                    and current_run.get("status") in ACTIVE_STATES
                ):
                    try:
                        current = self.refresh(run_id)
                        latest = self.store.get(run_id)
                        if (
                            current["status"] in ACTIVE_STATES
                            and latest.get("request_id") == request_id
                        ):
                            self.client.cancel(request_id)
                            self.store.update(
                                run_id,
                                status="cancelling",
                                error="elapsed-time ceiling reached; cancellation requested",
                            )
                    except PluginError as error:
                        self.store.update(run_id, error=str(error)[:2000])
            finally:
                with self.watchdog_lock:
                    self.watchdogs.discard(key)

        threading.Thread(target=watch, name=f"subagent-{run_id[:8]}", daemon=True).start()

    @staticmethod
    def _public(run: dict[str, Any]) -> dict[str, Any]:
        return {
            key: run.get(key)
            for key in (
                "run_id",
                "role",
                "status",
                "turn_id",
                "session_id",
                "output",
                "error",
                "created_at",
                "updated_at",
                "finished_at",
            )
        }

    def invoke(self, name: str, args: dict[str, Any]) -> dict[str, Any]:
        if not isinstance(args, dict):
            raise PluginError("tool arguments must be an object")
        if name == "spawn":
            return self.spawn(args)
        if name == "status":
            return self.refresh(str(args.get("run_id", "")))
        if name == "wait":
            return self.wait(args)
        if name == "send":
            return self.send(args)
        if name == "permissions":
            return self.permissions(args)
        if name == "decide":
            return self.decide(args)
        if name == "cancel":
            return self.cancel(args)
        if name == "list":
            return self.list_runs(args)
        raise PluginError(f"unknown tool: {name}")


def rpc_error(message_id: Any, code: int, message: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": message_id,
        "error": {"code": code, "message": message},
    }


def serve(manager: Subagents) -> int:
    for raw in sys.stdin:
        if not raw.strip():
            continue
        try:
            message = json.loads(raw)
        except json.JSONDecodeError:
            continue
        message_id = message.get("id")
        if message_id is None:
            continue
        try:
            method = message.get("method")
            if method == "list_tools":
                result = {"tools": TOOLS}
            elif method == "invoke_tool":
                params = message.get("params")
                if not isinstance(params, dict):
                    raise PluginError("invoke_tool params must be an object")
                result = manager.invoke(str(params.get("name", "")), params.get("args", {}))
            else:
                response = rpc_error(message_id, -32601, f"method not found: {method}")
                print(json.dumps(response, separators=(",", ":")), flush=True)
                continue
            response = {"jsonrpc": "2.0", "id": message_id, "result": result}
        except PluginError as error:
            response = rpc_error(message_id, -32602, str(error))
        except Exception as error:  # keep one malformed call from killing the plugin
            print(f"ocean-subagents internal error: {error}", file=sys.stderr, flush=True)
            response = rpc_error(message_id, -32603, "internal plugin error")
        print(json.dumps(response, separators=(",", ":")), flush=True)
    return 0


def build_manager() -> Subagents:
    url = os.environ.get("OCEAN_DAEMON_URL", "http://127.0.0.1:4780")
    return Subagents(DaemonClient(url), JsonStore(state_root()))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="validate startup and print tool names")
    args = parser.parse_args(argv)
    manager = build_manager()
    if args.check:
        print(json.dumps({"ok": True, "tools": [tool["name"] for tool in TOOLS]}))
        return 0
    return serve(manager)


if __name__ == "__main__":
    raise SystemExit(main())
