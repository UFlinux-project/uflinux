#!/bin/sh
#
# Build the browser client and drop its output where the gateway embeds it.
#
# The gateway serves every asset out of the binary (include_bytes!), so the
# wasm and its wasm-bindgen glue must exist in rds/gateway/assets before the
# gateway is compiled. Committed placeholders keep a plain `cargo build`
# working; run this script to replace them with a real client.
#
# Usage (from anywhere):  rds/scripts/build-wasm.sh
# Environment:
#   WASM_BINDGEN       path to the wasm-bindgen CLI (default: wasm-bindgen on PATH)
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
target_dir="${CARGO_TARGET_DIR:-target}"

# Distributions inject distro-wide rustflags for native packages (Alpine's
# abuild, for example, adds "-C link-arg=-Wl,-z,pack-relative-relocs"). Those
# are ELF linker options and wasm-ld rejects them, so the browser build runs
# with the flags the wasm backend actually understands.
unset CARGO_ENCODED_RUSTFLAGS
RUSTFLAGS=""
export RUSTFLAGS

if ! "$CARGO" build -p "$CRATE" --target "$TARGET" --"$PROFILE"; then
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

mkdir -p "$out_dir" || exit 1
"$WASM_BINDGEN" --target web --out-dir "$out_dir" "$wasm_input"

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
ASSETS="app.css login.js console.js $lib_name.js ${lib_name}_bg.wasm"

if command -v brotli >/dev/null 2>&1; then
  for name in $ASSETS; do
    file="$out_dir/$name"
    if [ ! -f "$file" ]; then
      echo "build-wasm: $name missing, skipping its brotli copy" >&2
      continue
    fi
    before=$(wc -c <"$file")
    if brotli -q "$BROTLI_LEVEL" -o "$file.br" "$file"; then
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
for name in "$lib_name.js" "${lib_name}_bg.wasm" "$lib_name.d.ts" "${lib_name}_bg.wasm.d.ts"; do
  file="$out_dir/$name"
  if [ -f "$file" ]; then
    printf '  %-32s %8d bytes\n' "$name" "$(wc -c <"$file")"
  fi
done

# The .d.ts files are development aids; the gateway only embeds the two above.
echo "build-wasm: done. Rebuild the gateway to embed the new client."
