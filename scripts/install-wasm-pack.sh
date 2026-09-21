#!/bin/sh
# Direct GitHub release download with retries. The installer script 504s.
set -eu
ver="${WASM_PACK_VERSION:-0.13.1}"
dest="${CARGO_HOME:-${HOME}/.cargo}/bin"
mkdir -p "$dest"
url="https://github.com/rustwasm/wasm-pack/releases/download/v${ver}/wasm-pack-v${ver}-x86_64-unknown-linux-musl.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
ok=0
i=0
while [ "$i" -lt 6 ]; do
  i=$((i + 1))
  if curl -fsSL --retry 5 --retry-delay 2 --retry-all-errors -o "$tmp/wp.tgz" "$url"; then
    tar -xzf "$tmp/wp.tgz" -C "$tmp"
    bin="$(find "$tmp" -name wasm-pack -type f -print | awk 'NR==1{print; exit}')"
    if [ -n "$bin" ]; then
      install -m 755 "$bin" "$dest/wasm-pack"
      ok=1
      break
    fi
  fi
  sleep $((i * 3))
done
if [ "$ok" -ne 1 ]; then
  cargo install wasm-pack --version "$ver" --locked
fi
"$dest/wasm-pack" --version
