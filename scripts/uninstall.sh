#!/usr/bin/env bash
set -euo pipefail

BIN_PATH="${BIN_PATH:-/usr/local/bin/doubleshot}"
DOUBLESHOT_HOME="${DOUBLESHOT_HOME:-/opt/doubleshot}"
DOUBLESHOT_USER="${DOUBLESHOT_USER:-doubleshot}"
NGINX_INCLUDE_PATH="${NGINX_INCLUDE_PATH:-/etc/nginx/doubleshot/proxy-pass.inc}"
PURGE=0
REMOVE_USER=0

usage() {
  cat <<'EOF'
Usage: uninstall.sh [options]

Uninstall doubleshot's systemd service and binary.

By default this preserves deployment state, config, releases, inbox contents,
application env files, and the nginx proxy include file. Use --purge to remove
doubleshot-managed runtime data as well.

Options:
  --bin-path <path>      Binary path to remove (default: /usr/local/bin/doubleshot)
  --home <path>          Doubleshot home directory (default: /opt/doubleshot)
  --user <name>          Service user name (default: doubleshot)
  --nginx-include <path> Nginx proxy include path (default: /etc/nginx/doubleshot/proxy-pass.inc)
  --purge               Remove doubleshot home and nginx include directory
  --remove-user         Remove the service user after uninstalling
  -h, --help            Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin-path)
      BIN_PATH="${2:?missing value for --bin-path}"
      shift 2
      ;;
    --home)
      DOUBLESHOT_HOME="${2:?missing value for --home}"
      shift 2
      ;;
    --user)
      DOUBLESHOT_USER="${2:?missing value for --user}"
      shift 2
      ;;
    --nginx-include)
      NGINX_INCLUDE_PATH="${2:?missing value for --nginx-include}"
      shift 2
      ;;
    --purge)
      PURGE=1
      shift
      ;;
    --remove-user)
      REMOVE_USER=1
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

sudo_cmd() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

remove_file_if_exists() {
  local path="$1"
  if [[ -e "$path" || -L "$path" ]]; then
    sudo_cmd rm -f "$path"
  fi
}

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "uninstall.sh only supports Linux hosts" >&2
  exit 1
fi

if command -v systemctl >/dev/null 2>&1; then
  if systemctl list-unit-files doubleshot.service --no-legend 2>/dev/null | grep -q '^doubleshot\.service'; then
    sudo_cmd systemctl stop doubleshot 2>/dev/null || true
    sudo_cmd systemctl disable doubleshot 2>/dev/null || true
  fi
fi

remove_file_if_exists /etc/systemd/system/doubleshot.service
remove_file_if_exists /etc/sudoers.d/doubleshot-nginx
remove_file_if_exists "$BIN_PATH"

if command -v systemctl >/dev/null 2>&1; then
  sudo_cmd systemctl daemon-reload
  sudo_cmd systemctl reset-failed doubleshot 2>/dev/null || true
fi

if [[ "$PURGE" -eq 1 ]]; then
  if [[ -d "$DOUBLESHOT_HOME" ]]; then
    sudo_cmd rm -rf "$DOUBLESHOT_HOME"
  fi

  nginx_dir="$(dirname "$NGINX_INCLUDE_PATH")"
  if [[ -d "$nginx_dir" ]]; then
    sudo_cmd rm -rf "$nginx_dir"
  fi
else
  echo "Preserved $DOUBLESHOT_HOME"
  echo "Preserved $(dirname "$NGINX_INCLUDE_PATH")"
fi

if [[ "$REMOVE_USER" -eq 1 ]] && id -u "$DOUBLESHOT_USER" >/dev/null 2>&1; then
  sudo_cmd userdel "$DOUBLESHOT_USER" 2>/dev/null || true
fi

echo "Uninstalled doubleshot service and binary"
if [[ "$PURGE" -ne 1 ]]; then
  echo "Run with --purge to remove doubleshot-managed runtime data"
fi
