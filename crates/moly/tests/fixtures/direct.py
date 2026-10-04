#!/usr/bin/env python3
"""Independent MPP peer and CLI process driver. Python stdlib, loopback HTTP only."""
import http.server
import json
import os
from pathlib import Path
import queue
import select
import selectors
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

TOKEN = "fixture-memory-token"
ROTATED = "fixture-rotated-token"
PRIVATE = "fixture-private-body-and-prompt"
METADATA = {"format": "independent.fixture.v1", "value": {"opaque": ["replay", {"n": 17}]}}


def provider(config_path):
    config = json.loads(Path(config_path).read_text())
    log = Path(config["log"])

    def record(value):
        with log.open("a") as output:
            output.write(json.dumps(value) + "\n")
            output.flush()

    def receive():
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError()
        value = json.loads(line)
        assert value["version"] == 1
        return value

    def send(value):
        sys.stdout.write(json.dumps(dict(version=1, **value)) + "\n")
        sys.stdout.flush()

    def respond(request, result):
        send({"type": "response", "id": request["id"], "result": result})

    next_reverse = 0

    def reverse(method, params):
        nonlocal next_reverse
        next_reverse += 1
        send({"type": "request", "id": next_reverse, "method": method, "params": params})
        answer = receive()
        assert answer["type"] == "response", answer
        assert answer["id"] == next_reverse
        return answer["result"]

    record({"pid": os.getpid(), "environment": dict(os.environ)})
    request = receive()
    assert request["method"] == "initialize"
    assert request["params"] == {"protocol_version": 2}
    respond(request, {"role": "model_provider", "protocol_version": 2})
    request = receive()
    method = request["method"]
    params = request["params"]
    record({"method": method, "params": params, "pid": os.getpid()})
    if method == "provider.validate":
        # A successful local validate proves dispatch, not remote service validity.
        respond(request, None)
    else:
        if method == "provider.auth":
            if params["operation"] == "login":
                result = reverse("host.interact", {
                    "attempt_id": params["attempt_id"],
                    "url": config.get("auth_url", "https://auth.example.test/authorize?state=fixture&code_challenge=synthetic"),
                })
                assert result == {"attempt_id": params["attempt_id"], "outcome": "opened"}
                reverse("host.credential.replace", {"credential": TOKEN})
                record({"replaced": TOKEN, "pid": os.getpid()})
            elif params["operation"] == "logout":
                reverse("host.credential.replace", {"credential": None})
                record({"replaced": None, "pid": os.getpid()})
        elif method == "provider.step":
            assert params["tools"] == [], "Direct mode must advertise zero tools"
            if params["messages"][-1]["text"] == "block-rotate":
                reverse("host.credential.replace", {"credential": ROTATED})
                record({"replaced": ROTATED, "pid": os.getpid()})
        else:
            raise AssertionError("unexpected MPP method")
        body = json.dumps({"method": method, "params": params, "pid": os.getpid()}).encode()
        endpoint = params["options"].get("model_endpoint", config["endpoint"])
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(urllib.request.Request(endpoint, data=body, headers={"Content-Type": "application/json"}), timeout=10) as response:
            answer = json.load(response)
        if "error" in answer:
            send({"type": "error", "id": request["id"], "error": answer["error"]})
        else:
            respond(request, answer["result"])
    # Stay alive until explicitly terminated or stdin closes. This lets tests
    # detect a host that returns without cleaning up even successful children.
    sys.stdin.buffer.read()


class Fixture:
    def __init__(self, directory, scenario):
        self.directory = Path(directory)
        self.requests = queue.Queue()
        self.release = threading.Event()
        self.registration_mode = "normal"
        self.exit_notifications = {}
        self.observed_exits = set()
        self.log = self.directory / "peer.jsonl"
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                length = int(self.headers["Content-Length"])
                request = json.loads(self.rfile.read(length))
                owner.requests.put(request)
                params = request["params"]
                if request["method"] == "provider.auth":
                    operation = params["operation"]
                    if scenario == "cancel-auth" and operation == "login":
                        assert owner.release.wait(10), "fixture auth gate timed out"
                    host = params["options"].get("host_id")
                    registration = None
                    if host:
                        registration = {"client_id": "fixture-issued-client", "host_id": host, "issuer": "https://auth.example.test"}
                        if owner.registration_mode == "conflict":
                            registration["client_id"] = "fixture-other-client"
                        elif owner.registration_mode == "secret":
                            registration["access_token"] = PRIVATE
                    result = {"attempt_id": params["attempt_id"], "authenticated": operation == "login" or (operation == "status" and bool(params["credential"])), "registration": registration, "revocation_confirmed": False if operation == "logout" else None}
                    answer = {"result": result}
                else:
                    message = params["messages"][-1]["text"]
                    if message in ("block", "block-rotate"):
                        assert owner.release.wait(10), "fixture model gate timed out"
                    if message == "fail":
                        answer = {"error": {"code": "provider_response_invalid", "message": PRIVATE}}
                    elif message == "tool":
                        answer = {"result": {"outcome": "await_host_tools", "text": "must-not-render-tool-prefix", "calls": [{"id": "fixture-call", "name": "read_file", "arguments": {"path": "must-not-read"}}], "metadata": METADATA}}
                    else:
                        answer = {"result": {"outcome": "completed", "text": "reply:" + message, "metadata": METADATA}}
                body = json.dumps(answer).encode()
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    # Cancellation killed the peer that owned this HTTP request.
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.endpoint = f"http://127.0.0.1:{self.server.server_port}/exact/model?fixture=yes"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.config_path = self.directory / "fixture.json"
        self.config_path.write_text(json.dumps({"endpoint": self.endpoint, "log": str(self.log)}))
        self.executable = self.directory / "independent-provider"
        source = str(Path(__file__).resolve())
        self.executable.write_text(f"#!{sys.executable}\nimport runpy\nrunpy.run_path({source!r})['provider']({str(self.config_path)!r})\n")
        self.executable.chmod(0o700)

    def next(self, method):
        request = self.requests.get(timeout=6)
        assert request["method"] == method, request
        self.watch_exit(request["pid"])
        return request

    def watch_exit(self, pid):
        if pid in self.exit_notifications:
            return
        try:
            if hasattr(os, "pidfd_open"):
                self.exit_notifications[pid] = os.pidfd_open(pid)
            else:
                watcher = select.kqueue()
                try:
                    watcher.control([select.kevent(pid, filter=select.KQ_FILTER_PROC, flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT, fflags=select.KQ_NOTE_EXIT)], 0, 0)
                except Exception:
                    watcher.close()
                    raise
                self.exit_notifications[pid] = watcher
        except ProcessLookupError:
            # Successful operations have already reaped their Provider before
            # rendering the prompt, so observing the request can happen afterward.
            pass

    def records(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def assert_children_gone(self):
        pids = {entry["pid"] for entry in self.records()}
        for pid in pids:
            if pid in self.observed_exits:
                continue
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                self.observed_exits.add(pid)
                continue
            self.watch_exit(pid)
            watcher = self.exit_notifications.get(pid)
            if watcher is None:
                continue
            # SIGKILL delivery is asynchronous. Wait on an OS exit notification,
            # not a polling sleep or an assumption about process scheduling.
            if isinstance(watcher, int):
                assert select.select([watcher], [], [], 3)[0], f"MPP child did not exit: {pid}"
            else:
                assert watcher.control(None, 1, 3), f"MPP child did not exit: {pid}"
            self.observed_exits.add(pid)

    def close(self):
        for watcher in self.exit_notifications.values():
            if isinstance(watcher, int):
                os.close(watcher)
            else:
                watcher.close()
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class Cli:
    def __init__(self, binary, fixture, profile="openai-codex", args=None, extra_env=None):
        self.fixture = fixture
        environment = {
            "MOLY_PROVIDER": profile,
            "MOLY_PROVIDER_EXECUTABLE": str(fixture.executable),
            "MOLY_AUTH_STATE_FILE": str(fixture.directory / "auth.json"),
            "MOLY_MODEL": "explicit-fixture-model",
            "MOLY_MODEL_ENDPOINT": fixture.endpoint,
            "MOLY_API_KEY": "must-ignore-ambient-key\n" if profile == "openai-codex" else "fixture-api-key",
            "RUST_LOG": "off",
            "FIXTURE_AMBIENT_SECRET": PRIVATE,
        }
        if extra_env:
            environment.update(extra_env)
        self.child = subprocess.Popen([binary] + (args if args is not None else ["--direct"]), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=fixture.directory, env=environment, start_new_session=True)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.child.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.stdout = ""
        self.stderr = ""
        self.finished = False

    def send(self, text):
        self.child.stdin.write(text.encode())
        self.child.stdin.flush()

    def until(self, suffix):
        suffix = suffix.encode()
        deadline = time.monotonic() + 6
        while suffix not in self.buffer:
            ready = self.selector.select(max(0, deadline - time.monotonic()))
            assert ready, f"CLI output timeout; buffered={self.buffer!r}"
            data = os.read(self.child.stdout.fileno(), 16384)
            if not data:
                status = self.child.wait(timeout=2)
                raise AssertionError(f"CLI stdout EOF ({status}): {self.child.stderr.read().decode()}")
            self.buffer += data
        end = self.buffer.index(suffix) + len(suffix)
        output = self.buffer[:end].decode()
        self.buffer = self.buffer[end:]
        self.stdout += output
        return output

    def prompt(self):
        return self.until("moly> ")

    def finish(self, quit=True):
        if quit:
            self.send("/quit\n")
        if self.child.stdin is not None:
            self.child.stdin.close()
            self.child.stdin = None
        output, error = self.child.communicate(timeout=6)
        self.stdout += self.buffer.decode() + output.decode()
        self.buffer = b""
        self.stderr += error.decode()
        self.finished = True
        assert self.child.returncode == 0, self.stderr
        for private in [TOKEN, ROTATED, PRIVATE, "must-ignore-ambient-key", "fixture-api-key"]:
            assert private not in self.stdout + self.stderr, "private data leaked by CLI"
        assert "Spawned unmanaged Agent Server" not in self.stderr
        self.fixture.assert_children_gone()
        return self.stderr

    def close(self):
        self.selector.close()
        if self.child.poll() is None:
            self.child.kill()
            self.child.communicate(timeout=4)
        for pipe in [self.child.stdin, self.child.stdout, self.child.stderr]:
            if pipe:
                pipe.close()


def auth(cli, fixture, operation, authenticated):
    cli.send(operation + "\n")
    text = cli.prompt()
    if operation == "/login":
        assert "Open this HTTPS URL in your browser:" in text
    assert f"Authentication: {'signed in' if authenticated else 'signed out'}" in text, text
    return fixture.next("provider.auth")


def turn(cli, fixture, message, expected=True):
    cli.send(message + "\n")
    output = cli.prompt()
    request = fixture.next("provider.step")
    if expected:
        assert "reply:" + (message[1:] if message.startswith("//") else message) in output, output
    else:
        assert "reply:" + message not in output
    return request["params"]


def run_scenario(binary, scenario):
    with tempfile.TemporaryDirectory(prefix="moly-direct-") as directory:
        # Place only the CLI in its binary directory. Any accidental Server spawn
        # fails deterministically even when target/debug contains moly-server.
        isolated = Path(directory) / "bin"
        isolated.mkdir()
        executable = isolated / "moly"
        shutil.copy2(binary, executable)
        fixture = Fixture(directory, scenario)
        clients = []
        try:
            def start(**kwargs):
                cli = Cli(str(executable), fixture, **kwargs)
                clients.append(cli)
                assert cli.prompt() == "moly> "
                return cli

            if scenario == "lazy":
                cli = start(extra_env={"MOLY_PROVIDER": "unknown-private-profile", "MOLY_PROVIDER_EXECUTABLE": "/nonexistent/provider", "MOLY_MODEL": ""})
                assert not fixture.log.exists()
                assert not (Path(directory) / "auth.json").exists()
                cli.send("\n/help\n/new\n/unknown\n/quit\n")
                diagnostic = cli.finish(quit=False)
                assert "Unknown command" in diagnostic
                assert "error:" not in diagnostic
                assert not fixture.log.exists()
                assert not (Path(directory) / "auth.json").exists()
                for args in [["--direct", "--connect", "missing"], ["--connect", "missing", "--direct"]]:
                    output = subprocess.run([str(executable)] + args, input=b"", capture_output=True, env={}, timeout=6)
                    assert output.returncode == 2
                    assert b"usage:" in output.stderr
                help_output = subprocess.run([str(executable), "--help"], capture_output=True, env={}, timeout=6)
                assert help_output.returncode == 0 and b"--direct" in help_output.stdout
            elif scenario == "auth":
                cli = start()
                status = auth(cli, fixture, "/auth", False)
                assert status["params"]["credential"] is None
                host = status["params"]["options"]["host_id"]
                login = auth(cli, fixture, "/login", True)
                assert login["params"]["options"]["registration"]["client_id"] == "fixture-issued-client"
                assert login["params"]["credential"] is None
                status = auth(cli, fixture, "/auth status", True)
                assert status["params"]["credential"] == TOKEN
                message = turn(cli, fixture, "first")
                assert message["credential"] == TOKEN
                cli.send("/new\n")
                cli.prompt()
                second = turn(cli, fixture, "second")
                assert second["credential"] == TOKEN
                assert second["context"]["session_id"] != message["context"]["session_id"]
                logout = auth(cli, fixture, "/logout", False)
                assert logout["params"]["credential"] == TOKEN
                assert "Upstream revocation was not confirmed." in cli.stdout
                assert auth(cli, fixture, "/auth", False)["params"]["credential"] is None
                auth(cli, fixture, "/login", True)
                cli.finish()
                path = Path(directory) / "auth.json"
                state = json.loads(path.read_text())
                assert state["host_id"] == host
                assert set(state) == {"host_id", "registration"}
                assert path.stat().st_mode & 0o777 == 0o600
                assert Path(str(path) + ".lock").stat().st_mode & 0o777 == 0o600
                assert TOKEN not in path.read_text()
                cli = start()
                restarted = auth(cli, fixture, "/auth", False)
                assert restarted["params"]["credential"] is None, "credentials must vanish on CLI exit"
                assert restarted["params"]["options"]["host_id"] == host
                assert restarted["params"]["options"]["registration"] == state["registration"]
                fixture.registration_mode = "conflict"
                cli.send("/auth\n")
                assert "Authentication:" not in cli.prompt()
                fixture.next("provider.auth")
                assert json.loads(path.read_text()) == state
                fixture.registration_mode = "secret"
                cli.send("/auth\n")
                assert "Authentication:" not in cli.prompt()
                fixture.next("provider.auth")
                assert json.loads(path.read_text()) == state
                diagnostic = cli.finish()
                assert "conflicting registration" in diagnostic
                assert "invalid nonsecret auth state" in diagnostic
                for record in fixture.records():
                    if "environment" in record:
                        assert "FIXTURE_AMBIENT_SECRET" not in record["environment"]
                        assert "MOLY_API_KEY" not in record["environment"]
                        assert record["environment"]["RUST_LOG"] == "off"
                assert not any(Path(directory).glob("server*"))
            elif scenario == "history":
                cli = start(profile="opencode-go")
                first = turn(cli, fixture, "//help")
                assert first["messages"] == [{"kind": "user", "text": "/help"}]
                assert first["options"]["profile"] == "opencode-go"
                assert first["options"]["model_endpoint"] == fixture.endpoint
                assert first["credential"] == "fixture-api-key"
                second = turn(cli, fixture, "second")
                assert second["messages"] == [
                    {"kind": "user", "text": "/help"},
                    {"kind": "assistant", "text": "reply:/help", "tool_calls": [], "metadata": METADATA},
                    {"kind": "user", "text": "second"},
                ]
                failure = turn(cli, fixture, "fail", expected=False)
                attack = turn(cli, fixture, "tool", expected=False)
                third = turn(cli, fixture, "third")
                assert [m["text"] for m in third["messages"] if m["kind"] == "user"] == ["/help", "second", "third"]
                assert "must-not-render-tool-prefix" not in cli.stdout
                for current in [second, failure, attack, third]:
                    assert current["context"]["session_id"] == first["context"]["session_id"]
                    assert current["tools"] == []
                contexts = [request["params"]["context"] for request in fixture.records() if request.get("method") == "provider.step"]
                for identity in ["run_id", "model_call_id"]:
                    assert len({context[identity] for context in contexts}) == len(contexts)
                cli.send("/new\n")
                cli.prompt()
                fresh = turn(cli, fixture, "fresh")
                assert fresh["messages"] == [{"kind": "user", "text": "fresh"}]
                assert fresh["context"]["session_id"] != first["context"]["session_id"]
                diagnostic = cli.finish()
                assert "provider_protocol" in diagnostic
                assert "provider_response_invalid" in diagnostic
                assert not (Path(directory) / "auth.json").exists()
            elif scenario == "cancel-model":
                cli = start()
                auth(cli, fixture, "/login", True)
                initial = turn(cli, fixture, "first")
                cli.send("block-rotate\n")
                blocked = fixture.next("provider.step")
                cli.send("queued\n")
                os.killpg(cli.child.pid, signal.SIGINT)
                assert cli.prompt() == "moly> "
                assert "reply:queued" in cli.prompt()
                resumed = fixture.next("provider.step")["params"]
                assert resumed["credential"] == ROTATED, "committed rotation must survive cancellation"
                assert resumed["context"]["session_id"] == initial["context"]["session_id"]
                assert [m["text"] for m in resumed["messages"] if m["kind"] == "user"] == ["first", "queued"]
                # A second active cancellation proves fresh operation ownership.
                cli.send("block\n")
                fixture.next("provider.step")
                cli.child.send_signal(signal.SIGINT)
                cli.prompt()
                assert "run cancelled" in cli.finish()
                assert sum(record.get("method") == "provider.step" and record["pid"] == blocked["pid"] for record in fixture.records()) == 1
            elif scenario == "queued-eof":
                cli = start(profile="openai")
                cli.send("block\n")
                fixture.next("provider.step")
                cli.send("queued\n")
                cli.child.stdin.close()
                cli.child.stdin = None
                fixture.release.set()
                diagnostic = cli.finish(quit=False)
                assert "reply:block" in cli.stdout
                assert "reply:queued" in cli.stdout
                assert "run cancelled" not in diagnostic
                queued = fixture.next("provider.step")["params"]
                assert len(queued["messages"]) == 3
            elif scenario == "cancel-auth":
                for intent in ["ctrl-c", "eof", "quit"]:
                    cli = start()
                    cli.send("/login\n")
                    cli.until("code_challenge=synthetic\n")
                    active = fixture.next("provider.auth")
                    if intent == "ctrl-c":
                        cli.child.send_signal(signal.SIGINT)
                        cli.prompt()
                        assert auth(cli, fixture, "/auth", True)["params"]["credential"] == TOKEN
                        diagnostic = cli.finish()
                        assert "Authentication cancelled." in diagnostic
                    elif intent == "eof":
                        cli.finish(quit=False)
                    else:
                        cli.finish()
                    assert cli.child.returncode == 0
                    assert active["params"]["credential"] is None
            elif scenario == "no-fallback":
                # Server mode must never silently contact the direct Provider.
                cli = start(args=["--connect", str(Path(directory) / "missing.sock")])
                cli.send("hello\n")
                cli.prompt()
                assert "error:" in cli.finish()
                assert not fixture.log.exists()
                cli = start(extra_env={"MOLY_PROVIDER": "unknown-private-profile"})
                cli.send("hello\n")
                cli.prompt()
                assert "invalid_config" in cli.finish()
                assert not fixture.log.exists()
                cli = start(extra_env={"MOLY_PROVIDER_EXECUTABLE": "/nonexistent/explicit-provider"})
                cli.send("hello\n")
                cli.prompt()
                diagnostic = cli.finish()
                assert "provider_unavailable" in diagnostic
                assert "could not start moly-server" not in diagnostic
                assert not fixture.log.exists()
                # Existing unsafe permissions must fail before spawning a peer.
                path = Path(directory) / "auth.json"
                path.chmod(0o644)
                cli = start()
                cli.send("/auth\n")
                cli.prompt()
                assert "owner-only" in cli.finish()
                assert not fixture.log.exists()
            else:
                raise AssertionError("unknown scenario")
        finally:
            fixture.release.set()
            for cli in clients:
                cli.close()
            fixture.close()


if __name__ == "__main__":
    assert len(sys.argv) == 3, "usage: direct.py CLI SCENARIO"
    run_scenario(sys.argv[1], sys.argv[2])
