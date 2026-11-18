#!/usr/bin/env bash
# nenwa を入れる（Mac と Linux）。
#   curl -fsSL https://github.com/nogu66/nenwa/releases/latest/download/install.sh | bash
#   招待で入るとき: ... | bash -s -- join <招待>
# 1. OS と CPU に合う実行ファイルを落とし、SHA256 を確かめる
# 2. ~/.local/bin/nenwa に置く
# 3. nenwa install（引数があれば nenwa <引数>）を実行する
# NENWA_DOWNLOAD_BASE で落とす場所を変えられる（手元のフォルダでもよい）。
# NENWA_INSTALL_DRY_RUN=1 なら、置くだけで、実行するコマンドを表示して終わる。
set -euo pipefail

base="${NENWA_DOWNLOAD_BASE:-https://github.com/nogu66/nenwa/releases/latest/download}"
case "$base" in *://*) ;; *) base="file://$(cd "$base" && pwd)" ;; esac

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "nenwa: この OS と CPU（$(uname -s) $(uname -m)）には対応していません" >&2; exit 1 ;;
esac
file="nenwa-$target.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "nenwa を落としています（${target}）…"
curl -fsSL "$base/$file" -o "$tmp/$file"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"

want="$(awk -v f="$file" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/$file" | awk '{ print $1 }')"; else got="$(shasum -a 256 "$tmp/$file" | awk '{ print $1 }')"; fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "nenwa: 落としたファイルの SHA256 が合いません（${file}）" >&2
  exit 1
fi

tar -xzf "$tmp/$file" -C "$tmp"
dir="$HOME/.local/bin"
mkdir -p "$dir"
install -m 755 "$tmp/nenwa" "$dir/nenwa"
echo "置きました: $dir/nenwa"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "${dir} に PATH が通っていません。シェルの設定に export PATH=\"$dir:\$PATH\" を足すと、nenwa だけで使えます" ;;
esac

[ $# -gt 0 ] || set -- install
if [ -n "${NENWA_INSTALL_DRY_RUN:-}" ]; then
  echo "実行するコマンド（NENWA_INSTALL_DRY_RUN なので実行しません）: $dir/nenwa $*"
  exit 0
fi
exec "$dir/nenwa" "$@"
