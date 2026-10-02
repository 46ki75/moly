#!/usr/bin/env python3
"""Unix product smoke: lazy spawn, inference, Ctrl-C, then direct reattachment.

Run from repository root after `cargo build --locked --workspace`:
    python3 conformance/state-transitions/cli_unmanaged_unix.py
No credentials or external services. Cleanup owns only test-created process groups.
"""

import http.server
import json
import os
import signal
import socket
import subprocess
import threading

arrived = threading.Event()
release = threading.Event()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if body["messages"][-1]["content"] == "cancel me":
            arrived.set()
            release.wait(10)
            return
        assert body["messages"][0]["content"] == "hello"
        data = json.dumps({"choices": [{
            "message": {"role": "assistant", "content": "mock-completed"},
            "finish_reason": "stop",
        }]}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
threading.Thread(target=provider.serve_forever, daemon=True).start()
env = dict(
    os.environ,
    MOLY_MODEL_ENDPOINT=f"http://127.0.0.1:{provider.server_port}/chat/completions",
    MOLY_MODEL="mock",
    RUST_LOG="off",
)
env.pop("MOLY_API_KEY", None)
process = subprocess.Popen(
    ["target/debug/moly"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    stderr=subprocess.PIPE, env=env, start_new_session=True,
)
server_pid = None
endpoint = None


def terminate():
    for pid in (server_pid, process.pid):
        if pid is not None:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


deadline = threading.Timer(15, terminate)
deadline.start()
try:
    assert process.stdout.read(6) == b"moly> "
    process.stdin.write(b"hello\n")
    process.stdin.flush()
    spawned = process.stderr.readline().decode()
    assert spawned.startswith("Spawned unmanaged Server pid="), spawned
    server_pid = int(spawned.split("pid=", 1)[1])
    diagnostic = process.stderr.readline().decode()
    assert diagnostic.startswith("Server "), diagnostic
    endpoint = diagnostic.split(" at ", 1)[1].split("; session ", 1)[0]
    assert process.stdout.readline() == b"mock-completed\n"
    assert process.stdout.read(6) == b"moly> "

    process.stdin.write(b"cancel me\n")
    process.stdin.flush()
    assert arrived.wait(5), "model call did not start"
    # Simulate a terminal signal to the whole foreground Client process group.
    os.killpg(process.pid, signal.SIGINT)
    assert process.stderr.readline() == b"run cancelled\n"
    assert process.stdout.read(6) == b"moly> "
    release.set()
    # Idle Ctrl-C must exit even with an uncancelable terminal/pipe read outstanding.
    # Keep stdin open; EOF must not be what permits runtime shutdown.
    os.killpg(process.pid, signal.SIGINT)
    assert process.wait(timeout=5) == 0

    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(3)
        peer.connect(endpoint)
        peer.sendall(json.dumps({
            "version": 1, "type": "request", "id": 1, "method": "initialize",
            "params": {"protocol_version": 2},
        }).encode() + b"\n")
        reply = json.loads(peer.makefile("rb").readline())
        assert reply["result"]["role"] == "server"
        assert reply["result"]["server_id"] == diagnostic.split()[1]
    print("PASS: lazy spawn, inference, active/idle Ctrl-C, Server survived CLI exit")
finally:
    deadline.cancel()
    release.set()
    terminate()
    process.wait(timeout=5)
    provider.shutdown()
    if endpoint:
        try:
            os.unlink(endpoint)
        except FileNotFoundError:
            pass
        os.rmdir(os.path.dirname(endpoint))
