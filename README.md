# doubleshot

`doubleshot` is a generic blue-green deployment helper. It can run as a daemon
that watches an inbox, or it can deploy a local artifact directly. The app
runtime, health check, slots, and traffic switch are configured instead of being
hardcoded to one stack.

## Commands

```bash
doubleshot init-config
doubleshot init-config --output doubleshot.toml
doubleshot serve --config /opt/doubleshot/doubleshot.toml
doubleshot deploy build/libs/app.jar --config /opt/doubleshot/doubleshot.toml
doubleshot status --config /opt/doubleshot/doubleshot.toml
```

`serve` watches the configured inbox directory and deploys artifacts as they
arrive. `deploy <artifact>` imports and deploys one artifact immediately on the
current machine.

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
env_files = ["/opt/bankerbee/prod.env"]

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
  runtime/     # deploy.lock, active-slot, active-release, pid files, logs
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
