#!/bin/sh
set -eu
target=$1
test "$(rustc -vV | sed -n 's/^host: //p')" = "$target"
test -z "${CARGO_ENCODED_RUSTFLAGS:-}"
base=${CARGO_TARGET_DIR:?}
work=$base/rds-pgo
profiles=$base/rds-training
replay=${UFLINUX_RDS_PGO_REPLAY:-0}
mkdir -p "$work/wrapper"
rm -rf "$work/pgo-profiles" "$work/bolt-profiles"
export UFLINUX_RDS_CARGO
UFLINUX_RDS_CARGO=$(command -v cargo)
cat > "$work/wrapper/cargo" <<'WRAPPER'
#!/bin/sh
if [ "${1:-}" = build ]; then
    exec "$UFLINUX_RDS_CARGO" auditable "$@"
fi
exec "$UFLINUX_RDS_CARGO" "$@"
WRAPPER
chmod +x "$work/wrapper/cargo"
PATH="$work/wrapper:$PATH"
export PATH CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_STRIP=none
RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-Wl,-z,nopack-relative-relocs -Csymbol-mangling-version=v0"
case "$target" in
    aarch64-*-linux-musl*) RUSTFLAGS="$RUSTFLAGS -Clink-arg=-Wl,-fini,0" ;;
esac
export RUSTFLAGS
binaries="uflinux-rdsd uflinux-rds-web uflinux-rdp-session uflinux-rdsctl uflinux-rds-system"
release=$work/$target/release
# Each workload gets a private temp directory and, where it needs one, a private
# session bus, so several of them can train at once. PGO only runs on the native
# x86_64 and aarch64 shards, where the runners have cores to spare.
jobs=${UFLINUX_PGO_JOBS:-3}
case "$jobs" in
    ''|*[!0-9]*) jobs=3 ;;
esac
test "$jobs" -ge 1 || jobs=3
cc -shared -fPIC -o "$work/rds-training.so" scripts/rds-training.c
train() {
    echo "Running RDS workload: $1$2"
    timeout --kill-after=10s 150s dbus-run-session -- \
        sh scripts/pgo-train.sh "$1" "$release" "$2" "$work/rds-training.so"
}
train_pgo() {
    LLVM_PROFILE_FILE="$work/pgo-profiles/$1-%m-%p.profraw" train "$1" ''
}
train_bolt() {
    train "$1" -bolt-instrumented
}
train_final() {
    train "$1" -bolt-optimized
    readelf -SW "$release/$1-bolt-optimized" | grep -q '\.dep-v0'
}
# The chroot shell is BusyBox ash, which has no `wait -n`, so the workloads run
# in batches of at most $jobs and every batch is reaped before the next starts.
collect() {
    status=0
    for pid in "$@"; do
        wait "$pid" || status=1
    done
    if [ "$status" -ne 0 ]; then
        echo "RDS training workloads failed" >&2
        exit 1
    fi
}
dispatch() {
    worker=$1
    shift
    running=0
    pids=
    for name in "$@"; do
        "$worker" "$name" &
        pids="$pids $!"
        running=$((running + 1))
        if [ "$running" -ge "$jobs" ]; then
            # shellcheck disable=SC2086
            collect $pids
            pids=
            running=0
        fi
    done
    # shellcheck disable=SC2086
    collect $pids
}
has_profile() {
    if ! find "$1" -name "$2" -type f -size +0c | grep -q .; then
        echo "Missing profile $2 in $1" >&2
        exit 1
    fi
}
set -- --workspace --bins --frozen --target "$target" --target-dir "$work" \
    --bin uflinux-rdsd --bin uflinux-rds-web --bin uflinux-rdp-session \
    --bin uflinux-rdsctl --bin uflinux-rds-system
# The packaging recipe selects the optional capabilities; cargo-pgo rebuilds
# every binary three times, so the selection has to travel with the flags.
if [ -n "${UFLINUX_RDS_FEATURES:-}" ]; then
    set -- "$@" --features "$UFLINUX_RDS_FEATURES"
fi
cargo pgo info
if [ "$replay" = 1 ]; then
    cp -R "$profiles/pgo-profiles" "$profiles/bolt-profiles" "$work/"
else
    cargo clean --release --target "$target" --target-dir "$work" \
        -p uflinux-rds-common -p uflinux-rds-codec -p uflinux-rds-wayland \
        -p uflinux-rds-broker -p uflinux-rds-gateway -p uflinux-rds-server \
        -p uflinux-rds-ctl -p uflinux-rds-system
    cargo pgo build -- "$@"
    mkdir -p "$work/pgo-profiles"
    # shellcheck disable=SC2086
    dispatch train_pgo $binaries
    for name in $binaries; do
        has_profile "$work/pgo-profiles" "$name-*.profraw"
    done
    cargo pgo bolt build --with-pgo -- "$@"
    # shellcheck disable=SC2086
    dispatch train_bolt $binaries
fi
for name in $binaries; do
    has_profile "$work/pgo-profiles" "$name-*.profraw"
    has_profile "$work/bolt-profiles/$name" '*.fdata'
done
if [ "$replay" = 0 ]; then
    cp -R "$work/pgo-profiles" "$work/bolt-profiles" "$profiles/"
fi
cargo pgo bolt optimize --with-pgo -- "$@"
# shellcheck disable=SC2086
dispatch train_final $binaries
mkdir -p "$base/release"
for name in $binaries; do
    install -m755 "$release/$name-bolt-optimized" "$base/release/$name"
done
rm -rf "$work"
