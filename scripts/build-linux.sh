#!/bin/bash
# Build a static Linux binary of queuetop from any host (macOS included) and
# print where it landed. No Docker, zig or cross-compiler needed: the musl
# targets ship their own C runtime and rust-lld links the ELF (see
# .cargo/config.toml).
#
#   scripts/build-linux.sh              # x86_64 (almost every login node)
#   scripts/build-linux.sh aarch64      # arm64 clusters (Grace, Ampere ...)
#   scripts/build-linux.sh x86_64 user@login.cluster.edu:bin/   # build and scp
set -euo pipefail
cd "$(dirname "$0")/.."
# rustup installs into ~/.cargo/bin, which interactive shells do not always have on PATH
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null || ! command -v rustup >/dev/null; then
  echo "rustup/cargo not found. Install with:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  exit 1
fi
arch=${1:-x86_64}
dest=${2:-}
bin=${BIN:-queuetop}
case "$arch" in
  x86_64|aarch64) ;;
  *) echo "usage: $0 [x86_64|aarch64] [scp-destination]" >&2; exit 2 ;;
esac
target=$arch-unknown-linux-musl
if ! rustup target list --installed | grep -qx "$target"; then
  rustup target add "$target"
fi
case "$bin" in
  queuetop) pkgs="-p queuetop"; names="queuetop" ;;
  *) echo "BIN must be queuetop" >&2; exit 2 ;;
esac
# shellcheck disable=SC2086
cargo build --release --target "$target" $pkgs
echo
for n in $names; do
  out=target/$target/release/$n
  echo "built: $out"
  command -v file >/dev/null && file "$out" | sed 's/^/       /'
  du -h "$out" | awk '{print "       " $1}'
  if [ -n "$dest" ]; then
    echo "copying to $dest"
    scp "$out" "$dest"
  fi
done
