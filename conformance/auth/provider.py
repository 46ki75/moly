"""Independent MPP v2 auth peer, launched by the tested host.

Audit files contain deliberately fake secrets as test evidence, not production
logs. This fixture performs no OAuth or external HTTP and imports no moly code.
"""

import json
import os
import socket
import sys
import uuid


AUDIT_PATH = sys.argv[1]
INSTANCE = str(uuid.uuid4())
LOGIN_URL = "https://auth.example.invalid/sign-in?state=auth-url-private-sentinel"
LOGIN_CREDENTIAL = '{"token":"auth-secret-login-sentinel","generation":1}'
HOST_ID = 0
GATE = None


def audit(message, direction):
    with open(AUDIT_PATH, "a", encoding="utf-8") as output:
        output.write(json.dumps({"pid": os.getpid(), "ppid": os.getppid(),
                                 "instance_id": INSTANCE, "direction": direction, "message": message}) + "\n")


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


def error(request, code):
    emit({"version": 1, "type": "error", "id": request["id"],
          "error": {"code": code, "message": LOGIN_URL + LOGIN_CREDENTIAL}})


def host(method, params, request_id=None):
    global HOST_ID
    HOST_ID += 1
    request_id = HOST_ID if request_id is None else request_id
    emit({"version": 1, "type": "request", "id": request_id,
          "method": method, "params": params})
    reply = receive()
    assert reply["id"] == request_id and reply["type"] in ("response", "error")
    return reply


def replace(credential):
    return host("host.credential.replace", {"credential": credential})


def ready(options, request, phase):
    global GATE
    if "gate_port" not in options:
        return
    GATE = socket.create_connection(("127.0.0.1", options["gate_port"]))
    GATE.sendall((json.dumps({"instance_id": INSTANCE, "pid": os.getpid(),
                             "phase": phase, "request": request}) + "\n").encode())


def release():
    # Keep the socket alive until the host kills this invocation, even after a
    # reply. EOF is therefore evidence of child cleanup, not voluntary gate close.
    while True:
        token = GATE.recv(1)
        if token == b"?":
            GATE.sendall(b"!")
        elif token == b"+":
            return
        else:
            raise RuntimeError("gate closed without release")


def status(request, authenticated, attempt=None):
    respond(request, {"attempt_id": attempt or request["params"]["attempt_id"],
                      "authenticated": authenticated,
                      "registration": None, "revocation_confirmed": None})


def authenticate(request):
    params = request["params"]
    options = params["options"]
    scenario = options["scenario"]
    operation = params["operation"]
    credential = params["credential"]
    if scenario == "wrong_status":
        status(request, False, str(uuid.uuid4()))
    elif scenario == "status_write":
        replace(LOGIN_CREDENTIAL)
        status(request, credential is not None)
    elif scenario == "status_interact":
        host("host.interact", {"attempt_id": params["attempt_id"], "url": LOGIN_URL})
        status(request, credential is not None)
    elif scenario == "error":
        error(request, "auth_failed")
    elif scenario == "unknown_auth":
        error(request, "unknown_method")
    elif operation == "status":
        status(request, credential is not None)
    elif operation == "logout":
        assert replace(None)["type"] == "response"
        respond(request, {"attempt_id": params["attempt_id"], "authenticated": False,
                          "registration": None, "revocation_confirmed": False})
    elif scenario == "commit_then_gate":
        assert replace(LOGIN_CREDENTIAL)["type"] == "response"
        ready(options, request, "committed")
        release()
        status(request, True)
    else:
        ready(options, request, "interaction")
        attempt = str(uuid.uuid4()) if scenario == "wrong_interaction" else params["attempt_id"]
        url = {"http_url": "http://auth.example.invalid/private",
               "control_url": LOGIN_URL + "\n",
               "oversized_url": LOGIN_URL + "x" * 8192}.get(scenario, LOGIN_URL)
        reply = host("host.interact", {"attempt_id": attempt, "url": url},
                     request_id=0 if scenario == "zero_host_id" else None)
        if reply["type"] == "error":
            error(request, reply["error"]["code"])
            return
        assert reply["result"]["attempt_id"] == params["attempt_id"]
        outcome = reply["result"]["outcome"]
        if outcome != "opened":
            error(request, "auth_declined" if outcome == "declined" else "interaction_unavailable")
            return
        if scenario == "repeat_host_id":
            host("host.interact", {"attempt_id": attempt, "url": LOGIN_URL}, request_id=HOST_ID)
            return
        if scenario == "sequential_login":
            assert host("host.interact", {"attempt_id": attempt, "url": LOGIN_URL})["type"] == "response"
        if scenario == "pending":
            GATE.sendall(b'{"phase":"presented"}\n')
            release()
        assert replace(LOGIN_CREDENTIAL)["type"] == "response"
        status(request, True)


def step(request):
    params = request["params"]
    options = params["options"]
    scenario = options["scenario"]
    if scenario == "refresh":
        ready(options, request, "refresh")
        release()
        record = json.loads(params["credential"])
        record["generation"] += 1
        record["token"] = "auth-secret-refreshed-sentinel"
        assert replace(json.dumps(record, separators=(",", ":")))["type"] == "response"
        text = "generation " + str(record["generation"])
    elif scenario == "step_write":
        replace(LOGIN_CREDENTIAL)
        text = "scope checked"
    elif scenario == "step_interact":
        host("host.interact", {"attempt_id": str(uuid.uuid4()), "url": LOGIN_URL})
        error(request, "auth_required")
        return
    else:
        text = "model complete"
    respond(request, {"outcome": "completed", "text": text, "metadata": None})


def main():
    print(LOGIN_URL + LOGIN_CREDENTIAL, file=sys.stderr, flush=True)
    initialize = receive()
    assert initialize["method"] == "initialize"
    assert initialize["params"] == {"protocol_version": 2}
    respond(initialize, {"role": "model_provider", "protocol_version": 2})
    request = receive()
    assert request["type"] == "request" and request["id"] > initialize["id"]
    if request["method"] == "provider.validate":
        if request["params"]["scenario"] == "validate_write":
            replace(LOGIN_CREDENTIAL)
        respond(request, None)
    elif request["method"] == "provider.auth":
        authenticate(request)
    else:
        assert request["method"] == "provider.step"
        step(request)
    sys.stdin.buffer.read()


if __name__ == "__main__":
    main()
