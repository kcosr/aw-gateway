# Proxy And CA Policy Guide

This guide describes a generic pattern for running a container-local or
host-reachable egress proxy with `aw-gateway`. It covers where to mount proxy
assets, how to install trust, how to start the proxy as a supervised service,
and how to pair it with firewall redirects.

It does not prescribe one proxy implementation. At the end of the guide,
`acl-proxy` is listed as one option.

## Common Models

| Model | How Clients Use It | Best For |
|---|---|---|
| Explicit proxy env | `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` | Simple local policy where tools respect proxy env vars. |
| Transparent proxy | Firewall redirects TCP 80/443 to the proxy | Enforcing policy for tools that do not honor proxy env vars. |
| Host or VM proxy | Container traffic exits to a host/VM proxy endpoint | Centralized policy shared by many containers. |
| Container-local proxy | Proxy runs inside each managed container | Per-container policy and per-container logs. |

Transparent TLS interception requires a trusted CA certificate inside the
container and often app-specific trust settings for runtimes such as Node.js.

## Stage Proxy Assets

Keep proxy binaries, config, and CA material outside the user-writable
workspace. Mount them read-only into the container:

```toml
[[container_mounts]]
source = "/opt/site-policy/proxy/bin/site-proxy"
target = "/opt/site-policy/bin/site-proxy"
mode = "ro"

[[container_mounts]]
source = "/opt/site-policy/proxy/etc"
target = "/etc/site-proxy"
mode = "ro"

[[container_mounts]]
source = "/opt/site-policy/proxy/certs/site-proxy-ca.crt"
target = "/etc/site-proxy/certs/site-proxy-ca.crt"
mode = "ro"
```

Use the same idea for Docker or Colima, but choose host paths that exist on the
machine running `aw-gateway`.

## Install CA Trust

If the proxy intercepts TLS, install the CA during container bootstrap. Examples
for common image families:

Debian or Ubuntu:

```toml
[[container_bootstrap_steps]]
name = "install-proxy-ca"
required = true
user = "root"
command = [
  "/bin/sh", "-lc",
  "cp /etc/site-proxy/certs/site-proxy-ca.crt /usr/local/share/ca-certificates/site-proxy-ca.crt && update-ca-certificates"
]
```

RHEL, Rocky, or Fedora:

```toml
[[container_bootstrap_steps]]
name = "install-proxy-ca"
required = true
user = "root"
command = [
  "/bin/sh", "-lc",
  "cp /etc/site-proxy/certs/site-proxy-ca.crt /etc/pki/ca-trust/source/anchors/site-proxy-ca.crt && update-ca-trust"
]
```

Alpine:

```toml
[[container_bootstrap_steps]]
name = "install-proxy-ca"
required = true
user = "root"
command = [
  "/bin/sh", "-lc",
  "cp /etc/site-proxy/certs/site-proxy-ca.crt /usr/local/share/ca-certificates/site-proxy-ca.crt && update-ca-certificates"
]
```

Prefer system trust when possible. Some tools still need explicit environment
variables; use target `session_env` for user sessions. The bundle paths below
are common examples; adjust them for the image family. `aw-gateway` does not set
proxy CA environment variables by default because incorrect CA paths produce
client warnings or failures in non-proxy deployments.

```toml
[targets.default.session_env]
NODE_EXTRA_CA_CERTS = "/etc/site-proxy/certs/site-proxy-ca.crt"
REQUESTS_CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
GIT_SSL_CAINFO = "/etc/ssl/certs/ca-certificates.crt"
```

Use only the variables your image and tools need. Incorrect CA paths can break
otherwise-working clients.

## Supervise A Container-Local Proxy

Run the proxy as an `aw-container-agent` service when it should live inside the
managed container:

```toml
[[container_agent.services]]
name = "egress-proxy"
required = true
user = "root"
command = [
  "/opt/site-policy/bin/site-proxy",
  "--config",
  "/etc/site-proxy/proxy.toml"
]
restart = "always"

[container_agent.services.health_check]
type = "tcp"
host = "127.0.0.1"
port = 8881
interval = "2s"
timeout = "1s"
```

If SSH depends on the proxy for downloads or extension installation, add the
dependency to the existing SSHD service:

```toml
[[container_agent.services]]
name = "container-sshd"
command = ["/opt/aw-gateway/bin/start-container-sshd"]
depends_on = ["egress-proxy"]
```

## Explicit Proxy Environment

For tools that honor proxy variables, set session env:

```toml
[targets.default.session_env]
HTTP_PROXY = "http://127.0.0.1:8881"
HTTPS_PROXY = "http://127.0.0.1:8881"
NO_PROXY = "127.0.0.1,localhost"
```

If the long-lived bootstrap or agent process also needs these variables, put
them in `container_env` instead or in addition:

```toml
[targets.default.container_env]
HTTP_PROXY = "http://127.0.0.1:8881"
HTTPS_PROXY = "http://127.0.0.1:8881"
NO_PROXY = "127.0.0.1,localhost"
```

Keep the distinction clear: `container_env` is for container startup;
`session_env` is for gateway exec paths and generated SSHD session environment.

## Transparent Proxy Firewall

Transparent proxying needs firewall rules in the container network namespace,
the host, or the Colima VM. The firewall should redirect client traffic to the
proxy and allow the proxy process itself to reach upstream destinations without
being redirected back into itself.

Wire the firewall with a host step:

```toml
[[host_steps]]
name = "transparent-proxy-firewall"
required = true
command = ["/opt/site-policy/bin/proxy-firewall", "add", "{container_pid}"]

[host_steps.health_check]
type = "command"
command = ["/opt/site-policy/bin/proxy-firewall", "check", "{container_pid}"]
```

See [Firewall Policy](firewall.md) for namespace and Colima placement details.

## Validation

Validate the layers independently:

```bash
# Proxy process is listening.
aw-gateway --config /etc/aw-gateway/gateway.toml run default -- ss -ltn

# CA trust works for an intercepted HTTPS site.
aw-gateway --config /etc/aw-gateway/gateway.toml run default -- curl -I https://example.com

# Direct egress is blocked or redirected according to the firewall policy.
aw-gateway --config /etc/aw-gateway/gateway.toml run default -- curl -I https://blocked.invalid
```

For VS Code or other Node-based tools, also validate from an SSH session so
`session_env` and SSHD `SetEnv` handling are exercised.

## Implementation Option

One possible proxy implementation is
[acl-proxy](https://github.com/kcosr/acl-proxy). `aw-gateway` does not require
that project; any proxy with a stable command, config file, health check, CA
bundle, and firewall policy can use the same integration pattern.
