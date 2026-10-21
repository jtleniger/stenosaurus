#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/vendor"
VENDOR="$ROOT/static/vendor/cactus-needle-0.3.0"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$DEST" "$VENDOR"

curl -fL --retry 3 --retry-delay 1 -o "$TMP/cactus-needle.tgz" \
  "https://registry.npmjs.org/cactus-needle/-/cactus-needle-0.3.0.tgz"
echo "0df3735368adb72c1fc08f217c1dcc768b6a06b499d6aba39ce4883220b3373a  $TMP/cactus-needle.tgz" | sha256sum -c -
tar -xzf "$TMP/cactus-needle.tgz" -C "$TMP"
rm -rf "$VENDOR/dist" "$VENDOR/vendor"
mkdir -p "$VENDOR/dist" "$VENDOR/vendor"
cp -R "$TMP/package/dist/." "$VENDOR/dist/"
cp "$TMP/package/vendor/needle.wasm" "$VENDOR/vendor/needle.wasm"
cp "$TMP/package/LICENSE" "$TMP/package/NOTICE" "$VENDOR/"
echo "c43f48e11f302087250d1e406024956781cd2f02595a5e343b3d7ddd5ef707fa  $VENDOR/vendor/needle.wasm" | sha256sum -c -

curl -fL --retry 3 --retry-delay 1 -o "$DEST/whistle.cact" \
  "https://huggingface.co/Cactus-Compute/whistle/resolve/b358ddadd89b7a713b5aa131f23032d3cca1b251/whistle.cact"
echo "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb  $DEST/whistle.cact" | sha256sum -c -

echo "Whistle assets ready: $DEST/whistle.cact and $VENDOR"
