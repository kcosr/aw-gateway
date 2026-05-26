from __future__ import annotations

from http import HTTPStatus

from awsmoke.gateway import gateway
from awsmoke.hosts import Host
from awsmoke.http_api import HttpDaemon


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
