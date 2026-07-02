#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'USAGE'
Capture Apple container CLI fixtures for aw-gateway development.

Run this on an Apple silicon Mac with macOS 26+ and Apple `container` installed.
The script captures JSON output, help text, stderr for bind conflicts, read-only
mount behavior, and stop/start published-port behavior into a tar.gz archive.

Usage:
  scripts/capture-apple-container-fixtures.sh [output-dir]

Environment:
  CONTAINER_CLI                       Runtime executable, default: container
  APPLE_CONTAINER_FIXTURE_IMAGE        Test image, default: alpine:latest
  APPLE_CONTAINER_FIXTURE_PORT         Published-port reuse test port, default: 49223
  APPLE_CONTAINER_FIXTURE_CONFLICT_PORT Bind-conflict test port, default: 49222
  APPLE_CONTAINER_FIXTURE_ALLOW_UNSUPPORTED_HOST
                                      Set to 1 only for script debugging off Mac
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
	usage
	exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT_DIR="${1:-"$ROOT/target/apple-container-fixtures/$TIMESTAMP"}"
OUT_PARENT="$(dirname "$OUT_DIR")"
OUT_BASE="$(basename "$OUT_DIR")"
CONTAINER_CLI="${CONTAINER_CLI:-container}"
IMAGE="${APPLE_CONTAINER_FIXTURE_IMAGE:-alpine:latest}"
REUSE_PORT="${APPLE_CONTAINER_FIXTURE_PORT:-49223}"
CONFLICT_PORT="${APPLE_CONTAINER_FIXTURE_CONFLICT_PORT:-49222}"
USER_NAME="$(id -un 2>/dev/null || printf user)"
PREFIX="awgw-${TIMESTAMP}-$$"
RUNNING_NAME="${PREFIX}-running"
CONFLICT_NAME="${PREFIX}-conflict"
REUSE_NAME="${PREFIX}-reuse"
LISTENER_PID=""
LAST_STATUS=0
FAILURES=0
READONLY_VERIFIED=0
CONTAINERS=()

mkdir -p "$OUT_DIR"

log() {
	printf '%s\n' "$*" | tee -a "$OUT_DIR/run.log"
}

quote_command() {
	for arg in "$@"; do
		printf '%q ' "$arg"
	done
	printf '\n'
}

capture() {
	local name="$1"
	shift
	local dir="$OUT_DIR/$name"
	mkdir -p "$dir"
	quote_command "$@" >"$dir/command.txt"
	set +e
	"$@" >"$dir/stdout.txt" 2>"$dir/stderr.txt"
	LAST_STATUS=$?
	set -e
	printf '%s\n' "$LAST_STATUS" >"$dir/exit-code.txt"
	log "$name exit=$LAST_STATUS"
	return 0
}

capture_shell() {
	local name="$1"
	local script="$2"
	local dir="$OUT_DIR/$name"
	mkdir -p "$dir"
	printf '%s\n' "$script" >"$dir/command.txt"
	set +e
	/bin/bash -lc "$script" >"$dir/stdout.txt" 2>"$dir/stderr.txt"
	LAST_STATUS=$?
	set -e
	printf '%s\n' "$LAST_STATUS" >"$dir/exit-code.txt"
	log "$name exit=$LAST_STATUS"
	return 0
}

require_success() {
	local name="$1"
	if [[ "$LAST_STATUS" -ne 0 ]]; then
		log "required step failed: $name"
		FAILURES=$((FAILURES + 1))
	fi
}

require_stdout() {
	local name="$1"
	if [[ ! -s "$OUT_DIR/$name/stdout.txt" ]]; then
		log "required stdout was empty: $name"
		FAILURES=$((FAILURES + 1))
	fi
}

capture_required() {
	local name="$1"
	shift
	capture "$name" "$@"
	require_success "$name"
}

capture_required_stdout() {
	local name="$1"
	shift
	capture "$name" "$@"
	require_success "$name"
	require_stdout "$name"
}

require_failure_with_stderr() {
	local name="$1"
	if [[ "$LAST_STATUS" -eq 0 ]]; then
		log "expected command to fail, but it succeeded: $name"
		FAILURES=$((FAILURES + 1))
	fi
	if [[ ! -s "$OUT_DIR/$name/stderr.txt" ]]; then
		log "expected stderr was empty: $name"
		FAILURES=$((FAILURES + 1))
	fi
}

remember_container() {
	CONTAINERS+=("$1")
}

cleanup() {
	if [[ -n "$LISTENER_PID" ]]; then
		kill "$LISTENER_PID" >/dev/null 2>&1 || true
		wait "$LISTENER_PID" >/dev/null 2>&1 || true
	fi
	for container_name in "${CONTAINERS[@]}"; do
		"$CONTAINER_CLI" delete --force "$container_name" >/dev/null 2>&1 || true
	done
}
trap cleanup EXIT

port_in_use() {
	local port="$1"
	lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1
}

start_conflict_listener() {
	local port="$1"
	if port_in_use "$port"; then
		log "port $port is already in use; choose another APPLE_CONTAINER_FIXTURE_CONFLICT_PORT"
		return 1
	fi
	(
		while true; do
			nc -l 127.0.0.1 "$port" >/dev/null 2>&1
		done
	) &
	LISTENER_PID=$!
	sleep 1
	if ! port_in_use "$port"; then
		log "failed to start listener on 127.0.0.1:$port"
		return 1
	fi
	return 0
}

write_metadata() {
	{
		printf 'timestamp=%s\n' "$TIMESTAMP"
		printf 'root=%s\n' "$ROOT"
		printf 'container_cli=%s\n' "$CONTAINER_CLI"
		printf 'image=%s\n' "$IMAGE"
		printf 'reuse_port=%s\n' "$REUSE_PORT"
		printf 'conflict_port=%s\n' "$CONFLICT_PORT"
		printf 'prefix=%s\n' "$PREFIX"
		printf 'user=%s\n' "$USER_NAME"
		printf 'uid=%s\n' "$(id -u)"
		printf 'gid=%s\n' "$(id -g)"
		printf 'uname_s=%s\n' "$(uname -s)"
		printf 'uname_m=%s\n' "$(uname -m)"
		if command -v sw_vers >/dev/null 2>&1; then
			sw_vers | sed 's/^/sw_vers_/'
		fi
	} >"$OUT_DIR/metadata.txt"
}

run_readonly_probe() {
	local name="$1"
	shift
	local source_dir="$OUT_DIR/$name-source"
	local container_name="${PREFIX}-${name}"
	mkdir -p "$source_dir"
	printf 'original\n' >"$source_dir/probe.txt"
	remember_container "$container_name"
	capture "$name" "$CONTAINER_CLI" run --rm --name "$container_name" "$@" "$IMAGE" sh -c \
		'cat /mnt/ro/probe.txt >/tmp/probe-copy; if echo changed >>/mnt/ro/probe.txt; then exit 42; else exit 0; fi'
	local probe_status="$LAST_STATUS"
	{
		printf 'exit_code=%s\n' "$probe_status"
		case "$probe_status" in
			0)
				printf 'result=verified_readonly\n'
				READONLY_VERIFIED=1
				;;
			42)
				printf 'result=writable_failure\n'
				log "read-only mount was writable: $name"
				FAILURES=$((FAILURES + 1))
				;;
			*)
				printf 'result=unsupported_or_runtime_failure\n'
				;;
		esac
	} >"$OUT_DIR/$name/observation.txt"
	capture_shell "${name}_host_file_after" "cat '$source_dir/probe.txt'; ls -l '$source_dir/probe.txt'"
}

log "writing fixtures to $OUT_DIR"
write_metadata

if [[ "${APPLE_CONTAINER_FIXTURE_ALLOW_UNSUPPORTED_HOST:-0}" != "1" ]]; then
	if [[ "$(uname -s)" != "Darwin" ]]; then
		log "unsupported host: this fixture capture must run on macOS"
		exit 1
	fi
	if [[ "$(uname -m)" != "arm64" ]]; then
		log "unsupported host: Apple container requires Apple silicon; uname -m is $(uname -m)"
		exit 1
	fi
fi
if ! command -v "$CONTAINER_CLI" >/dev/null 2>&1; then
	log "missing runtime executable: $CONTAINER_CLI"
	exit 1
fi

capture_required_stdout system_version_json "$CONTAINER_CLI" system version --format json
capture_required_stdout system_status_json "$CONTAINER_CLI" system status --format json

capture_required_stdout help_run "$CONTAINER_CLI" run --help
capture_required_stdout help_exec "$CONTAINER_CLI" exec --help
capture_required_stdout help_list "$CONTAINER_CLI" list --help
capture_required_stdout help_inspect "$CONTAINER_CLI" inspect --help
capture_required_stdout help_start "$CONTAINER_CLI" start --help
capture_required_stdout help_stop "$CONTAINER_CLI" stop --help
capture_required_stdout help_delete "$CONTAINER_CLI" delete --help
capture_required_stdout help_system_version "$CONTAINER_CLI" system version --help
capture_required_stdout help_system_status "$CONTAINER_CLI" system status --help

capture_required image_pull "$CONTAINER_CLI" image pull "$IMAGE"

capture_required_stdout list_initial_json "$CONTAINER_CLI" list --all --format json

RUN_ENV_INHERIT_VALUE="aw-gateway-run-env-$TIMESTAMP"
capture_required_stdout run_env_inherit env "AWGW_FIXTURE_RUN_INHERIT=$RUN_ENV_INHERIT_VALUE" \
	"$CONTAINER_CLI" run --rm --env AWGW_FIXTURE_RUN_INHERIT "$IMAGE" sh -c \
	'printf "%s\n" "$AWGW_FIXTURE_RUN_INHERIT"'
if [[ "$(tr -d '\r' <"$OUT_DIR/run_env_inherit/stdout.txt")" != "$RUN_ENV_INHERIT_VALUE" ]]; then
	log "container run did not inherit bare --env key from host environment"
	FAILURES=$((FAILURES + 1))
fi

remember_container "$RUNNING_NAME"
capture_required run_labeled_detached "$CONTAINER_CLI" run --detach --init \
	--name "$RUNNING_NAME" \
	--label io.aw-gateway.gateway=true \
	--label "io.aw-gateway.user=$USER_NAME" \
	--label "io.aw-gateway.uid=$(id -u)" \
	--label io.aw-gateway.mode=fixture \
	--label io.aw-gateway.target=apple-fixture \
	"$IMAGE" sleep 3600
capture_required_stdout inspect_labeled_running_json "$CONTAINER_CLI" inspect "$RUNNING_NAME"
capture_required_stdout list_with_labeled_running_json "$CONTAINER_CLI" list --all --format json
capture_required_stdout exec_env_explicit "$CONTAINER_CLI" exec \
	--env AWGW_FIXTURE_EXEC_EXPLICIT=exec-fixture \
	"$RUNNING_NAME" sh -c 'printf "%s\n" "$AWGW_FIXTURE_EXEC_EXPLICIT"'
EXEC_ENV_INHERIT_VALUE="aw-gateway-exec-env-$TIMESTAMP"
capture_required_stdout exec_env_inherit env "AWGW_FIXTURE_EXEC_INHERIT=$EXEC_ENV_INHERIT_VALUE" \
	"$CONTAINER_CLI" exec --env AWGW_FIXTURE_EXEC_INHERIT \
	"$RUNNING_NAME" sh -c 'printf "%s\n" "$AWGW_FIXTURE_EXEC_INHERIT"'
if [[ "$(tr -d '\r' <"$OUT_DIR/exec_env_inherit/stdout.txt")" != "$EXEC_ENV_INHERIT_VALUE" ]]; then
	log "container exec did not inherit bare --env key from host environment"
	FAILURES=$((FAILURES + 1))
fi
capture_required stop_labeled "$CONTAINER_CLI" stop "$RUNNING_NAME"
capture_required_stdout inspect_labeled_stopped_json "$CONTAINER_CLI" inspect "$RUNNING_NAME"
capture_required delete_labeled "$CONTAINER_CLI" delete --force "$RUNNING_NAME"

run_readonly_probe readonly_volume_colon_ro --volume "$OUT_DIR/readonly_volume_colon_ro-source:/mnt/ro:ro"
run_readonly_probe readonly_mount_flag --mount "type=bind,source=$OUT_DIR/readonly_mount_flag-source,target=/mnt/ro,readonly"
if [[ "$READONLY_VERIFIED" -ne 1 ]]; then
	log "no read-only mount syntax was verified"
	FAILURES=$((FAILURES + 1))
fi

if port_in_use "$REUSE_PORT"; then
	log "port $REUSE_PORT is already in use; skipping stop/start published-port fixture"
	FAILURES=$((FAILURES + 1))
else
	remember_container "$REUSE_NAME"
	capture_required run_reuse_published "$CONTAINER_CLI" run --detach --init \
		--name "$REUSE_NAME" \
		--label io.aw-gateway.gateway=true \
		--label "io.aw-gateway.user=$USER_NAME" \
		--label "io.aw-gateway.uid=$(id -u)" \
		--label io.aw-gateway.mode=fixture \
		--publish "127.0.0.1:$REUSE_PORT:22/tcp" \
		"$IMAGE" sleep 3600
	capture_shell reuse_lsof_after_run "lsof -nP -iTCP:$REUSE_PORT -sTCP:LISTEN"
	local_after_run="$LAST_STATUS"
	capture_required_stdout inspect_reuse_running_json "$CONTAINER_CLI" inspect "$REUSE_NAME"
	capture_required stop_reuse "$CONTAINER_CLI" stop "$REUSE_NAME"
	capture_shell reuse_lsof_after_stop "lsof -nP -iTCP:$REUSE_PORT -sTCP:LISTEN"
	local_after_stop="$LAST_STATUS"
	capture_required start_reuse "$CONTAINER_CLI" start "$REUSE_NAME"
	capture_shell reuse_lsof_after_start "lsof -nP -iTCP:$REUSE_PORT -sTCP:LISTEN"
	local_after_start="$LAST_STATUS"
	{
		printf 'after_run_bound=%s\n' "$([[ "$local_after_run" -eq 0 ]] && printf true || printf false)"
		printf 'after_stop_bound=%s\n' "$([[ "$local_after_stop" -eq 0 ]] && printf true || printf false)"
		printf 'after_start_bound=%s\n' "$([[ "$local_after_start" -eq 0 ]] && printf true || printf false)"
		printf 'note=%s\n' "This observes host port binding only; SSH reachability is covered by later smoke tests."
	} >"$OUT_DIR/reuse_port_observation.txt"
	if [[ "$local_after_run" -ne 0 || "$local_after_start" -ne 0 ]]; then
		log "published host port was not bound after run/start"
		FAILURES=$((FAILURES + 1))
	fi
	capture_required_stdout inspect_reuse_restarted_json "$CONTAINER_CLI" inspect "$REUSE_NAME"
	capture_required delete_reuse "$CONTAINER_CLI" delete --force "$REUSE_NAME"
fi

if start_conflict_listener "$CONFLICT_PORT"; then
	capture_shell conflict_lsof_before_run "lsof -nP -iTCP:$CONFLICT_PORT -sTCP:LISTEN || true"
	remember_container "$CONFLICT_NAME"
	capture run_bind_conflict "$CONTAINER_CLI" run --detach --init \
		--name "$CONFLICT_NAME" \
		--label io.aw-gateway.gateway=true \
		--label "io.aw-gateway.user=$USER_NAME" \
		--label "io.aw-gateway.uid=$(id -u)" \
		--label io.aw-gateway.mode=fixture \
		--publish "127.0.0.1:$CONFLICT_PORT:22/tcp" \
		"$IMAGE" sleep 60
	require_failure_with_stderr run_bind_conflict
	capture inspect_after_bind_conflict "$CONTAINER_CLI" inspect "$CONFLICT_NAME"
	if [[ "$LAST_STATUS" -eq 0 ]]; then
		log "leftover container exists after bind conflict; captured inspect for cleanup semantics"
	fi
	capture delete_after_bind_conflict "$CONTAINER_CLI" delete --force "$CONFLICT_NAME"
else
	log "skipping bind-conflict fixture because listener could not be started"
	FAILURES=$((FAILURES + 1))
fi

capture_required_stdout list_final_json "$CONTAINER_CLI" list --all --format json

ARCHIVE="$OUT_DIR.tar.gz"
mkdir -p "$OUT_PARENT"
tar -czf "$ARCHIVE" -C "$OUT_PARENT" "$OUT_BASE"
log "archive written: $ARCHIVE"

if [[ "$FAILURES" -ne 0 ]]; then
	log "completed with $FAILURES required failure(s); archive still contains captured output"
	exit 1
fi

log "completed successfully"
