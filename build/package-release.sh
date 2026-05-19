#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="$REPO_DIR/target"
BINARY="doubleshot"

VERSION="${VERSION:-$(grep '^version' "$REPO_DIR/Cargo.toml" | head -1 | sed 's/.*= *"\(.*\)"/\1/')}"
OUT_DIR="${OUT_DIR:-$REPO_DIR/dist}"

TARGETS=(
  "aarch64-apple-darwin"
  "aarch64-unknown-linux-gnu"
  "aarch64-unknown-linux-musl"
  "x86_64-apple-darwin"
  "x86_64-unknown-linux-gnu"
  "x86_64-unknown-linux-musl"
)

echo "Packaging $BINARY v$VERSION"
echo "Output: $OUT_DIR"
echo

echo "Building targets..."
for target in "${TARGETS[@]}"; do
  echo "  BUILD $target"
  cargo zigbuild --release --target "$target"
done
echo

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

for target in "${TARGETS[@]}"; do
  bin="$TARGET_DIR/$target/release/$BINARY"

  if [[ ! -f "$bin" ]]; then
    echo "  SKIP  $target  (no binary found)"
    continue
  fi

  archive_name="${BINARY}-v${VERSION}-${target}"
  package_dir="$TARGET_DIR/package/$archive_name"
  rm -rf "$package_dir"
  mkdir -p "$package_dir"
  cp "$bin" "$package_dir/$BINARY"
  cp "$REPO_DIR/scripts/install.sh" "$package_dir/install.sh"
  cp "$REPO_DIR/scripts/update.sh" "$package_dir/update.sh"
  cp "$REPO_DIR/scripts/uninstall.sh" "$package_dir/uninstall.sh"
  chmod 0755 "$package_dir/$BINARY" "$package_dir/install.sh" "$package_dir/update.sh" "$package_dir/uninstall.sh"

  case "$target" in
    *apple-darwin*)
      archive="$OUT_DIR/${archive_name}.tar.gz"
      tar -czf "$archive" -C "$package_dir" "$BINARY" "install.sh" "update.sh" "uninstall.sh"
      shasum -a 256 "$archive" | awk '{print $1}' > "${archive}.sha256"
      echo "  OK    $target  →  $(basename "$archive")"
      ;;
    *linux*)
      archive="$OUT_DIR/${archive_name}.tar.gz"
      tar -czf "$archive" -C "$package_dir" "$BINARY" "install.sh" "update.sh" "uninstall.sh"
      shasum -a 256 "$archive" | awk '{print $1}' > "${archive}.sha256"
      echo "  OK    $target  →  $(basename "$archive")"
      ;;
    *)
      echo "  SKIP  $target  (unrecognised target)"
      ;;
  esac
done

# Combined checksums file
echo
echo "Writing checksums.txt"
(
  cd "$OUT_DIR"
  for f in *.sha256; do
    printf "$(cat "$f")  ${f%.sha256}\n"
  done
) > "$OUT_DIR/checksums.txt"
rm -f "$OUT_DIR"/*.sha256

echo
echo "Done. Artifacts in $OUT_DIR:"
ls -lh "$OUT_DIR"
