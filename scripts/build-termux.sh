#!/usr/bin/env bash
# Build the Grok Build TUI natively for the current Termux/Android device.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_dir"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required; install it with: pkg install rust" >&2
    exit 1
fi

if [ -n "${PROTOC:-}" ]; then
    protoc_bin=$PROTOC
elif command -v protoc >/dev/null 2>&1; then
    protoc_bin=$(command -v protoc)
elif [ -n "${PREFIX:-}" ] && [ -x "$PREFIX/bin/protoc" ]; then
    protoc_bin="$PREFIX/bin/protoc"
else
    echo "protoc is required; install it with: pkg install protobuf" >&2
    exit 1
fi

export PROTOC="$protoc_bin"
grok_termux_jobs=${GROK_TERMUX_JOBS:-1}
grok_termux_target=${GROK_TERMUX_TARGET:-}

case "$grok_termux_target" in
    ""|aarch64-linux-android)
        ;;
    *)
        echo "unsupported GROK_TERMUX_TARGET: $grok_termux_target" >&2
        echo "supported values: empty (native) or aarch64-linux-android" >&2
        exit 1
        ;;
esac

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$repo_dir/scripts/termux-android-linker.sh"

echo "Building xai-grok-pager for Android/Termux (jobs: $grok_termux_jobs)"
echo "Using protoc: $PROTOC"
echo "Using linker: $CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"

if [ -n "$grok_termux_target" ]; then
    echo "Using target: $grok_termux_target"
fi

# jemalloc is not enabled in the composition-root package's Termux build.
# Kernel sandbox enforcement is also unavailable on Android; its public API
# safely degrades to the no-op backend at compile time.
if [ -n "$grok_termux_target" ]; then
    cargo build -j "$grok_termux_jobs" \
        --release \
        --target "$grok_termux_target" \
        -p xai-grok-pager-bin \
        --no-default-features
else
    cargo build -j "$grok_termux_jobs" \
        --release \
        -p xai-grok-pager-bin \
        --no-default-features
fi

echo
if [ -n "$grok_termux_target" ]; then
    echo "Built: $repo_dir/target/$grok_termux_target/release/xai-grok-pager"
    echo "Run:   $repo_dir/target/$grok_termux_target/release/xai-grok-pager"
else
    echo "Built: $repo_dir/target/release/xai-grok-pager"
    echo "Run:   $repo_dir/target/release/xai-grok-pager"
fi
