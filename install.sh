#!/bin/sh
# Install the latest t3up release into ~/.local/bin (or $T3UP_INSTALL_DIR).
set -eu
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-musl ;;
  *) echo "No t3up build for $(uname -s) $(uname -m); use: cargo install --git https://github.com/hkarlsen06/t3up" >&2; exit 1 ;;
esac
dir=${T3UP_INSTALL_DIR:-$HOME/.local/bin}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "https://github.com/hkarlsen06/t3up/releases/latest/download/t3up-$target.tar.gz" | tar xz -C "$tmp"
mkdir -p "$dir"
mv "$tmp/t3up-$target/t3up" "$dir/t3up"
echo "Installed $("$dir/t3up" --version) to $dir/t3up"
case ":$PATH:" in *":$dir:"*) ;; *) echo "Add $dir to your PATH to run it as t3up." ;; esac
