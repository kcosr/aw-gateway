# Ephemeral Remove Design Draft

Temporary design note for adding first-class removal of aw-gateway-managed
ephemeral session containers.

## Problem

Ephemeral targets create one container per session id. Existing commands can
address an ephemeral session for `connect`, `up`, `run`, `launch`, `stop`, and
`status`, but `remove` only accepts a target:

```text
aw-gateway remove [target]
```

That means `remove` cannot address a concrete ephemeral container, because
runtime resolution for an ephemeral target requires a session id. Operators can
see these containers through `status --all`, but stale or stopped ephemeral
containers currently require direct runtime commands such as `podman rm`.

This is a lifecycle-management gap. Gateway-managed containers should be
removable through the gateway so label validation, lifecycle hooks, lock
ordering, control-socket cleanup, and workspace safety checks remain centralized.

## Target CLI, SSH, And HTTP Behavior

Add session-id support to remove:

```text
aw-gateway remove [target] [--session-id ID]
```

Rules:

- Fixed targets continue to reject `--session-id`.
- Ephemeral targets require `--session-id`.
- The session id uses the same validation as `connect`, `up`, `run`, `launch`,
  `stop`, and `status`.
- `remove --session-id` removes only the one resolved session container. It must
  never imply bulk removal of every ephemeral session for a target. If bulk
  cleanup is needed later, add a separate explicit command or flag such as
  `remove --all-sessions target`.
- SSH dispatch should accept equivalent forms if `remove` is enabled:

```text
ssh host 'remove target --session-id abc123def456'
ssh host 'remove --session-id abc123def456 target'
```

The SSH `rm` alias is intentionally not supported. `remove` is the only gateway
removal command name.

HTTP should grow lifecycle stop/remove actions in the same change:

```text
POST /api/v1/stop
POST /api/v1/remove
```

Both HTTP request bodies should accept optional `target` and `session_id`
fields. `stop` and `remove` must be gated by `http.enabled_actions` just like
`up`, `run`, and `launch`; add `stop` and `remove` to the HTTP action registry
only when the routes are implemented. Fixed targets reject `session_id`, and
ephemeral targets require it for both `stop` and `remove`.

## Remove Semantics

For a fixed target, behavior should stay the same:

1. Resolve the target without a session id.
2. Acquire the lifecycle lock.
3. If the container exists and is running, stop it through the normal stop path.
4. Remove the container.
5. Clean the control socket runtime directory.

For an ephemeral target, behavior should mirror the fixed path but resolve the
specific session container:

1. Resolve the target with the supplied session id.
2. Acquire the lifecycle lock.
3. If the container does not exist, clean the control socket runtime directory
   and report no removal, but only after the session id has been validated and
   the derived runtime directory has passed the same path-safety checks used by
   normal socket cleanup. Missing containers must not turn arbitrary session ids
   into a path-deletion primitive.
4. Validate gateway labels before mutating the container.
5. If the container is running, stop it through the normal stop path.
6. Remove the container if it still exists.
7. Clean the control socket runtime directory.
8. Attempt session workspace cleanup as described below.

## Lifecycle Hooks

Manual `stop` and `remove` should keep the existing lifecycle behavior:

- `stop --session-id` on a running ephemeral container fires `pre_stop`, agent
  shutdown, runtime stop, optional `remove_on_stop`, `post_stop`, and control
  socket cleanup.
- `remove --session-id` on a running ephemeral container first uses the same
  stop path, so stop hooks fire before removal.
- `remove --session-id` on an already-stopped ephemeral container does not fire
  stop hooks, because there is no running container PID/context. It validates
  labels, removes the stopped container, and performs cleanup.

Plain `stop --session-id` should not remove the session workspace. Stopping is
reversible and may be used before inspection or restart with the same session
id.

## Workspace Cleanup

Today, session workspace cleanup is driven by post-session completion for
attached operations such as `connect`, `run`, and `launch`. The outcome is:

- `run` and `launch`: success when the in-container command exits with code 0.
- `connect` and local-listen `up`: success when the gateway operation returns
  successfully.
- setup/start/proxy errors: failure.

Manual remove has no attached command outcome. Treat explicit
`remove --session-id` as operator intent to clean up the managed ephemeral
session footprint once the resolved managed container has been removed or is
already absent.

Manual remove workspace policy:

- `workspace.cleanup = "never"`: preserve the workspace.
- `workspace.cleanup = "success"`: delete the workspace on explicit remove.
- `workspace.cleanup = "always"`: delete the workspace on explicit remove.

The cleanup must still use the existing workspace safety checks:

- target mode must be ephemeral;
- workspace cleanup must be configured;
- resolved workspace must include the session id;
- resolved workspace must be under an `aw-gateway` path component;
- resolved workspace must not be empty, `/`, the user's home directory, a path
  with `.`/`..` components, or a symlink;
- missing workspace is success.

## Avoiding Duplicate Cleanup

Manual remove and post-session cleanup can race when an attached session is
still unwinding. Explicit remove should win: once the gateway has removed the
resolved managed container, it may delete the resolved session workspace even if
active markers still exist, because the operator explicitly terminated that
session footprint. This avoids the hole where explicit remove kills an active
session, the session later records a failure outcome, and
`workspace.cleanup = "success"` preserves the workspace despite the explicit
remove request.

Manual remove cleanup rules:

1. Remove the managed container first, or confirm that it is already absent
   after validating the target/session id and path safety.
2. If workspace cleanup is not `never`, apply the existing workspace safety
   checks and delete the resolved session workspace.
3. Do not delete broad target-level directories or any path that fails the
   existing cleanup validators.
4. If post-session cleanup later attempts the same deletion, missing workspace
   remains success.

The lifecycle lock should cover the container mutation and workspace cleanup
decision so manual remove and post-session cleanup have predictable ordering.

## Implementation Sketch

- Add a `RemoveArgs` struct with `target: Option<String>` and
  `session_id: Option<String>`.
- Change `GatewayCommand::Remove` to use `RemoveArgs` instead of `TargetArg`.
- Thread `session_id` through `GatewayOperation::Remove`.
- Update `GatewayOperation::from_remove_args`.
- Update `operation_remove` to call `Runtime::load(..., session_id, false)`.
- Update SSH dispatch parsing for `remove` to accept `--session-id`.
- Add HTTP `stop` and `remove` routes, request parsing, action allow-list
  entries, and operation dispatch.
- Add focused tests for:
  - CLI parsing of `remove --session-id` forms.
  - SSH parsing of `remove --session-id` forms, with no `rm` alias.
  - HTTP `stop` and `remove` routes reject disabled actions and dispatch when
    enabled.
  - fixed target rejects `--session-id`.
  - ephemeral target requires `--session-id`.
  - remove resolves the expected ephemeral container name.
  - running remove fires stop hooks through the existing stop path.
  - stopped remove skips stop hooks but removes the container.
  - workspace cleanup runs for explicit remove when cleanup is not `never`,
    including when active session markers still exist.
  - missing workspace is success.
- Update README command reference and API docs.
