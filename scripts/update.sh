#!/usr/bin/env bash
set -euo pipefail

REPO="${DOUBLESHOT_REPO:-mrodz/doubleshot}"
VERSION="${VERSION:-latest}"
TARGET_TRIPLE="${TARGET_TRIPLE:-}"
BIN_PATH="${BIN_PATH:-/usr/local/bin/doubleshot}"
RESTART_SERVICE=1

usage() {
  cat <<'EOF'
Usage: update.sh [options]

Update the doubleshot binary and restart the existing systemd service.

Options:
  --version <vX.Y.Z|latest>  Release version to install (default: latest)
  --target <triple>          Override target triple (default: detected Linux glibc)
  --bin-path <path>          Binary path to replace (default: /usr/local/bin/doubleshot)
  --no-restart               Do not restart doubleshot after installing the binary
  -h, --help                 Show this help
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
      shift 2
      ;;
    --no-restart)
      RESTART_SERVICE=0
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

detect_target() {
  if [[ "$(uname -s)" != "Linux" ]]; then
    echo "update.sh only supports Linux systemd hosts" >&2
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

semver_major() {
  sed -n 's/.*[^0-9v]v\{0,1\}\([0-9][0-9]*\)\.[0-9][0-9]*\.[0-9][0-9]*.*/\1/p; s/^v\{0,1\}\([0-9][0-9]*\)\.[0-9][0-9]*\.[0-9][0-9]*.*/\1/p' \
    | head -1
}

installed_version() {
  if [[ ! -x "$BIN_PATH" ]]; then
    echo "cannot determine current doubleshot version because $BIN_PATH is not executable" >&2
    exit 1
  fi

  "$BIN_PATH" --version
}

prevent_major_version_change() {
  local current_version="$1"
  local target_version="$2"
  local current_major
  local target_major

  current_major="$(printf '%s\n' "$current_version" | semver_major)"
  target_major="$(printf '%s\n' "$target_version" | semver_major)"

  if [[ -z "$current_major" ]]; then
    echo "failed to parse current doubleshot version: $current_version" >&2
    exit 1
  fi

  if [[ -z "$target_major" ]]; then
    echo "failed to parse target doubleshot version: $target_version" >&2
    exit 1
  fi

  if [[ "$current_major" != "$target_major" ]]; then
    echo "refusing to update across major versions: $current_version -> $target_version" >&2
    exit 1
  fi
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

need curl
need tar
need sed

TARGET_TRIPLE="${TARGET_TRIPLE:-$(detect_target)}"
if [[ "$VERSION" == "latest" ]]; then
  VERSION="$(latest_version)"
  if [[ -z "$VERSION" ]]; then
    echo "failed to resolve latest release version" >&2
    exit 1
  fi
fi

prevent_major_version_change "$(installed_version)" "$VERSION"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

download_release "$VERSION" "$TARGET_TRIPLE" "$tmp_dir"
"$tmp_dir/doubleshot" --version >/dev/null

backup=""
if [[ -f "$BIN_PATH" ]]; then
  backup="$BIN_PATH.bak.$(date -u +%Y%m%dT%H%M%SZ)"
  sudo_cmd cp "$BIN_PATH" "$backup"
fi

sudo_cmd install -m 0755 "$tmp_dir/doubleshot" "$BIN_PATH"

if command -v systemctl >/dev/null 2>&1 \
  && systemctl list-unit-files doubleshot.service --no-legend 2>/dev/null | grep -q '^doubleshot\.service'; then
  sudo_cmd systemctl daemon-reload
  if [[ "$RESTART_SERVICE" -eq 1 ]]; then
    sudo_cmd systemctl restart doubleshot
  fi
fi

echo "Updated doubleshot to $VERSION at $BIN_PATH"
if [[ -n "$backup" ]]; then
  echo "Previous binary backup: $backup"
fi
