# Interrupted Foreground Cleanup

Issue: foreground `aw-gateway run` and `aw-gateway launch` can be interrupted by
client teardown, process signals, or PTY closure before they reach the normal
post-session cleanup path.

## Behavior Contract

- Handled cancellation is distinct from command failure.
- A handled cancellation should return an interrupted process exit code to the
  caller, but should use a `Canceled` session outcome for cleanup policy.
- `workspace.cleanup = "success"` should treat `Canceled` as a clean session
  conclusion, while preserving workspaces after real command/setup failures.
- `workspace.cleanup = "always"` should continue to clean after success,
  cancellation, or failure.
- `workspace.cleanup = "never"` should continue to preserve all workspaces.
- Fixed targets must not be removed just because one foreground command was
  interrupted. They follow existing idle cleanup policy.
- Ephemeral targets follow existing configured cleanup behavior.

## Gateway-Owned Cleanup

When `idle_cleanup.owner = "gateway"`, the `aw-gateway` process that observes
the session ending owns one cleanup attempt:

1. Terminate the active foreground runtime exec best-effort.
2. Drop the operation session marker and any agent session hold.
3. Wait the configured `idle_grace`.
4. Acquire the lifecycle lock.
5. Re-check active session markers.
6. Stop/remove only if no active markers remain and policy allows it.
7. Apply workspace cleanup according to `workspace.cleanup`.

If another `aw-gateway` instance starts a session during the grace period, the
marker re-check causes the old cleanup attempt to exit. The later session then
owns the next cleanup edge when it ends.

## Agent-Owned Cleanup

When `idle_cleanup.owner = "agent"`, `aw-gateway` should drop its marker and
agent session hold, then exit. The in-container agent owns its idle grace and
cleanup loop through `active_streams` and `active_sessions`.

## Signals And Teardown

Foreground `run` and `launch` should handle:

- `SIGINT`
- `SIGTERM`
- `SIGHUP`
- runtime exec completion caused by PTY/stdin teardown

`SIGKILL`, host crash, and immediate process death cannot be handled in-process.
Those remain recovery or explicit-remove cases.

## Testing

Add deterministic tests for:

- interrupted foreground `run`
- interrupted foreground `launch`
- `Canceled` cleanup policy semantics
- another active marker appearing during gateway-owned idle grace

Add live smoke coverage for interrupted foreground launch cleanup and run the
new smoke case before the full smoke suite.
