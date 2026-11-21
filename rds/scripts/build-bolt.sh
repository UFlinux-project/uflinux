#!/bin/sh
set -eu
src=$1
build=$src/bolt-build
prefix=$src/pgo-tools
case $(uname -m) in
    x86_64) target=X86 ;;
    aarch64) target=AArch64 ;;
    *) exit 2 ;;
esac
cmake -S "$src/llvm-project-22.1.3.src/bolt" -B "$build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_COMPILER=clang-22 -DCMAKE_CXX_COMPILER=clang++-22 \
    -DCMAKE_INSTALL_PREFIX="$prefix" -DLLVM_DIR=/usr/lib/llvm22/lib/cmake/llvm \
    -DBOLT_TARGETS_TO_BUILD="$target" -DBOLT_ENABLE_RUNTIME=ON -DLLVM_INCLUDE_TESTS=OFF
cmake --build "$build" --parallel "${JOBS:-2}" --target llvm-bolt merge-fdata
install -Dm755 "$build/bin/llvm-bolt" "$prefix/bin/llvm-bolt"
install -Dm755 "$build/bin/merge-fdata" "$prefix/bin/merge-fdata"
for runtime in instr hugify; do
    install -Dm644 "$build/bolt_rt-bins/lib/libbolt_rt_$runtime.a" "$prefix/lib/libbolt_rt_$runtime.a"
done
