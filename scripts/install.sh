#!/usr/bin/env bash
set -euo pipefail

REPO="${DOUBLESHOT_REPO:-mrodz/doubleshot}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOCAL_BINARY="${LOCAL_BINARY:-$SCRIPT_DIR/doubleshot}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
BIN_PATH="${BIN_PATH:-$INSTALL_DIR/doubleshot}"
VERSION="${VERSION:-latest}"
TARGET_TRIPLE="${TARGET_TRIPLE:-}"
FORCE_CONFIG=0
START_SERVICE=1
CONFIGURE_NGINX_SUDO=1

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Install doubleshot and configure it as a Linux systemd service.

Options:
  --version <vX.Y.Z|latest>  Release version to install (default: latest)
  --target <triple>          Override target triple (default: detected Linux glibc)
  --bin-path <path>          Install binary path (default: /usr/local/bin/doubleshot)
  --force-config             Replace an existing doubleshot.toml after backing it up
  --no-start                 Install and enable service, but do not start it
  --no-nginx-sudo            Do not install sudoers rule for nginx reload
  -h, --help                 Show this help

Environment overrides:
  DOUBLESHOT_HOME            Default: /opt/doubleshot
  DOUBLESHOT_USER            Default: doubleshot
  APP_TYPE                   Default: spring-boot (spring-boot, django, axum, express)
  JAVA_BIN                   Default: /usr/bin/java
  PYTHON_BIN                 Default: /usr/bin/python3
  NODE_BIN                   Default: /usr/bin/node
  LAUNCH_COMMAND             Override generated launch.command
  APP_ENV_FILE               Default: /opt/bankerbee/prod.env
  BLUE_PORT                  Default: 8081
  GREEN_PORT                 Default: 8082
  HEALTH_PATH                Default depends on APP_TYPE
  HEALTH_TIMEOUT_SECONDS     Default: 120
  HEALTH_INTERVAL_MILLIS     Default: 1000
  NGINX_INCLUDE_PATH         Default: /etc/nginx/doubleshot/proxy-pass.inc
  SWITCH_HOST                Default: 127.0.0.1
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="${2:?missing value for --version}"
      shift 2
      ;;
    --target)
      TARGET_TRIPLE="${2:?missing value for --target}"
      shift 2
      ;;
    --bin-path)
      BIN_PATH="${2:?missing value for --bin-path}"
      INSTALL_DIR="$(dirname "$BIN_PATH")"
      shift 2
      ;;
    --force-config)
      FORCE_CONFIG=1
      shift
      ;;
    --no-start)
      START_SERVICE=0
      shift
      ;;
    --no-nginx-sudo)
      CONFIGURE_NGINX_SUDO=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command not found: $1" >&2
    exit 1
  fi
}

sudo_cmd() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

prompt_default() {
  local name="$1"
  local default="$2"
  local answer

  if [[ -t 0 ]]; then
    read -r -p "$name [$default]: " answer
    printf '%s' "${answer:-$default}"
  else
    printf '%s' "$default"
  fi
}

prompt_app_type() {
  local default="$1"
  local answer

  while true; do
    if [[ -t 0 ]]; then
      cat <<EOF
Application type:
  1) Spring Boot
  2) Django
  3) Axum
  4) Express
EOF
      read -r -p "APP_TYPE [$default]: " answer
      answer="${answer:-$default}"
    else
      answer="$default"
    fi

    normalize_app_type "$answer" && return
    echo "Unsupported APP_TYPE: $answer" >&2
  done
}

normalize_app_type() {
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    1|spring|springboot|spring-boot) printf '%s\n' "spring-boot" ;;
    2|django) printf '%s\n' "django" ;;
    3|axum|rust|rust-axum) printf '%s\n' "axum" ;;
    4|express|node|nodejs|node.js) printf '%s\n' "express" ;;
    *) return 1 ;;
  esac
}

default_health_path() {
  case "$1" in
    spring-boot) printf '%s\n' "/actuator/health" ;;
    django|axum|express) printf '%s\n' "/health" ;;
    *) return 1 ;;
  esac
}

default_launch_command() {
  local app_type="$1"
  local java_bin="$2"
  local python_bin="$3"
  local node_bin="$4"

  case "$app_type" in
    spring-boot)
      printf '%s\n' "env APP_VERSION={slot} $java_bin -Dserver.port={port} -jar {artifact}"
      ;;
    django)
      printf '%s\n' "env PORT={port} APP_VERSION={slot} $python_bin {artifact}"
      ;;
    axum)
      printf '%s\n' "env PORT={port} APP_VERSION={slot} {artifact}"
      ;;
    express)
      printf '%s\n' "env PORT={port} VERSION={slot} $node_bin {artifact}"
      ;;
    *)
      return 1
      ;;
  esac
}

detect_target() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "install.sh only supports Linux systemd hosts" >&2
    exit 1
  fi

  if ! command -v systemctl >/dev/null 2>&1; then
    echo "systemctl was not found; install.sh requires systemd" >&2
    exit 1
  fi

  case "$(uname -m)" in
    x86_64|amd64) printf '%s\n' "x86_64-unknown-linux-gnu" ;;
    aarch64|arm64) printf '%s\n' "aarch64-unknown-linux-gnu" ;;
    *)
      echo "unsupported architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac
}

latest_version() {
  curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -1
}

download_release() {
  local version="$1"
  local target="$2"
  local tmp="$3"
  local archive="doubleshot-${version}-${target}.tar.gz"
  local base="https://github.com/$REPO/releases/download/$version"

  echo "Downloading $archive"
  curl -fL "$base/$archive" -o "$tmp/$archive"

  if curl -fsL "$base/checksums.txt" -o "$tmp/checksums.txt"; then
    (
      cd "$tmp"
      if command -v sha256sum >/dev/null 2>&1; then
        grep "  $archive$" checksums.txt | sha256sum -c -
      elif command -v shasum >/dev/null 2>&1; then
        grep "  $archive$" checksums.txt | shasum -a 256 -c -
      else
        echo "sha256sum/shasum not found; skipping checksum verification" >&2
      fi
    )
  else
    echo "checksums.txt was not available; skipping checksum verification" >&2
  fi

  tar -xzf "$tmp/$archive" -C "$tmp"
}

write_config() {
  local path="$1"
  local home="$2"
  local app_user="$3"
  local app_group="$4"
  local launch_command="$5"
  local app_env_file="$6"
  local blue_port="$7"
  local green_port="$8"
  local health_path="$9"
  local health_timeout="${10}"
  local health_interval="${11}"
  local nginx_include="${12}"
  local switch_host="${13}"

  if [[ -f "$path" && "$FORCE_CONFIG" -ne 1 ]]; then
    echo "Config exists at $path; leaving it unchanged"
    return
  fi

  if [[ -f "$path" ]]; then
    sudo_cmd cp "$path" "$path.bak.$(date -u +%Y%m%dT%H%M%SZ)"
  fi

  local tmp
  tmp="$(mktemp)"
  cat > "$tmp" <<EOF
home = "$home"
shutdown_timeout_seconds = 20

[slots.blue]
port = $blue_port

[slots.green]
port = $green_port

[launch]
command = "$launch_command"
env_files = ["$app_env_file"]

[health]
kind = "http"
url = "http://127.0.0.1:{port}$health_path"
method = "GET"
expected_status = 200
timeout_seconds = $health_timeout
interval_millis = $health_interval

[switch]
kind = "nginx-proxy-pass-include"
path = "$nginx_include"
reload_command = "sudo nginx -t && sudo systemctl reload nginx"
host = "$switch_host"
EOF
  sudo_cmd install -m 0640 -o "$app_user" -g "$app_group" "$tmp" "$path"
  rm -f "$tmp"
}

write_service() {
  local app_user="$1"
  local home="$2"
  local config="$3"
  local service="/etc/systemd/system/doubleshot.service"
  local tmp
  tmp="$(mktemp)"

  cat > "$tmp" <<EOF
[Unit]
Description=Doubleshot deployment server
After=network.target nginx.service

[Service]
Type=simple
User=$app_user
WorkingDirectory=$home
ExecStart=$BIN_PATH --config $config serve
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF
  sudo_cmd install -m 0644 "$tmp" "$service"
  rm -f "$tmp"
}

write_nginx_sudoers() {
  local app_user="$1"
  local tmp
  tmp="$(mktemp)"

  cat > "$tmp" <<EOF
$app_user ALL=(root) NOPASSWD: /usr/sbin/nginx -t, /usr/sbin/nginx -t *, /usr/bin/systemctl reload nginx, /bin/systemctl reload nginx
EOF
  if command -v visudo >/dev/null 2>&1; then
    sudo_cmd visudo -cf "$tmp"
  fi
  sudo_cmd install -m 0440 "$tmp" /etc/sudoers.d/doubleshot-nginx
  rm -f "$tmp"
}

USE_LOCAL_BINARY=0
if [[ -x "$LOCAL_BINARY" ]]; then
  USE_LOCAL_BINARY=1
fi

TARGET_TRIPLE="${TARGET_TRIPLE:-$(detect_target)}"
if [[ "$USE_LOCAL_BINARY" -ne 1 ]]; then
  need curl
  need tar
  need sed

  if [[ "$VERSION" == "latest" ]]; then
    VERSION="$(latest_version)"
    if [[ -z "$VERSION" ]]; then
      echo "failed to resolve latest release version" >&2
      exit 1
    fi
  fi
fi

DOUBLESHOT_HOME="$(prompt_default DOUBLESHOT_HOME "${DOUBLESHOT_HOME:-/opt/doubleshot}")"
DOUBLESHOT_USER="$(prompt_default DOUBLESHOT_USER "${DOUBLESHOT_USER:-doubleshot}")"
APP_TYPE="$(prompt_app_type "${APP_TYPE:-spring-boot}")"
JAVA_BIN="$(prompt_default JAVA_BIN "${JAVA_BIN:-/usr/bin/java}")"
PYTHON_BIN="$(prompt_default PYTHON_BIN "${PYTHON_BIN:-/usr/bin/python3}")"
NODE_BIN="$(prompt_default NODE_BIN "${NODE_BIN:-/usr/bin/node}")"
APP_ENV_FILE="$(prompt_default APP_ENV_FILE "${APP_ENV_FILE:-/opt/bankerbee/prod.env}")"
BLUE_PORT="$(prompt_default BLUE_PORT "${BLUE_PORT:-8081}")"
GREEN_PORT="$(prompt_default GREEN_PORT "${GREEN_PORT:-8082}")"
DEFAULT_HEALTH_PATH="$(default_health_path "$APP_TYPE")"
DEFAULT_LAUNCH_COMMAND="$(default_launch_command "$APP_TYPE" "$JAVA_BIN" "$PYTHON_BIN" "$NODE_BIN")"
LAUNCH_COMMAND="$(prompt_default LAUNCH_COMMAND "${LAUNCH_COMMAND:-$DEFAULT_LAUNCH_COMMAND}")"
HEALTH_PATH="$(prompt_default HEALTH_PATH "${HEALTH_PATH:-$DEFAULT_HEALTH_PATH}")"
HEALTH_TIMEOUT_SECONDS="$(prompt_default HEALTH_TIMEOUT_SECONDS "${HEALTH_TIMEOUT_SECONDS:-120}")"
HEALTH_INTERVAL_MILLIS="$(prompt_default HEALTH_INTERVAL_MILLIS "${HEALTH_INTERVAL_MILLIS:-1000}")"
NGINX_INCLUDE_PATH="$(prompt_default NGINX_INCLUDE_PATH "${NGINX_INCLUDE_PATH:-/etc/nginx/doubleshot/proxy-pass.inc}")"
SWITCH_HOST="$(prompt_default SWITCH_HOST "${SWITCH_HOST:-127.0.0.1}")"
CONFIG_PATH="$DOUBLESHOT_HOME/doubleshot.toml"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

if [[ "$USE_LOCAL_BINARY" -eq 1 ]]; then
  install_source="$LOCAL_BINARY"
else
  download_release "$VERSION" "$TARGET_TRIPLE" "$tmp_dir"
  install_source="$tmp_dir/doubleshot"
fi
sudo_cmd install -d -m 0755 "$INSTALL_DIR"
sudo_cmd install -m 0755 "$install_source" "$BIN_PATH"

if ! id -u "$DOUBLESHOT_USER" >/dev/null 2>&1; then
  sudo_cmd useradd --system --create-home --home-dir "$DOUBLESHOT_HOME" --shell /usr/sbin/nologin "$DOUBLESHOT_USER"
fi
sudo_cmd usermod -aG systemd-journal "$DOUBLESHOT_USER" 2>/dev/null || true
DOUBLESHOT_GROUP="$(id -gn "$DOUBLESHOT_USER")"

sudo_cmd install -d -m 0755 -o "$DOUBLESHOT_USER" -g "$DOUBLESHOT_GROUP" "$DOUBLESHOT_HOME"
for dir in inbox inbox/failed releases runtime; do
  sudo_cmd install -d -m 0755 -o "$DOUBLESHOT_USER" -g "$DOUBLESHOT_GROUP" "$DOUBLESHOT_HOME/$dir"
done
sudo_cmd install -d -m 0750 -o "$DOUBLESHOT_USER" -g "$DOUBLESHOT_GROUP" "$(dirname "$APP_ENV_FILE")"
if [[ ! -f "$APP_ENV_FILE" ]]; then
  tmp_env="$(mktemp)"
  printf '# Application environment for doubleshot-launched processes\n' > "$tmp_env"
  sudo_cmd install -m 0640 -o "$DOUBLESHOT_USER" -g "$DOUBLESHOT_GROUP" "$tmp_env" "$APP_ENV_FILE"
  rm -f "$tmp_env"
fi
sudo_cmd install -d -m 0755 -o "$DOUBLESHOT_USER" -g "$DOUBLESHOT_GROUP" "$(dirname "$NGINX_INCLUDE_PATH")"
if [[ ! -f "$NGINX_INCLUDE_PATH" ]]; then
  tmp_include="$(mktemp)"
  printf 'proxy_pass http://%s:%s;\n' "$SWITCH_HOST" "$BLUE_PORT" > "$tmp_include"
  sudo_cmd install -m 0644 -o "$DOUBLESHOT_USER" -g "$DOUBLESHOT_GROUP" "$tmp_include" "$NGINX_INCLUDE_PATH"
  rm -f "$tmp_include"
fi

write_config "$CONFIG_PATH" "$DOUBLESHOT_HOME" "$DOUBLESHOT_USER" "$DOUBLESHOT_GROUP" "$LAUNCH_COMMAND" "$APP_ENV_FILE" "$BLUE_PORT" "$GREEN_PORT" "$HEALTH_PATH" "$HEALTH_TIMEOUT_SECONDS" "$HEALTH_INTERVAL_MILLIS" "$NGINX_INCLUDE_PATH" "$SWITCH_HOST"
write_service "$DOUBLESHOT_USER" "$DOUBLESHOT_HOME" "$CONFIG_PATH"
if [[ "$CONFIGURE_NGINX_SUDO" -eq 1 ]]; then
  write_nginx_sudoers "$DOUBLESHOT_USER"
fi

sudo_cmd systemctl daemon-reload
sudo_cmd systemctl enable doubleshot
if [[ "$START_SERVICE" -eq 1 ]]; then
  sudo_cmd systemctl restart doubleshot
fi

echo
echo "Installed doubleshot $VERSION to $BIN_PATH"
echo "Config: $CONFIG_PATH"
echo "Service: doubleshot"
echo
echo "Useful commands:"
echo "  sudo systemctl status doubleshot"
echo "  sudo journalctl -u doubleshot -f"
