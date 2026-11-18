#!/bin/sh
set -eu
target=$1
test "$(rustc -vV | sed -n 's/^host: //p')" = "$target"
test -z "${CARGO_ENCODED_RUSTFLAGS:-}"
base=${CARGO_TARGET_DIR:?}
work=$base/settings-pgo
profiles=$base/settings-training
replay=${UFLINUX_SETTINGS_PGO_REPLAY:-0}
mkdir -p "$work/wrapper"
rm -rf "$work/pgo-profiles" "$work/bolt-profiles"
export UFLINUX_SETTINGS_CARGO
UFLINUX_SETTINGS_CARGO=$(command -v cargo)
cat > "$work/wrapper/cargo" <<'WRAPPER'
#!/bin/sh
if [ "${1:-}" = build ]; then
    exec "$UFLINUX_SETTINGS_CARGO" auditable "$@"
fi
exec "$UFLINUX_SETTINGS_CARGO" "$@"
WRAPPER
chmod +x "$work/wrapper/cargo"
PATH="$work/wrapper:$PATH"
export PATH CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_STRIP=none
RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-Wl,-z,nopack-relative-relocs -Csymbol-mangling-version=v0"
case "$target" in
    aarch64-*-linux-musl*) RUSTFLAGS="$RUSTFLAGS -Clink-arg=-Wl,-fini,0" ;;
esac
export RUSTFLAGS
binaries="uflinux-settings uflinux-desktop uflinux-installer uflinux-screencapture uflinux-settingsd uflinux-settingsctl uflinux-settings-system"
release=$work/$target/release
cc -shared -fPIC -o "$work/system-training.so" scripts/system-training.c
train() {
    echo "Running Settings workload: $1$2"
    timeout --kill-after=10s 150s dbus-run-session -- \
        sh scripts/pgo-train.sh "$1" "$release" "$2" "$work/system-training.so"
}
has_profile() {
    if ! find "$1" -name "$2" -type f -size +0c | grep -q .; then
        echo "Missing profile $2 in $1" >&2
        exit 1
    fi
}
set -- --workspace --bins --frozen --target "$target" --target-dir "$work"
cargo pgo info
if [ "$replay" = 1 ]; then
    cp -R "$profiles/pgo-profiles" "$profiles/bolt-profiles" "$work/"
else
    cargo clean --release --target "$target" --target-dir "$work" \
        -p settings-core -p uflinux-settings -p uflinux-desktop -p uflinux-installer \
        -p uflinux-screencapture -p uflinux-settings-daemon -p uflinux-settings-system
    cargo pgo build -- "$@"
    mkdir -p "$work/pgo-profiles"
    for name in $binaries; do
        LLVM_PROFILE_FILE="$work/pgo-profiles/$name-%m-%p.profraw" train "$name" ''
        has_profile "$work/pgo-profiles" "$name-*.profraw"
    done
    cargo pgo bolt build --with-pgo -- "$@"
    for name in $binaries; do
        train "$name" -bolt-instrumented
    done
fi
for name in $binaries; do
    has_profile "$work/pgo-profiles" "$name-*.profraw"
    has_profile "$work/bolt-profiles/$name" '*.fdata'
done
if [ "$replay" = 0 ]; then
    cp -R "$work/pgo-profiles" "$work/bolt-profiles" "$profiles/"
fi
cargo pgo bolt optimize --with-pgo -- "$@"
for name in $binaries; do
    train "$name" -bolt-optimized
    readelf -SW "$release/$name-bolt-optimized" | grep -q '\.dep-v0'
done
mkdir -p "$base/release"
for name in $binaries; do
    install -m755 "$release/$name-bolt-optimized" "$base/release/$name"
done
rm -rf "$work"
