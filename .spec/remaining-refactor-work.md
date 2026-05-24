# Remaining Refactor Work

This file is the standalone handoff for the remaining aw-gateway cleanup work.
It is intentionally self-contained. Do not depend on any external planning or
review files to understand the work.

## Global Rules

- Work in this repository directory. Do not create new worktrees.
- Use a local integration branch named `integration/remaining-refactors`.
- Do not push unless the user explicitly asks.
- Do not open PRs unless the user explicitly asks.
- For each numbered item:
  1. Start from `integration/remaining-refactors`.
  2. Create a local topic branch with the item name, for example
     `refactor/toml-fixture-cleanup`.
  3. Implement the item on that topic branch.
  4. Run the focused checks for the item and the standard non-smoke gate.
  5. Commit the item.
  6. Switch back to `integration/remaining-refactors`.
  7. Merge the topic branch locally into `integration/remaining-refactors`.
  8. Start the next item from the updated integration branch.
- Keep each item behavior-preserving unless that item explicitly says to
  stop for a behavior decision.
- Do not add compatibility shims, alias fields, fallback parsers, bridge
  routes, dual-shape readers, or hidden migration layers.
- Do not run smoke/e2e tests unless the user explicitly asks.
- Do not edit released changelog sections. For this local integration flow,
  skip changelog entries unless an item changes user-facing behavior or the
  user explicitly asks for changelog updates.
- Run the standard non-smoke gate before handoff:
  - `cargo fmt`
  - `cargo test`
  - `cargo clippy --all-targets --all-features`
  - `cargo run --bin aw-gateway -- --config aw-gateway.sample.toml config validate`
  - `cargo run --bin aw-container-agent -- --config container-agent.sample.toml config validate`
- For branch-sized mechanical moves, a lightweight review is acceptable:
  inspect the diff, run focused tests for touched behavior, then run the full
  gate before merging into the integration branch.
- For semantic items, stop and ask for approval before changing behavior.

## Preferred Order

1. `refactor/toml-fixture-cleanup`
2. `refactor/hidden-policy-naming`
3. `refactor/detach-contract`
4. `refactor/runtime-lifecycle-slimming`
5. `refactor/runtime-backend-parsing`
6. `split/gateway-http`
7. `split/gateway-modules`

This order front-loads low-risk test/policy cleanup, then clarifies detach
semantics before touching runtime/gateway lifecycle code, then handles localized
runtime/HTTP splits before the broad `src/gateway.rs` module work.

---

## 1. `refactor/toml-fixture-cleanup`

### Goal

Make TOML fixture mutation less fragile in tests and smoke/deploy harness code
without changing production config behavior.

### Current Problem

The sample gateway config is used as documentation, as the `config init` source,
and as a test fixture. Some tests and smoke deployment code mutate TOML by
string replacement. That is easy to break when sample formatting changes.

Current examples to inspect:

- `tests/cli/helpers.rs`
  - `sample_gateway_config()`
  - `replace_required()`
- CLI tests under `tests/cli/` that call those helpers.
- `smoke/awsmoke/deploy.py`
  - config rendering around `render_gateway_config`
  - string replacements for install roots, runtime program, target mode,
    ephemeral names, `remove_on_stop`, `[client_config]`, HTTP auth token, and
    limited HTTP action lists.

### Implementation Target

Start with Rust tests. Replace brittle sample string surgery with one of:

- a small structural helper that parses TOML into `toml::Value`, mutates the
  intended field, and serializes it back; or
- smaller targeted inline fixtures where using the full sample is unnecessary.

Keep `sample_gateway_config()` available where tests genuinely need the
canonical sample. If `replace_required()` remains, make it fail loudly and
minimize its use.

For the smoke harness, inspect whether a small parser-backed mutation helper is
practical without pulling in a new dependency. If it is small, replace the
current line/string scanning with a typed or parser-backed helper. If it is not
small, leave smoke harness changes out of this branch and write a short note in
the item handoff notes that smoke TOML cleanup remains a separate harness task.

### In Scope

- Test helper cleanup in `tests/cli/helpers.rs`.
- Test fixture call-site updates in `tests/cli/*.rs`.
- Optional smoke harness TOML rendering cleanup in `smoke/awsmoke/deploy.py`
  only if the change stays focused and low risk.
- Focused tests for affected CLI config behavior.

### Out Of Scope

- Production config schema changes.
- Changes to sample config content except where tests explicitly assert exact
  emitted output and the change is intentional.
- Full smoke/e2e runs.
- Broad test framework rewrite.

### Verification

Run focused tests first:

- `cargo test --test cli`
- `cargo test --test example_configs`

Then run the standard full non-smoke gate.

### Stop Conditions

Stop and ask if parser-backed smoke TOML rendering requires a new Python
dependency or becomes larger than the Rust test cleanup.

---

## 2. `refactor/hidden-policy-naming`

### Goal

Name and test-pin several currently implicit policy decisions without changing
behavior.

### Policies To Inspect

1. Bootstrap passwd/group collision behavior:
   - `src/bootstrap.rs`
   - User UID collision is rejected.
   - Group GID collision with a different group name appears to be tolerated.
   - Make this policy explicit through naming, a short comment, and tests.

2. Gateway session shell environment:
   - `src/gateway.rs`
   - Runtime command environments include `SHELL=/usr/bin/bash`.
   - Name this policy near the environment construction. Do not change the
     shell value without approval.

3. Process exits inside async/container-agent flows:
   - `src/agent/idle.rs`
   - `src/agent/control.rs`
   - These exits are PID-1/service-control policy, not arbitrary library exits.
   - Name the helper or add a small local function/comment so the policy is
     visible.

4. Status label fallback:
   - `src/gateway.rs`
   - Missing or inferred status labels render as `"unknown"`.
   - Name the fallback policy and keep output unchanged.

### Implementation Target

Default to behavior-preserving names, helper constants, and focused tests.
Good changes look like:

- `const DEFAULT_SESSION_SHELL_ENV: &str = "/usr/bin/bash";`
- `const UNKNOWN_STATUS_LABEL: &str = "unknown";`
- a helper like `group_gid_collision_policy_allows_existing_gid(...)` if it
  makes the bootstrap decision readable.
- tests that assert the current behavior.

### In Scope

- Small helper names/constants/comments.
- Focused tests for UID/GID collision behavior and status fallback if not
  already pinned.
- No output/schema/API changes.

### Out Of Scope

- Changing UID/GID collision behavior.
- Changing shell selection.
- Replacing `std::process::exit` with a larger shutdown architecture.
- Changing status labels from `"unknown"` to any other string.

### Verification

Run focused tests for touched areas:

- Bootstrap tests in `src/bootstrap.rs`.
- Gateway status tests in `src/gateway.rs`.
- Agent control/idle tests if touched.

Then run the standard full non-smoke gate.

### Stop Conditions

Stop and ask before changing any of the four policies. This branch should name
and pin current behavior, not redesign it.

---

## 3. `refactor/detach-contract`

### Goal

Clarify the detached operation contract now that operation execution has been
centralized. The branch should either make the current fire-and-forget contract
explicit or stop for approval if a status registry is warranted.

### Current Behavior To Inspect

- Detached run and launch paths in `src/gateway.rs`.
- `OperationRunner` / operation execution types.
- `ExecutionOutcome::Detached { operation_id }` in `src/gateway/ops.rs`.
- HTTP response projection in `src/gateway/http.rs`.
- Detached background failure logging.
- Session marker and agent hold lifetime tests.

Current user-visible behavior to preserve unless approved otherwise:

- Detached operations return an `operation_id`.
- Background failures are logged, not exposed through a queryable operation
  status API.
- HTTP detach response shape and status code stay unchanged.
- Session marker cleanup behavior stays unchanged.

### Implementation Target

First make a written decision in the item notes or code comments:

- Current contract is explicitly fire-and-forget; `operation_id` is a log/
  correlation handle only.

If that contract is acceptable, implement small behavior-preserving cleanup:

- Rename helpers or add comments so `operation_id` is not mistaken for a
  queryable registry key.
- Add focused tests that pin detached HTTP response shape and marker cleanup.
- Keep background failure logging behavior unchanged.

If inspection shows a real need for a queryable operation registry, stop and ask
for approval before implementing it. A registry is a new API/behavior surface.

### In Scope

- Naming/comments/tests around detach contract.
- Small internal helper extraction if it reduces duplicate detach handling.

### Out Of Scope

- New operation status endpoint.
- New persistent operation registry.
- HTTP response schema changes.
- CLI/SSH output changes.
- Session marker behavior changes.
- Runtime lifecycle changes unrelated to detach.

### Verification

Focused tests:

- Detached run marker cleanup.
- Detached launch marker cleanup.
- HTTP run/launch detach response shape.
- Background failure logging tests if existing test infrastructure supports it.

Then run the standard full non-smoke gate.

### Stop Conditions

Stop if the work requires changing the meaning of `operation_id` or adding a
new status query surface.

---

## 4. `refactor/runtime-lifecycle-slimming`

### Goal

Reduce remaining lifecycle/start/readiness complexity in gateway runtime code
without changing start/reuse/create, rollback, lifecycle hook, host-step, agent
readiness, status, or session behavior.

### Current Area To Inspect

- `src/gateway.rs`
  - `Runtime`
  - `Runtime::from_config`
  - readiness/start/reuse/create logic
  - lifecycle hook execution
  - host step execution
  - bootstrap config rendering
  - cleanup/rollback paths
- Existing runtime identity/path groupings.
- Existing OperationRunner integration.

### Implementation Target

Choose one narrow slice only. Preferred target:

- Extract a readiness/start coordinator or plan object if it removes real state
  and branch complexity from `Runtime::ensure_ready`.

Good candidates:

- Move `started_container` / `attempted_container_start` style flags into a
  small plan/coordinator if those flags still exist.
- Make rollback ownership explicit.
- Separate readiness decision data from side-effect execution.

Bad candidates:

- Moving code into a new file without reducing state/branch complexity.
- Mixing runtime backend command construction into this item.
- Changing lifecycle hook order.

### In Scope

- Internal readiness/start coordinator.
- Tests that pin readiness/reuse/create/rollback behavior.
- Minimal import/module changes.

### Out Of Scope

- Docker/Podman command construction.
- Detach API/registry decisions.
- Gateway HTTP route changes.
- Config refactors.
- Broad `src/gateway.rs` decomposition.

### Verification

Focused tests should include current gateway lifecycle/readiness coverage:

- readiness plan tests
- create vs reuse tests
- lifecycle hook ordering tests
- host step ordering tests
- bootstrap run-spec tests
- session marker tests if touched

Then run the standard full non-smoke gate.

### Stop Conditions

Stop if a behavior change is needed to simplify rollback or readiness. This
item is about clearer ownership, not a new lifecycle model.

---

## 5. `refactor/runtime-backend-parsing`

### Goal

Extract a narrow, behavior-preserving runtime parsing/formatting boundary from
`src/runtime.rs`.

### Current Area To Inspect

- `src/runtime.rs`
  - runtime backend command construction
  - Docker/Podman/Colima divergence
  - container JSON parsing
  - label parsing
  - mount formatting
  - environment formatting
  - run argument construction

### Implementation Target

Prefer one of these small slices:

1. Extract container JSON parsing and managed-container label parsing into a
   private `runtime/parse.rs` or `runtime/inspect.rs`.
2. Extract label constants/parsing helpers into a private `runtime/labels.rs`.
3. Add a small typed run-argument builder only if it reduces existing
   conditional complexity while keeping Docker/Podman divergence explicit.

Do not do all three if the diff gets large. Pick the smallest boundary that
removes real parsing/formatting clutter.

### In Scope

- Internal parser/helper modules.
- Tests for parser and label behavior.
- No command argument changes.

### Out Of Scope

- Docker/Podman/Colima command semantics.
- Mount/env behavior changes.
- Gateway `Runtime` lifecycle refactor.
- Runtime startup/readiness changes.

### Verification

Focused tests:

- runtime JSON parsing tests
- label parsing tests
- command construction tests for touched code

Then run the standard full non-smoke gate.

### Stop Conditions

Stop if the cleanup requires changing command arguments, accepted JSON shapes,
label names, or error messages.

---

## 6. `split/gateway-http`

### Goal

Split the overlarge `src/gateway/http.rs` into focused private HTTP support
modules while preserving the public HTTP API exactly.

### Current Area To Inspect

- `src/gateway/http.rs` is still over 1,500 lines.
- Candidate seams:
  - router/app construction
  - auth and authorization
  - request parsing
  - response projection
  - error projection
  - route handlers
  - tests

### Implementation Target

Choose a mechanical split that keeps routes and behavior unchanged. A good
module layout would be:

- `src/gateway/http.rs` as the public module entry/router glue.
- `src/gateway/http/auth.rs` for bearer token/auth/action authorization.
- `src/gateway/http/request.rs` for request DTOs and mode/output/launch-var
  parsing.
- `src/gateway/http/response.rs` for success/error response projection.
- `src/gateway/http/handlers.rs` for route handlers if it reduces size without
  circular imports.

If nested `src/gateway/http/` modules require converting the existing
`src/gateway/http.rs` file into `src/gateway/http/mod.rs`, keep the move
mechanical and preserve public imports.

### In Scope

- File/module split.
- Visibility/import cleanup.
- Moving tests only if it helps ownership.

### Out Of Scope

- HTTP route path changes.
- JSON request/response shape changes.
- Status code changes.
- Error code/message changes.
- Auth behavior changes.
- Operation error redesign.
- Gateway operation runner changes.

### Verification

Focused tests:

- HTTP route tests.
- HTTP auth tests.
- HTTP error projection tests.
- HTTP wait/detach tests.
- Launch variable HTTP tests.

Then run the standard full non-smoke gate.

### Stop Conditions

Stop if splitting requires changing route construction behavior or public
response schemas.

---

## 7. `split/gateway-modules`

### Goal

Start decomposing the still-large `src/gateway.rs` after the smaller
boundaries have landed. This item should choose one cohesive slice and keep it
reviewable.

### Current Area To Inspect

`src/gateway.rs` still owns too much:

- CLI dispatch and rendering.
- SSH gateway action dispatch.
- launch parsing, display, variable resolution, and execution.
- run/connect/up/stop/remove/status behavior.
- Runtime construction and execution.
- lifecycle orchestration.
- agent control calls.
- socket path rendering and cleanup.
- workspace cleanup safety.
- status-all label rendering.
- large inline tests.

Some support modules already exist under `src/gateway/`, including operation,
client, identity, session, HTTP, file/health/listener support. Reorient from
the current `integration/remaining-refactors` branch before deciding the slice.

### Preferred First Slice

Pick the smallest slice that materially reduces `src/gateway.rs` and has a
clear boundary. Preferred order:

1. Rendering/status projection helpers:
   - text/JSON printing
   - `*_text` helpers
   - status-all label rendering
   - no operation behavior changes

2. Launch metadata and variable resolution helpers:
   - launch display/metadata
   - launch var resolution if still in `gateway.rs`
   - launch step execution only if it stays cohesive

3. Workspace/socket path cleanup helpers:
   - path rendering
   - control socket cleanup
   - workspace cleanup safety

Avoid lifecycle/start/readiness work here if `runtime-lifecycle-slimming` has
not landed yet.

### In Scope

- One cohesive gateway module split.
- Mechanical moves plus visibility/import cleanup.
- Focused tests for moved behavior.

### Out Of Scope

- Splitting every gateway concern in one item.
- Runtime backend command construction.
- OperationRunner redesign.
- Detach contract changes.
- HTTP route/schema changes.
- Config/schema changes.

### Verification

Focused tests depend on the chosen slice:

- rendering/status tests for render/status split
- launch var/metadata tests for launch split
- workspace/path/session cleanup tests for workspace/path split

Then run the standard full non-smoke gate.

### Stop Conditions

Stop if the slice crosses behavior boundaries or requires changing CLI, SSH,
HTTP, config, status JSON, session marker, operation ID, detach, or error
mapping behavior.

---

## Final Completion Criteria

The remaining cleanup is complete when:

- All seven items above have either landed or been explicitly deferred with a
  reason.
- `src/config.rs` and `src/gateway.rs` are materially smaller and have clearer
  ownership boundaries.
- Runtime and detach semantics are named and test-pinned.
- TOML fixture mutation is not silently brittle in the common Rust test path.
- HTTP and runtime support files have localized parsing/projection helpers where
  it reduced real complexity.
- The full non-smoke gate passes on the final `integration/remaining-refactors`
  branch.
