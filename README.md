# doubleshot

[![CI](https://github.com/mrodz/doubleshot/actions/workflows/ci.yml/badge.svg)](https://github.com/mrodz/doubleshot/actions)

`doubleshot` is a generic blue-green deployment helper. It can run as a daemon
that watches an inbox, or it can deploy a local artifact directly. The app
runtime, health check, slots, and traffic switch are configured instead of being
hardcoded to one stack.

## Management

Linux release archives include scripts for updating, installing, and removing a
systemd-managed `doubleshot` daemon. Download and extract the archive for your
platform first, then run the script you need:

```bash
# update the binary while preserving doubleshot.toml and the service unit
./update.sh --version vX.Y.Z

# install the binary, runtime directories, config, and systemd unit
./install.sh --version vX.Y.Z

# uninstall the service, sudoers rule, and binary
./uninstall.sh

# uninstall and remove /opt/doubleshot plus the nginx include directory
./uninstall.sh --purge
```

Set prompt defaults through environment variables for non-interactive installs:

```bash
DOUBLESHOT_HOME=/opt/doubleshot \
DOUBLESHOT_USER=doubleshot \
APP_TYPE=spring-boot \
APP_ENV_FILE=/opt/myapp/prod.env \
JAVA_BIN=/usr/bin/java \
./install.sh --version vX.Y.Z
```

## Installation

**Pre-built binaries** are available on the [releases page](https://github.com/mrodz/doubleshot/releases). Download the archive for your platform, extract it, and place the binary somewhere on your `$PATH`:

| Platform | Archive |
|---|---|
| macOS (Apple Silicon) | `doubleshot-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `doubleshot-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| Linux aarch64 (glibc) | `doubleshot-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 (musl) | `doubleshot-vX.Y.Z-aarch64-unknown-linux-musl.tar.gz` |
| Linux x86_64 (glibc) | `doubleshot-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| Linux x86_64 (musl) | `doubleshot-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |

```bash
# example — adjust version and target triple as needed
curl -LO https://github.com/mrodz/doubleshot/releases/download/vX.Y.Z/doubleshot-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
tar -xzf doubleshot-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
sudo mv doubleshot /usr/local/bin/
```

Linux release archives also include installer scripts for systemd hosts. To
install the binary, create the runtime directories, write a Spring Boot-oriented
config, install the systemd unit, and start the daemon:

```bash
tar -xzf doubleshot-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
./install.sh --version vX.Y.Z
```

The installer prompts with defaults and can be driven non-interactively:

```bash
DOUBLESHOT_HOME=/opt/doubleshot \
DOUBLESHOT_USER=doubleshot \
APP_TYPE=spring-boot \
APP_ENV_FILE=/opt/myapp/prod.env \
JAVA_BIN=/usr/bin/java \
./install.sh --version vX.Y.Z
```

Supported `APP_TYPE` values are `spring-boot`, `django`, `axum`, and
`express`. The app type sets the default `launch.command` and health path:

| App type | Default launch command | Default health path |
|---|---|---|
| `spring-boot` | `env APP_VERSION={slot} /usr/bin/java -Dserver.port={port} -jar {artifact}` | `/actuator/health` |
| `django` | `env PORT={port} APP_VERSION={slot} /usr/bin/python3 {artifact}` | `/health` |
| `axum` | `env PORT={port} APP_VERSION={slot} {artifact}` | `/health` |
| `express` | `env PORT={port} VERSION={slot} /usr/bin/node {artifact}` | `/health` |

Set `LAUNCH_COMMAND` or `HEALTH_PATH` to override those defaults.

Updates preserve `doubleshot.toml` and the existing service unit:

```bash
./update.sh --version vX.Y.Z
```

Uninstall removes the service, sudoers rule, and binary while preserving
deployment state and config by default:

```bash
./uninstall.sh
# remove /opt/doubleshot and the nginx include directory too
./uninstall.sh --purge
```

**Build from source** (requires Rust 1.85+):

```bash
cargo install doubleshot
# or directly from the repository
cargo install --git https://github.com/mrodz/doubleshot
```

---

## Commands

```bash
doubleshot init-config
doubleshot init-config --output doubleshot.toml
doubleshot init-config --from-nginx --nginx-conf /etc/nginx/nginx.conf
doubleshot serve --config /opt/doubleshot/doubleshot.toml
doubleshot deploy build/libs/app.jar --config /opt/doubleshot/doubleshot.toml
doubleshot status --config /opt/doubleshot/doubleshot.toml
doubleshot follow app-20260519T143000.jar --config /opt/doubleshot/doubleshot.toml
```

`serve` watches the configured inbox directory and deploys artifacts as they
arrive. `deploy <artifact>` imports and deploys one artifact immediately on the
current machine. `follow <artifact-name>` streams deployment events written by
`serve` until that artifact succeeds or fails. Queue artifacts with unique names
when using `follow`, so the deployer follows the current deployment rather than
an older `app.jar` event.

`init-config --from-nginx` scans existing Nginx config, ranks likely
reverse-proxy locations, and prints a proposed `doubleshot.toml`. It does not
modify Nginx. Use `--server-name api.example.com` to bias candidate selection
when one Nginx host has multiple proxied apps.

Common path flags can override config values:

```bash
doubleshot serve --home /opt/my-app
doubleshot status --runtime-dir /opt/my-app/runtime
```

## Config

`doubleshot.toml` is the primary interface. If `--config` is omitted,
`doubleshot` tries `./doubleshot.toml`; if that file is missing, defaults are
used.

```toml
home = "/opt/doubleshot"
poll_seconds = 5
shutdown_timeout_seconds = 20

[slots.blue]
port = 8081

[slots.green]
port = 8082

[launch]
command = "/usr/bin/java -Dserver.port={port} -jar {artifact}"
env_files = ["/opt/myappname/prod.env"]

[health]
kind = "http"
url = "http://127.0.0.1:{port}/actuator/health"
method = "GET"
expected_status = 200
timeout_seconds = 120
interval_millis = 1000
headers = { "X-Origin-Verify" = "replace-me" }

[switch]
kind = "nginx-proxy-pass-include"
path = "/etc/nginx/doubleshot/proxy-pass.inc"
reload_command = "sudo nginx -t && sudo systemctl reload nginx"
host = "127.0.0.1"
```

Template placeholders are available in launch commands, health URLs, and switch
host values:

```text
{artifact}  imported release artifact path, shell-quoted
{release}   same as artifact, shell-quoted
{port}      target slot port
{slot}      target slot name
```

## Runtime State

By default, `home = "/opt/doubleshot"` creates:

```text
/opt/doubleshot/
  inbox/       # serve watches here
  releases/    # imported artifacts
  runtime/     # deploy.lock, active-slot, active-release, pid files, logs, deployments.log
```

The deployment semaphore is `runtime/deploy.lock`. It is created atomically so
only one promotion can run at a time.

## Health Checks

Two health check kinds are supported:

```toml
[health]
kind = "tcp"
host = "127.0.0.1"
timeout_seconds = 120
interval_millis = 1000
```

```toml
[health]
kind = "http"
url = "http://127.0.0.1:{port}/actuator/health"
method = "GET"
expected_status = 200
timeout_seconds = 120
interval_millis = 1000
headers = { "X-Origin-Verify" = "replace-me" }
```

HTTP health checks currently support plain `http://` URLs with an explicit port.

## Nginx Switching

The first switch strategy writes a tiny include file containing the active
`proxy_pass`, then runs the configured reload command.

For the current BankerBee Nginx config, replace:

```nginx
proxy_pass http://127.0.0.1:8080;
```

with:

```nginx
include /etc/nginx/doubleshot/proxy-pass.inc;
```

During promotion, `doubleshot` rewrites that include as:

```nginx
proxy_pass http://127.0.0.1:8081;
```

or the other configured slot port.

The rest of the server block can keep owning TLS, origin verification, rate
limits, and proxy headers.

## Tests

Run the normal unit tests:

```bash
cargo test
```

Docker-backed integration tests use `testcontainers` and are feature-gated so
normal test runs do not require Docker:

```bash
cargo test --features docker-tests
```
