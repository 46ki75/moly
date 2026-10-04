"""Independent stdlib MPP peer for gaps not covered by the reused fixtures.

Only synthetic secrets appear in this test-owned audit. No Rust/SDK or existing
Provider implementation is imported, and no external service is contacted.
"""

import json
import os
import socket
import sys
import uuid


SCENARIO, AUDIT_PATH = sys.argv[1:3]
INSTANCE = str(uuid.uuid4())
GATE = None
HOST_ID = 0
SECRET = "mpp-wire-secret-sentinel"
ROTATED = '{"token":"auth-secret-mpp-rotated-sentinel","generation":2}'
METADATA = {
    "format": "independent.mpp.opaque.v1",
    "value": {"reasoning": [None, {"private": "opaque α\nreplay"}], "future": True},
}


def audit(message, direction):
    with open(AUDIT_PATH, "a", encoding="utf-8") as output:
        output.write(json.dumps({
            "pid": os.getpid(), "ppid": os.getppid(), "instance_id": INSTANCE,
            "environment": dict(os.environ), "argv": sys.argv,
            "direction": direction, "message": message,
        }) + "\n")


def receive():
    frame = sys.stdin.buffer.readline(1024 * 1024 + 2)
    assert frame.endswith(b"\n") and len(frame) <= 1024 * 1024 + 1
    message = json.loads(frame)
    assert message["version"] == 1
    audit(message, "received")
    return message


def emit(message):
    audit(message, "sent")
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(request, result):
    emit({"version": 1, "type": "response", "id": request["id"], "result": result})


def host(method, params):
    global HOST_ID
    HOST_ID += 1
    emit({"version": 1, "type": "request", "id": HOST_ID,
          "method": method, "params": params})
    reply = receive()
    assert reply["id"] == HOST_ID and reply["type"] in ("response", "error")
    return reply


def ready(phase, request):
    global GATE
    GATE = socket.create_connection(("127.0.0.1", int(sys.argv[3])))
    GATE.sendall((json.dumps({"pid": os.getpid(), "instance_id": INSTANCE,
                             "phase": phase, "request": request}) + "\n").encode())
    # Never close this socket manually. EOF on the host observes process exit,
    # including when the SDK kills a child after a successful/error response.
    while True:
        token = GATE.recv(1)
        if token == b"?":
            GATE.sendall(b"!")
        elif token == b"+":
            return
        else:
            raise RuntimeError("host gate closed without release")


def completed(text="model complete", metadata=None):
    return {"outcome": "completed", "text": text, "metadata": metadata}


def call(identity="provider-call-1", arguments=None):
    return {"id": identity, "name": "component_echo",
            "arguments": {} if arguments is None else arguments}


def step(request):
    if SCENARIO == "conversation":
        stage = request["params"]["options"]["stage"]
        if stage == 0:
            respond(request, {"outcome": "await_host_tools", "text": "before tool",
                              "calls": [call(arguments={"nested": [True, None, 7]})],
                              "metadata": METADATA})
        else:
            messages = request["params"]["messages"]
            assert messages[1]["metadata"] == METADATA
            assert messages[2]["kind"] == "tool_result"
            # The host may continue even after a correlated recoverable tool
            # failure; output stays structured JSON, never a stringified object.
            assert messages[2]["output"]["error"]["code"] == "tool_file_not_found"
            respond(request, completed("" if stage == 2 else "tool continued", METADATA))
    elif SCENARIO in ("read_gate", "success_gate", "error_gate"):
        ready("step", request)
        if SCENARIO == "error_gate":
            emit({"version": 1, "type": "error", "id": request["id"],
                  "error": {"code": SECRET, "message": SECRET}})
        else:
            respond(request, completed())
    elif SCENARIO in ("step_commit_gate", "commit_error"):
        assert host("host.credential.replace", {"credential": ROTATED})["type"] == "response"
        if SCENARIO == "step_commit_gate":
            ready("committed", request)
            respond(request, completed())
        else:
            emit({"version": 1, "type": "error", "id": request["id"],
                  "error": {"code": "auth_failed", "message": SECRET + ROTATED}})
    elif SCENARIO == "credential_limit":
        reply = host("host.credential.replace", {"credential": "é" * (32 * 1024 + 1)})
        assert reply["type"] == "error" and reply["error"]["code"] == "invalid_params"
        respond(request, completed("size rejected"))
    elif SCENARIO == "reverse_limit":
        for _ in range(65):
            reply = host("host.unknown", {})
            assert reply["type"] == "error" and reply["error"]["code"] == "unknown_method"
        raise AssertionError("65th reverse request must terminate operation")
    elif SCENARIO == "partial_eof":
        sys.stdout.buffer.write(b'{"version":1')
        sys.stdout.buffer.flush()
        os.close(sys.stdout.fileno())
    elif SCENARIO == "invalid_utf8":
        sys.stdout.buffer.write(b'\xff\n')
        sys.stdout.buffer.flush()
    elif SCENARIO == "event":
        emit({"version": 1, "type": "event", "event": SECRET, "params": SECRET})
    elif SCENARIO == "missing_text":
        respond(request, {"outcome": "completed", "metadata": None})
    elif SCENARIO == "null_text":
        respond(request, {"outcome": "completed", "text": None})
    else:
        calls = {
            "empty_batch": [],
            "too_many_calls": [call(str(i)) for i in range(33)],
            "max_calls": [call(str(i)) for i in range(32)],
            "empty_id": [call("")],
            "arguments_array": [call(arguments=[SECRET])],
        }[SCENARIO]
        respond(request, {"outcome": "await_host_tools", "calls": calls, "metadata": None})


def main():
    print(SECRET, file=sys.stderr, flush=True)
    initialize = receive()
    assert initialize["type"] == "request" and initialize["method"] == "initialize"
    assert initialize["params"] == {"protocol_version": 2}
    if SCENARIO == "handshake_gate":
        ready("initialize", initialize)
    if SCENARIO == "handshake_reverse":
        host("host.credential.replace", {"credential": ROTATED})
        return
    respond(initialize, {"role": "model_provider", "protocol_version": 2})
    request = receive()
    assert request["type"] == "request" and request["id"] > initialize["id"]
    if request["method"] == "provider.validate":
        if SCENARIO == "validate_gate":
            ready("validate", request)
        respond(request, None)
    else:
        assert request["method"] == "provider.step"
        step(request)
    # No voluntary exit after a reply: the SDK must own invocation cleanup.
    sys.stdin.buffer.read()


if __name__ == "__main__":
    main()
