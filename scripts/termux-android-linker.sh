#!/usr/bin/env bash
# Termux's libc++ package provides the shared C++ runtime only. Rust's
# built-in aarch64-linux-android target asks for libc++_static by default, so
# translate that one library name before invoking the Termux linker. The
# relative runpath lets a binary installed at $PREFIX/bin find Termux's
# shared libraries at $PREFIX/lib on both native and cross builds.
set -euo pipefail

grok_termux_clang=${GROK_TERMUX_CLANG:-clang}
link_args=()
for arg in "$@"; do
    if [[ "$arg" == "-lc++_static" ]]; then
        link_args+=("-lc++_shared")
    else
        link_args+=("$arg")
    fi
done

link_args+=("-Wl,-rpath,\$ORIGIN/../lib")

exec "$grok_termux_clang" "${link_args[@]}"
