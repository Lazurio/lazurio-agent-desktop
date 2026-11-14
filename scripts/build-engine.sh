#!/usr/bin/env bash
# Builds the pinned Agent Desktop engine for one target and puts it where Tauri
# expects its sidecars: src-tauri/binaries/<name>-<target triple>[.exe].
#
#   scripts/build-engine.sh aarch64-apple-darwin
#   scripts/build-engine.sh x86_64-pc-windows-msvc   (Git Bash on Windows)
set -euo pipefail

target="${1:?usage: build-engine.sh <target triple>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
ref_file="$root/engine/agent-desktop.ref"
repository="$(sed -n 's/^repository=//p' "$ref_file")"
commit="$(sed -n 's/^commit=//p' "$ref_file")"
src="$root/.engine-src"

if [ ! -d "$src/.git" ]; then
  git init -q "$src"
  git -C "$src" remote add origin "$repository"
fi
if [ "$(git -C "$src" rev-parse HEAD 2>/dev/null || true)" != "$commit" ]; then
  git -C "$src" fetch -q --depth 1 origin "$commit"
  git -C "$src" checkout -q --force FETCH_HEAD
fi
test "$(git -C "$src" rev-parse HEAD)" = "$commit"

# The engine pins its own toolchain in its rust-toolchain.toml.
(
  cd "$src"
  rustup toolchain install 2>/dev/null || rustup show >/dev/null
  rustup target add "$target" >/dev/null
  cargo build --locked --release -p agent-desktop --target "$target"
  case "$target" in
    *-apple-darwin)
      cargo build --locked --release -p agent-desktop-macos --bin agent-desktop-macos-helper --target "$target"
      ;;
  esac
)

out="$root/src-tauri/binaries"
mkdir -p "$out" "$root/src-tauri/resources"
case "$target" in
  *-windows-*)
    cp "$src/target/$target/release/agent-desktop.exe" "$out/agent-desktop-$target.exe"
    ;;
  *-apple-darwin)
    cp "$src/target/$target/release/agent-desktop" "$out/agent-desktop-$target"
    cp "$src/target/$target/release/agent-desktop-macos-helper" "$out/agent-desktop-macos-helper-$target"
    ;;
  *)
    cp "$src/target/$target/release/agent-desktop" "$out/agent-desktop-$target"
    ;;
esac
cp "$src/LICENSE" "$root/src-tauri/resources/agent-desktop-LICENSE"
echo "engine $commit for $target -> $out"
