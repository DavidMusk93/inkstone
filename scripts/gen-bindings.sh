#!/usr/bin/env bash
# Generate host-language bindings for the engine.
#
#   ./scripts/gen-bindings.sh swift build/bindings
#   ./scripts/gen-bindings.sh kotlin build/bindings
#
# Swift apps link `libnotebase_ffi.dylib` (macOS) or the static `libnotebase_ffi.a`;
# the module map next to the generated Swift file is what swiftc needs.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

LANG="${1:-swift}"
OUT="${2:-build/bindings}"
mkdir -p "$OUT"

cargo build --release -p notebase-ffi
LIB=""
for candidate in target/release/libnotebase_ffi.dylib target/release/libnotebase_ffi.so target/release/notebase_ffi.dll; do
  if [ -f "$candidate" ]; then LIB="$candidate"; break; fi
done
if [ -z "$LIB" ]; then
  echo "no built library found under target/release" >&2
  exit 1
fi

cargo run --release -q -p notebase-ffi --bin uniffi-bindgen -- \
  generate --library "$LIB" --language "$LANG" --out-dir "$OUT"
echo "bindings: $OUT ($LANG) from $LIB"
