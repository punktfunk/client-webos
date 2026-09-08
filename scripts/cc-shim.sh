#!/bin/sh
# Cargo/cc-crate linker+compiler shim for the armv7-unknown-linux-gnueabi target
# (webOS TV). Wraps the webosbrew NDK's arm-webos-linux-gnueabi-gcc with an
# explicit --sysroot: this toolchain build's baked-in default sysroot points at a
# build-time path segment that no longer exists post-relocate (confirmed via
# -print-sysroot vs the actual on-disk layout), so every invocation needs it passed
# explicitly. See `task toolchain` (Taskfile.yml) for how the NDK gets here — this
# script and its C++ counterpart (cxx-shim.sh) are the only build logic that stays
# outside the Taskfile: Cargo's `linker`/`CC`/`CXX` config need a real executable it
# invokes directly with a full compiler argv, not a task name.
set -eu

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NDK_DIR="${PUNKTFUNK_WEBOS_NDK:-$REPO_ROOT/.toolchains/arm-webos-linux-gnueabi_sdk-buildroot}"

SYSROOT="$NDK_DIR/arm-webos-linux-gnueabi/sysroot"

# Link libstdc++ statically: `-lstdc++` becomes the archive, so no libstdc++.so.6
# rides in the .ipk. A bundled one is loaded by the binary's DT_RPATH, which outranks
# the jail's `LD_LIBRARY_PATH=/usr/lib:$APPDIR/lib` — the TV's own libraries then get
# our copy too, and webOS 11's /usr/lib/libhelpers.so.2 wants GLIBCXX_3.4.32 that the
# SDK's 6.0.30 has never had. SDL reports that as "Failed to load webOS libraries".
# Static keeps one binary correct on every firmware: our C++ runtime is ours, the TV's
# stays the TV's. Nothing C++ crosses the boundary — SDL, NDL and Luna are all C.
n=$#
for arg in "$@"; do
    case "$arg" in
    -lstdc++) set -- "$@" "$SYSROOT/usr/lib/libstdc++.a" ;;
    *) set -- "$@" "$arg" ;;
    esac
done
shift "$n"

exec "$NDK_DIR/bin/arm-webos-linux-gnueabi-gcc" --sysroot="$SYSROOT" "$@"
