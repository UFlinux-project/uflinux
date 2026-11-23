#!/bin/sh
#
# Build the browser client and drop its output where the gateway embeds it.
#
# The gateway serves every asset out of the binary (include_bytes!), so the
# wasm and its wasm-bindgen glue must exist in rds/gateway/assets before the
# gateway is compiled. Run this script before building the gateway.
#
# Usage (from anywhere):  rds/scripts/build-wasm.sh
# Environment:
#   WASM_BINDGEN       path to the wasm-bindgen CLI (default: wasm-bindgen on PATH)
#   WASM_OPT           path to Binaryen wasm-opt (default: wasm-opt on PATH)
#   CARGO              cargo binary to use (default: cargo)
#   PROFILE            cargo profile directory name (default: release)
#   CARGO_TARGET_DIR   cargo's target directory (default: ./target)
#
# POSIX sh only: abuild runs the recipe with BusyBox ash, which cannot parse
# bash arrays, [[ ]] or ${BASH_SOURCE[0]}.

set -eu

# wasm-bindgen refuses to process a module whose embedded version marker differs
# from its own, so pin the exact version the crate depends on.
BINDGEN_VERSION="0.2.122"
CRATE="uflinux-rds-web"
TARGET="wasm32-unknown-unknown"

# Resolve the workspace (rds/) from this script's location, not the caller's cwd.
# shellcheck disable=SC1007  # `CDPATH= cmd` runs cmd with that variable cleared.
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
workspace=$(dirname -- "$script_dir")
out_dir="$workspace/gateway/assets"

cd "$workspace" || exit 1

CARGO="${CARGO:-cargo}"
PROFILE="${PROFILE:-release}"
WASM_BINDGEN="${WASM_BINDGEN:-wasm-bindgen}"
WASM_OPT="${WASM_OPT:-wasm-opt}"
target_dir="${CARGO_TARGET_DIR:-target}"

# Distributions inject distro-wide rustflags for native packages (Alpine's
# abuild, for example, adds "-C link-arg=-Wl,-z,pack-relative-relocs"). Those
# are ELF linker options and wasm-ld rejects them, so the browser build runs
# with the flags the wasm backend actually understands.
unset CARGO_ENCODED_RUSTFLAGS
RUSTFLAGS=""
export RUSTFLAGS

if ! "$CARGO" build --locked -p "$CRATE" --target "$TARGET" --"$PROFILE"; then
  echo "build-wasm: cargo build failed" >&2
  echo "build-wasm: is the $TARGET target installed?  rustup target add $TARGET" >&2
  exit 1
fi

# Cargo names the artifact after the crate with hyphens turned into
# underscores.
lib_name=$(printf '%s' "$CRATE" | tr - _)
wasm_input="$target_dir/$TARGET/$PROFILE/$lib_name.wasm"
if [ ! -f "$wasm_input" ]; then
  echo "build-wasm: expected $wasm_input after a successful build" >&2
  exit 1
fi

# --- wasm-bindgen ---------------------------------------------------------
#
# --target web emits a single ES module plus the rewritten .wasm; no wasm-pack
# is needed because the crate is a plain cdylib.

if ! command -v "$WASM_BINDGEN" >/dev/null 2>&1 && [ ! -x "$WASM_BINDGEN" ]; then
  cat >&2 <<EOF
build-wasm: wasm-bindgen not found.
            Install the matching CLI (cargo install wasm-bindgen-cli --version $BINDGEN_VERSION)
            or point WASM_BINDGEN at an existing binary.
EOF
  exit 1
fi

# A version mismatch produces a confusing "rust and wasm versions differ" error
# deep inside the tool, so check up front and fail loudly.
bindgen_version=$("$WASM_BINDGEN" --version | awk '{print $NF}')
if [ "$bindgen_version" != "$BINDGEN_VERSION" ]; then
  echo "build-wasm: wasm-bindgen $BINDGEN_VERSION is required, found $bindgen_version" >&2
  echo "build-wasm: cargo install wasm-bindgen-cli --version $BINDGEN_VERSION --locked" >&2
  exit 1
fi

if ! command -v "$WASM_OPT" >/dev/null 2>&1; then
  echo "build-wasm: wasm-opt not found; install Binaryen or set WASM_OPT" >&2
  exit 1
fi

mkdir -p "$out_dir"
asset_stage=$(mktemp -d "$out_dir/.wasm-build.XXXXXX")
trap 'rm -rf "$asset_stage"' EXIT
"$WASM_BINDGEN" --target web --no-typescript --out-dir "$asset_stage" "$wasm_input"

wasm="$asset_stage/${lib_name}_bg.wasm"
before=$(wc -c <"$wasm")
"$WASM_OPT" -Os "$wasm" -o "$asset_stage/optimized.wasm"
after=$(wc -c <"$asset_stage/optimized.wasm")
printf '  wasm-opt -Os %d -> %d bytes\n' "$before" "$after"
mv "$asset_stage/optimized.wasm" "$out_dir/${lib_name}_bg.wasm"
# The ES module namespace contains the private IronRDP bridge exported by
# wasm-bindgen. Pass it to Rust after initialization to start the page.
cat >>"$asset_stage/$lib_name.js" <<'JS'

import(import.meta.url).then(async (bridge) => {
    await bridge.default();
    await bridge.startConsole(bridge);
}).catch(() => {
    const status = document.getElementById("status");
    if (status) status.textContent = "Could not load the RDP client.";
});
JS
mv "$asset_stage/$lib_name.js" "$out_dir/$lib_name.js"

# --- brotli ---------------------------------------------------------------
#
# The wasm module is several megabytes and dominates first paint, so each
# served asset also gets a brotli-compressed sibling that the gateway serves
# to a browser that offers `Accept-Encoding: br`.
#
# The .br files are generated, not committed (see gateway/assets/.gitignore):
# they would bloat the source bundle that scripts/rds-source.py tars out of
# this directory. A build without the brotli CLI therefore serves identity
# only, which is why the gateway treats a missing .br as "no compressed
# variant" rather than as an error.
BROTLI_LEVEL=11
ASSETS="app.css login.js $lib_name.js ${lib_name}_bg.wasm"

if command -v brotli >/dev/null 2>&1; then
  for name in $ASSETS; do
    file="$out_dir/$name"
    if [ ! -f "$file" ]; then
      echo "build-wasm: $name missing, skipping its brotli copy" >&2
      continue
    fi
    before=$(wc -c <"$file")
    if brotli -f -q "$BROTLI_LEVEL" -o "$file.br" "$file"; then
      after=$(wc -c <"$file.br")
      printf '  brotli %-28s %8d -> %8d bytes\n' "$name" "$before" "$after"
    else
      rm -f "$file.br"
      echo "build-wasm: brotli failed for $name, serving it uncompressed" >&2
    fi
  done
else
  echo "build-wasm: brotli not found; assets will be served uncompressed" >&2
  echo "build-wasm: install it (apk add brotli) to compress the wasm bundle" >&2
fi

# --- report ---------------------------------------------------------------

echo "build-wasm: wrote into $out_dir"
for name in "$lib_name.js" "${lib_name}_bg.wasm"; do
  file="$out_dir/$name"
  if [ -f "$file" ]; then
    printf '  %-32s %8d bytes\n' "$name" "$(wc -c <"$file")"
  fi
done

echo "build-wasm: done. Rebuild the gateway to embed the new client."
