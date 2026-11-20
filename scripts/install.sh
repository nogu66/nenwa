#!/usr/bin/env bash
# Install nenwa (macOS and Linux).
#   curl -fsSL https://github.com/nogu66/nenwa/releases/latest/download/install.sh | bash
#   With an invite: ... | bash -s -- join <invite>
# 1. Download the binary for this OS and CPU and check its SHA256.
# 2. Put it at ~/.local/bin/nenwa.
# 3. Run "nenwa install" (or "nenwa <args>" when args are given).
# NENWA_DOWNLOAD_BASE changes where to download from (a local folder works too).
# NENWA_INSTALL_DRY_RUN=1 only places the binary and prints the command it would run.
set -euo pipefail

base="${NENWA_DOWNLOAD_BASE:-https://github.com/nogu66/nenwa/releases/latest/download}"
case "$base" in *://*) ;; *) base="file://$(cd "$base" && pwd)" ;; esac

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "nenwa: this OS and CPU ($(uname -s) $(uname -m)) are not supported" >&2; exit 1 ;;
esac
file="nenwa-$target.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading nenwa (${target})..."
curl -fsSL "$base/$file" -o "$tmp/$file"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

want="$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/$file" | awk '{ print $1 }')"; else got="$(shasum -a 256 "$tmp/$file" | awk '{ print $1 }')"; fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "nenwa: SHA256 of ${file} does not match SHA256SUMS" >&2
  exit 1
fi

tar -xzf "$tmp/$file" -C "$tmp"
dir="$HOME/.local/bin"
mkdir -p "$dir"
install -m 755 "$tmp/nenwa" "$dir/nenwa"
echo "Placed: $dir/nenwa"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "${dir} is not on your PATH. Add export PATH=\"$dir:\$PATH\" to your shell config to run nenwa by name." ;;
esac

[ $# -gt 0 ] || set -- install
if [ -n "${NENWA_INSTALL_DRY_RUN:-}" ]; then
  echo "Would run (NENWA_INSTALL_DRY_RUN is set): $dir/nenwa $*"
  exit 0
fi
exec "$dir/nenwa" "$@"
