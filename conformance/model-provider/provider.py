"""Independent stdlib MPP v2 peer; the tested host launches this fixture.

The audit is local test evidence, deliberately containing fake sensitive data.
No production Python/Rust implementation, HTTP service, or SDK is imported.
"""

import json
import os
import socket
import sys
import uuid


SCENARIO, AUDIT_PATH = sys.argv[1:3]
INSTANCE_ID = str(uuid.uuid4())
BODY_SENTINEL = "provider-message-secret-sentinel"
CODE_SENTINEL = "provider-code-secret-sentinel"
STDERR_SENTINEL = "provider-stderr-secret-sentinel"
TOOL_ARGUMENTS = {"nested": {"values": [True, None, 7]}}


def receive():
    frame = sys.stdin.buffer.readline(1024 * 1024 + 2)
    assert frame.endswith(b"\n") and len(frame) <= 1024 * 1024 + 1
    message = json.loads(frame.decode("utf-8"))
    assert message["version"] == 1
    assert message["type"] == "request"
    with open(AUDIT_PATH, "a", encoding="utf-8") as audit:
        audit.write(json.dumps({
            "pid": os.getpid(),
            "ppid": os.getppid(),
            "instance_id": INSTANCE_ID,
            "environment": dict(os.environ),
            "request": message,
        }) + "\n")
    return message


def emit(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(request, result, **overrides):
    message = {"version": 1, "type": "response", "id": request["id"], "result": result}
    message.update(overrides)
    emit(message)


def error(request, code):
    emit({"version": 1, "type": "error", "id": request["id"],
          "error": {"code": code, "message": BODY_SENTINEL}})


def completed(text):
    return {"outcome": "completed", "text": text, "metadata": None}


def tool_call(name="component_echo"):
    return {"id": "provider-call-1", "name": name, "arguments": TOOL_ARGUMENTS}


def tools(calls):
    return {"outcome": "await_host_tools", "text": None, "calls": calls, "metadata": None}


def step(request):
    # Keep this socket alive until process exit, including after a successful
    # response. EOF on the test side therefore observes process cleanup, not an
    # intentional close of the gate by a still-running fixture.
    gate = None
    if SCENARIO == "gate":
        gate = socket.create_connection(("127.0.0.1", int(sys.argv[3])))
        gate.sendall((json.dumps({"pid": os.getpid(), "ppid": os.getppid(),
                                  "context": request["params"]["context"]}) + "\n").encode())
        while True:
            token = gate.recv(1)
            if token == b"?":
                gate.sendall(b"!")
            elif token == b"+":
                break
            else:
                raise RuntimeError("test gate closed without release")
        respond(request, completed("gate released"))
    elif SCENARIO == "eof":
        os.close(sys.stdout.fileno())
    elif SCENARIO == "malformed":
        sys.stdout.write("{" + BODY_SENTINEL + "\n")
        sys.stdout.flush()
    elif SCENARIO == "mismatched_id":
        respond(request, completed(BODY_SENTINEL), id=request["id"] + 1)
    elif SCENARIO == "oversized":
        respond(request, completed(BODY_SENTINEL + "x" * (1024 * 1024)))
    elif SCENARIO == "unknown_type":
        emit({"version": 1, "type": CODE_SENTINEL, "id": request["id"], "payload": BODY_SENTINEL})
    elif SCENARIO == "unknown_error":
        error(request, CODE_SENTINEL)
    elif SCENARIO == "known_error":
        error(request, "invalid_secret")
    elif SCENARIO == "unknown_outcome":
        respond(request, {"outcome": CODE_SENTINEL, "text": BODY_SENTINEL})
    elif SCENARIO == "metadata_array":
        respond(request, {"outcome": "completed", "text": "invalid", "metadata": ["format", {}]})
    elif SCENARIO == "tool_call_arrays":
        respond(request, tools([["provider-call-1", "component_echo", {}]]))
    elif SCENARIO == "duplicate_ids":
        respond(request, tools([tool_call(), tool_call()]))
    elif SCENARIO == "unadvertised_tool":
        # An advertised first call must not execute before the entire batch is
        # validated. Different IDs keep this independent of the duplicate test.
        unadvertised = tool_call("not_advertised")
        unadvertised["id"] = "provider-call-2"
        respond(request, tools([tool_call(), unadvertised]))
    elif SCENARIO == "conversation":
        messages = request["params"]["messages"]
        if messages[-1]["kind"] == "tool_result":
            respond(request, completed("tool roundtrip complete"))
        elif sum(message["kind"] == "user" for message in messages) == 1:
            respond(request, tools([tool_call()]))
        else:
            respond(request, completed("second turn complete"))
    else:
        respond(request, completed("independent provider complete"))
    # Do not exit voluntarily after replying; the host owns invocation cleanup.
    sys.stdin.buffer.read()
    return gate


def main():
    print(STDERR_SENTINEL, file=sys.stderr, flush=True)
    initialize = receive()
    assert initialize["method"] == "initialize"
    assert initialize["params"] == {"protocol_version": 2}
    role = "wrong_role" if SCENARIO == "bad_role" else "model_provider"
    version = 1 if SCENARIO == "bad_version" else 2
    envelope_version = 2 if SCENARIO == "bad_envelope_version" else 1
    initialized = [role, version] if SCENARIO == "bad_handshake_array" else {"role": role, "protocol_version": version}
    respond(initialize, initialized, version=envelope_version)
    if SCENARIO in ("bad_role", "bad_version", "bad_envelope_version", "bad_handshake_array"):
        sys.stdin.buffer.read()
        return
    request = receive()
    if request["method"] == "provider.validate":
        if SCENARIO == "validate_unknown_error":
            error(request, CODE_SENTINEL)
        elif SCENARIO == "validate_known_error":
            error(request, "invalid_config")
        elif SCENARIO == "validate_non_null":
            respond(request, {"unexpected": BODY_SENTINEL})
        else:
            respond(request, None)
        sys.stdin.buffer.read()
    else:
        assert request["method"] == "provider.step"
        step(request)


if __name__ == "__main__":
    main()
