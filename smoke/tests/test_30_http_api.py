from __future__ import annotations

import base64
import hashlib
from http import HTTPStatus
import json
import os
import socket
import struct
import time
from urllib.parse import urlparse

from awsmoke.gateway import gateway
from awsmoke.hosts import Host
from awsmoke.http_api import HttpClient, HttpDaemon


def assert_error(response, status: HTTPStatus, code: str) -> None:
    assert response.status == status
    assert response.body["ok"] is False
    assert response.body["error"]["code"] == code


def test_http_bearer_auth_and_not_found(host: Host) -> None:
    with HttpDaemon(host) as http:
        missing = http.get("/api/v1/targets", token="")
        assert_error(missing, HTTPStatus.UNAUTHORIZED, "unauthorized")

        wrong = http.get("/api/v1/targets", token="not-the-token")
        assert_error(wrong, HTTPStatus.UNAUTHORIZED, "unauthorized")

        not_found = http.get("/api/v1/not-a-route")
        assert_error(not_found, HTTPStatus.NOT_FOUND, "not_found")


def test_http_metadata_routes(host: Host) -> None:
    with HttpDaemon(host) as http:
        targets = http.get("/api/v1/targets")
        assert targets.status == HTTPStatus.OK
        assert targets.body["ok"] is True
        assert host.target in {entry["target"] for entry in targets.body["data"]}

        status = http.get(f"/api/v1/status?target={host.target}")
        assert status.status == HTTPStatus.OK
        assert status.body["ok"] is True
        assert status.body["data"]["target"] == host.target

        all_status = http.get("/api/v1/status/all")
        assert all_status.status == HTTPStatus.OK
        assert all_status.body["ok"] is True
        assert isinstance(all_status.body["data"], list)


def test_http_up_route(host: Host) -> None:
    gateway(host, "remove", host.target, timeout=180)

    with HttpDaemon(host) as http:
        response = http.post("/api/v1/up", {"target": host.target})
        if host.runtime == "colima":
            assert_error(response, HTTPStatus.INTERNAL_SERVER_ERROR, "operation_failed")
            assert "local_ssh.mode = \"listen\"" in response.body["error"]["message"]
            return

        assert response.status == HTTPStatus.OK
        assert response.body["ok"] is True
        assert response.body["data"]["target"] == host.target


def test_http_stop_and_remove_routes(host: Host) -> None:
    gateway(host, "run", host.target, "--", "/bin/true", timeout=300).assert_success()

    with HttpDaemon(host) as http:
        stop = http.post("/api/v1/stop", {"target": host.target})
        assert stop.status == HTTPStatus.OK
        assert stop.body["ok"] is True
        assert stop.body["data"]["container"]
        assert stop.body["data"]["stopped"] is True

        remove = http.post("/api/v1/remove", {"target": host.target})
        assert remove.status == HTTPStatus.OK
        assert remove.body["ok"] is True
        assert remove.body["data"]["container"]
        assert remove.body["data"]["removed"] is True


def test_http_run_wait_exit_codes_and_output_selection(host: Host) -> None:
    with HttpDaemon(host) as http:
        success = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "command": ["/bin/bash", "-lc", "printf ok-out; printf ok-err >&2"],
                "output": ["stdout"],
            },
        )
        assert success.status == HTTPStatus.OK
        assert success.body["ok"] is True
        assert success.body["mode"] == "wait"
        assert success.body["exit_code"] == 0
        assert success.body["stdout"] == "ok-out"
        assert "stderr" not in success.body

        failed = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "command": ["/bin/bash", "-lc", "printf failed; exit 7"],
            },
        )
        assert failed.status == HTTPStatus.OK
        assert failed.body["ok"] is True
        assert failed.body["exit_code"] == 7
        assert failed.body["stdout"] == "failed"


def test_http_run_wait_json_output_projection(host: Host) -> None:
    with HttpDaemon(host) as http:
        projected = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "command": [
                    "/bin/sh",
                    "-lc",
                    "printf '%s\\n' '{\"ok\":true,\"nested\":{\"value\":42},\"items\":[\"a\",\"b\"]}'",
                ],
                "mode": "wait",
                "output": ["stdout"],
                "output_format": {"stdout": "json"},
            },
        )
        assert projected.status == HTTPStatus.OK
        assert projected.body["ok"] is True
        assert projected.body["mode"] == "wait"
        assert projected.body["exit_code"] == 0
        assert projected.body["stdout_json"]["ok"] is True
        assert projected.body["stdout_json"]["nested"]["value"] == 42
        assert projected.body["stdout_json"]["items"] == ["a", "b"]
        assert "stdout" not in projected.body

        invalid = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "command": [
                    "/bin/sh",
                    "-lc",
                    "printf 'not-json'; printf 'stderr-note' >&2; exit 7",
                ],
                "mode": "wait",
                "output": ["stdout", "stderr"],
                "output_format": {"stdout": "json"},
            },
        )
        assert invalid.status == HTTPStatus.OK
        assert invalid.body["ok"] is True
        assert invalid.body["mode"] == "wait"
        assert invalid.body["exit_code"] == 7
        assert invalid.body["stdout"] == "not-json"
        assert invalid.body["stderr"] == "stderr-note"
        assert "stdout_json" not in invalid.body
        assert invalid.body["output_errors"]["stdout"]["format"] == "json"
        assert invalid.body["output_errors"]["stdout"]["code"] == "invalid_json"


def test_http_run_detach(host: Host) -> None:
    with HttpDaemon(host) as http:
        response = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "mode": "detach",
                "command": ["/bin/bash", "-lc", "sleep 1"],
            },
        )
        assert response.status == HTTPStatus.ACCEPTED
        assert response.body["ok"] is True
        assert response.body["mode"] == "detach"
        assert response.body["status"] == "accepted"
        assert response.body["operation_id"]


def test_http_run_pty_close_terminates_container_process(host: Host) -> None:
    pidfile = f"/tmp/aw-gateway-pty-close-{host.name}.pid"
    child_pidfile = f"/tmp/aw-gateway-pty-close-{host.name}.child.pid"
    command = (
        f"rm -f {pidfile} {child_pidfile}; "
        "trap '' HUP INT TERM; "
        f"/bin/bash -lc 'trap \"\" HUP INT TERM; printf \"%s\" $$ > {child_pidfile}; "
        "while :; do sleep 1; done' & "
        f"printf '%s' $$ > {pidfile}; "
        "wait"
    )

    with HttpDaemon(host) as http:
        response = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "mode": "pty",
                "command": ["/bin/bash", "-lc", command],
                "terminal": {"cols": 80, "rows": 24},
            },
        )
        assert response.status == HTTPStatus.CREATED
        assert response.body["ok"] is True
        assert response.body["mode"] == "pty"

        with _WebSocket(http, response.body["attach_url"]) as ws:
            ws.send_text(json.dumps({"type": "auth", "token": response.body["attach_token"]}))
            ready = ws.recv_json()
            assert ready["type"] == "ready"
            _wait_for_http_command(http, host, f"test -s {pidfile}")
            _wait_for_http_command(http, host, f"test -s {child_pidfile}")
            ws.send_text(json.dumps({"type": "close"}))

        _wait_for_http_command(
            http,
            host,
            f"pid=$(cat {pidfile}) && ! kill -0 \"$pid\" 2>/dev/null",
        )
        _wait_for_http_command(
            http,
            host,
            f"pid=$(cat {child_pidfile}) && ! kill -0 \"$pid\" 2>/dev/null",
        )


def test_http_run_validation_errors(host: Host) -> None:
    with HttpDaemon(host) as http:
        empty = http.post("/api/v1/run", {"target": host.target, "command": []})
        assert_error(empty, HTTPStatus.BAD_REQUEST, "invalid_request")

        mode = http.post(
            "/api/v1/run",
            {"target": host.target, "mode": "stream", "command": ["true"]},
        )
        assert_error(mode, HTTPStatus.BAD_REQUEST, "invalid_mode")

        output = http.post(
            "/api/v1/run",
            {"target": host.target, "output": ["stdout", "stdout"], "command": ["true"]},
        )
        assert_error(output, HTTPStatus.BAD_REQUEST, "invalid_output")

        detach_output = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "mode": "detach",
                "output": ["stdout"],
                "command": ["true"],
            },
        )
        assert_error(detach_output, HTTPStatus.BAD_REQUEST, "invalid_output")


def test_http_launch_metadata_and_run(host: Host) -> None:
    with HttpDaemon(host) as http:
        launches = http.get("/api/v1/launches")
        assert launches.status == HTTPStatus.OK
        assert launches.body["ok"] is True
        assert "smoke-echo" in {entry["name"] for entry in launches.body["data"]}

        detail = http.get("/api/v1/launches/smoke-echo")
        assert detail.status == HTTPStatus.OK
        assert detail.body["ok"] is True
        assert detail.body["data"]["target"] == host.target
        assert set(detail.body["data"]["vars"]) == {"flag", "name"}

        run = http.post(
            "/api/v1/launches/smoke-echo/run",
            {
                "vars": {"name": host.name, "flag": True},
            },
        )
        assert run.status == HTTPStatus.OK
        assert run.body["ok"] is True
        assert run.body["exit_code"] == 0
        assert run.body["stdout"] == f"smoke:{host.name}:true"


def test_http_launch_validation_errors(host: Host) -> None:
    with HttpDaemon(host) as http:
        unknown = http.get("/api/v1/launches/not-real")
        assert_error(unknown, HTTPStatus.NOT_FOUND, "not_found")

        invalid = http.post(
            "/api/v1/launches/smoke-echo/run",
            {"vars": {"name": ["bad"]}},
        )
        assert_error(invalid, HTTPStatus.BAD_REQUEST, "invalid_launch_var")


def test_http_action_allowlist_blocks_disabled_route(host: Host) -> None:
    with HttpDaemon(host, config_path=host.http_limited_config_path) as http:
        targets = http.get("/api/v1/targets")
        assert targets.status == HTTPStatus.OK

        run = http.post("/api/v1/run", {"target": host.target, "command": ["true"]})
        assert_error(run, HTTPStatus.FORBIDDEN, "disabled_action")

        stop = http.post("/api/v1/stop", {"target": host.target})
        assert_error(stop, HTTPStatus.FORBIDDEN, "disabled_action")

        remove = http.post("/api/v1/remove", {"target": host.target})
        assert_error(remove, HTTPStatus.FORBIDDEN, "disabled_action")


class _WebSocket:
    def __init__(self, http: HttpClient, path: str):
        self.http = http
        self.path = path
        self.sock: socket.socket | None = None

    def __enter__(self) -> "_WebSocket":
        parsed = urlparse(self.http.base_url)
        host = parsed.hostname or "127.0.0.1"
        port = parsed.port or 80
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        sock = socket.create_connection((host, port), timeout=30)
        request = (
            f"GET {self.path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        sock.sendall(request.encode("ascii"))
        header = self._read_http_header(sock)
        if not header.startswith(b"HTTP/1.1 101 "):
            raise AssertionError(header.decode("utf-8", errors="replace"))
        expected_accept = base64.b64encode(
            hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")).digest()
        ).decode("ascii")
        if f"sec-websocket-accept: {expected_accept}".lower().encode("ascii") not in header.lower():
            raise AssertionError("websocket accept header did not match")
        self.sock = sock
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if self.sock is not None:
            self.sock.close()
            self.sock = None

    def send_text(self, text: str) -> None:
        assert self.sock is not None
        self.sock.sendall(_masked_ws_frame(0x1, text.encode("utf-8")))

    def recv_json(self) -> dict:
        deadline = 20
        assert self.sock is not None
        self.sock.settimeout(deadline)
        while True:
            opcode, payload = _read_ws_frame(self.sock)
            if opcode == 0x1:
                return json.loads(payload.decode("utf-8"))
            if opcode == 0x8:
                raise AssertionError("websocket closed before JSON frame")
            if opcode == 0x9:
                self.sock.sendall(_masked_ws_frame(0xA, payload))

    @staticmethod
    def _read_http_header(sock: socket.socket) -> bytes:
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                break
            data += chunk
        return data


def _masked_ws_frame(opcode: int, payload: bytes) -> bytes:
    first = 0x80 | opcode
    length = len(payload)
    if length < 126:
        header = struct.pack("!BB", first, 0x80 | length)
    elif length <= 0xFFFF:
        header = struct.pack("!BBH", first, 0x80 | 126, length)
    else:
        header = struct.pack("!BBQ", first, 0x80 | 127, length)
    mask = os.urandom(4)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return header + mask + masked


def _read_ws_frame(sock: socket.socket) -> tuple[int, bytes]:
    header = _recv_exact(sock, 2)
    first, second = header
    opcode = first & 0x0F
    masked = second & 0x80
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", _recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", _recv_exact(sock, 8))[0]
    mask = _recv_exact(sock, 4) if masked else b""
    payload = _recv_exact(sock, length)
    if mask:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return opcode, payload


def _recv_exact(sock: socket.socket, count: int) -> bytes:
    data = b""
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise AssertionError("websocket closed while reading frame")
        data += chunk
    return data


def _wait_for_http_command(http: HttpClient, host: Host, command: str) -> None:
    deadline = time.monotonic() + 20
    last = None
    while time.monotonic() < deadline:
        last = http.post(
            "/api/v1/run",
            {
                "target": host.target,
                "command": ["/bin/bash", "-lc", command],
            },
        )
        assert last.status == HTTPStatus.OK
        assert last.body["ok"] is True
        if last.body["exit_code"] == 0:
            return
        time.sleep(0.25)
    raise AssertionError(f"command did not succeed before timeout: {command!r}; last={last.body!r}")
