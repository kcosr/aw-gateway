from __future__ import annotations

from dataclasses import dataclass
from http import HTTPStatus
import json
import os
import random
import shlex
import socket
import subprocess
import tempfile
import time
from typing import Any, BinaryIO
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from .gateway import gateway_command_for_config
from .hosts import Host
from .ssh import remote


@dataclass(frozen=True)
class HttpResponse:
    status: int
    body: dict[str, Any]


class HttpClient:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.token = token

    def get(self, path: str, *, token: str | None = None) -> HttpResponse:
        return self.request("GET", path, token=token)

    def post(self, path: str, body: dict[str, Any], *, token: str | None = None) -> HttpResponse:
        return self.request("POST", path, body=body, token=token)

    def request(
        self,
        method: str,
        path: str,
        *,
        body: dict[str, Any] | None = None,
        token: str | None = None,
    ) -> HttpResponse:
        data = None
        headers = {"Accept": "application/json"}
        if token is None:
            token = self.token
        if token:
            headers["Authorization"] = f"Bearer {token}"
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = Request(
            f"{self.base_url}{path}",
            data=data,
            headers=headers,
            method=method,
        )
        try:
            with urlopen(request, timeout=180) as response:
                payload = json.loads(response.read().decode("utf-8"))
                return HttpResponse(response.status, payload)
        except HTTPError as err:
            payload = json.loads(err.read().decode("utf-8"))
            return HttpResponse(err.code, payload)


class HttpDaemon:
    def __init__(self, host: Host, *, config_path: str | None = None):
        self.host = host
        self.config_path = config_path or host.config_path
        self.local_port = _free_local_port()
        self.remote_port = random.randint(20000, 60999)
        self.remote_config_path = (
            f"/tmp/aw-gateway-smoke-http-{host.name}-{os.getpid()}-{self.remote_port}.toml"
        )
        self.process: subprocess.Popen[str] | None = None
        self.stderr_file: BinaryIO | None = None
        self.client = HttpClient(f"http://127.0.0.1:{self.local_port}", host.http_token)

    def __enter__(self) -> HttpClient:
        try:
            self._write_remote_config()
            remote_command = gateway_command_for_config(self.host, self.remote_config_path, "http")
            self.stderr_file = tempfile.TemporaryFile()
            self.process = subprocess.Popen(
                [
                    "ssh",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "ExitOnForwardFailure=yes",
                    "-L",
                    f"127.0.0.1:{self.local_port}:127.0.0.1:{self.remote_port}",
                    self.host.ssh,
                    remote_command,
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=self.stderr_file,
                text=True,
            )
            self._wait_ready()
        except Exception:
            self._cleanup()
            raise
        return self.client

    def __exit__(self, exc_type, exc, tb) -> None:
        self._cleanup()

    def signal_remote(self, signal: str) -> None:
        command = f"""
set -eu
pids="$({_listener_pids_script(self.remote_port)})"
test -n "$pids"
kill -{shlex.quote(signal)} $pids
"""
        remote(self.host.ssh, command, timeout=30).assert_success()

    def wait_exited(self, timeout: int = 20) -> None:
        if self.process is None:
            return
        try:
            self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as err:
            raise AssertionError(
                f"http daemon did not exit within {timeout}s{self._stderr_tail()}"
            ) from err

    def wait_remote_stopped(self, timeout: int = 20) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = remote(self.host.ssh, _listener_pids_script(self.remote_port), timeout=30)
            if not result.stdout.strip():
                return
            time.sleep(0.25)
        raise AssertionError(f"remote http daemon still listening after {timeout}s")

    def _cleanup(self) -> None:
        if self.process is not None:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=10)
            else:
                self.process.wait(timeout=1)
        if self.stderr_file is not None:
            self.stderr_file.close()
            self.stderr_file = None
        self._stop_remote_http()

    def _wait_ready(self) -> None:
        deadline = time.monotonic() + 45
        last_error: Exception | None = None
        while time.monotonic() < deadline:
            if self.process is not None and self.process.poll() is not None:
                raise AssertionError(
                    "http daemon exited before becoming ready "
                    f"with status {self.process.returncode}{self._stderr_tail()}"
                )
            try:
                response = self.client.get("/api/v1/targets")
                if response.status == HTTPStatus.OK:
                    return
            except (ConnectionError, TimeoutError, URLError, json.JSONDecodeError) as err:
                last_error = err
            time.sleep(0.25)
        raise AssertionError(
            f"http daemon did not become ready: {last_error!r}{self._stderr_tail()}"
        )

    def _stderr_tail(self) -> str:
        if self.stderr_file is None:
            return ""
        try:
            self.stderr_file.seek(0, os.SEEK_END)
            size = self.stderr_file.tell()
            self.stderr_file.seek(max(0, size - 65536), os.SEEK_SET)
            data = self.stderr_file.read()
        except OSError:
            return ""
        if not data:
            return ""
        text = data.decode("utf-8", errors="replace").strip()
        if not text:
            return ""
        return f"\nstderr tail:\n{text}"

    def _write_remote_config(self) -> None:
        command = (
            "sed "
            + shlex.quote(
                f's#listen = "127.0.0.1:[0-9][0-9]*"#listen = "127.0.0.1:{self.remote_port}"#'
            )
            + f" {shlex.quote(self.config_path)} > {shlex.quote(self.remote_config_path)}"
        )
        result = remote(self.host.ssh, command, timeout=30)
        result.assert_success()

    def _stop_remote_http(self) -> None:
        command = f"""
set +e
pids="$({_listener_pids_script(self.remote_port)})"
if [ -n "$pids" ]; then
  kill $pids 2>/dev/null
  sleep 0.5
  pids="$({_listener_pids_script(self.remote_port)})"
  if [ -n "$pids" ]; then
    kill -9 $pids 2>/dev/null
  fi
fi
rm -f {shlex.quote(self.remote_config_path)}
true
"""
        try:
            remote(self.host.ssh, command, timeout=30)
        except subprocess.TimeoutExpired:
            pass


def _free_local_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _listener_pids_script(port: int) -> str:
    return f"""
pids=""
if command -v lsof >/dev/null 2>&1; then
  pids="$(lsof -ti tcp:{port} -sTCP:LISTEN 2>/dev/null)"
elif command -v ss >/dev/null 2>&1; then
  pids="$(ss -H -ltnp 'sport = :{port}' 2>/dev/null | sed -n 's/.*pid=\\([0-9][0-9]*\\).*/\\1/p' | sort -u)"
elif command -v fuser >/dev/null 2>&1; then
  pids="$(fuser {port}/tcp 2>/dev/null)"
fi
printf '%s\\n' "$pids"
"""
