#!/usr/bin/env bash
# Cross-compile the Grok Build TUI for Android/Termux on a Linux CI host.
#
# The actual Cargo composition stays in build-termux.sh. This wrapper only
# supplies the Android NDK toolchain and stages a deterministic CI artifact.
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$repo_dir"

android_api=${ANDROID_API:-24}
grok_termux_jobs=${GROK_TERMUX_JOBS:-2}
ndk_root=${ANDROID_NDK_ROOT:-${ANDROID_NDK_HOME:-}}
ndk_host_tag=${ANDROID_NDK_HOST_TAG:-linux-x86_64}

if [ -z "$ndk_root" ]; then
    echo "ANDROID_NDK_ROOT or ANDROID_NDK_HOME is required" >&2
    exit 1
fi

ndk_prebuilt="$ndk_root/toolchains/llvm/prebuilt/$ndk_host_tag"
clang="$ndk_prebuilt/bin/aarch64-linux-android${android_api}-clang"
clangxx="$ndk_prebuilt/bin/aarch64-linux-android${android_api}-clang++"
llvm_ar="$ndk_prebuilt/bin/llvm-ar"
llvm_ranlib="$ndk_prebuilt/bin/llvm-ranlib"

for required in cargo rustc rustup protoc "$clang" "$clangxx" "$llvm_ar" "$llvm_ranlib"; do
    if [ ! -x "$required" ] && ! command -v "$required" >/dev/null 2>&1; then
        echo "required tool is missing: $required" >&2
        exit 1
    fi
done

if ! rustup target list --installed | grep -qx 'aarch64-linux-android'; then
    rustup target add aarch64-linux-android
fi

export PROTOC="${PROTOC:-$(command -v protoc)}"
export GROK_TERMUX_JOBS="$grok_termux_jobs"
export GROK_TERMUX_TARGET=aarch64-linux-android
export GROK_TERMUX_CLANG="$clang"
export CC_aarch64_linux_android="$clang"
export CXX_aarch64_linux_android="$clangxx"
export AR_aarch64_linux_android="$llvm_ar"
export RANLIB_aarch64_linux_android="$llvm_ranlib"

echo "Building xai-grok-pager for Android/Termux (API $android_api; jobs: $GROK_TERMUX_JOBS)"
echo "Using NDK: $ndk_root"
echo "Using linker: $GROK_TERMUX_CLANG"

# Reuse the native build entry point so the local and CI builds keep the same
# package selection and --no-default-features behavior. Only the compiler and
# target support differ between the two environments.
"$repo_dir/scripts/build-termux.sh"

binary="$repo_dir/target/$GROK_TERMUX_TARGET/release/xai-grok-pager"
if [ ! -x "$binary" ]; then
    echo "expected release binary was not produced: $binary" >&2
    exit 1
fi

if ! command -v readelf >/dev/null 2>&1; then
    echo "readelf is required to validate the Android ELF artifact" >&2
    exit 1
fi

if ! readelf -h "$binary" | grep -q 'AArch64'; then
    echo "the release binary is not an AArch64 ELF: $binary" >&2
    readelf -h "$binary" >&2 || true
    exit 1
fi

out_dir=${GROK_TERMUX_OUT_DIR:-$repo_dir/dist}
mkdir -p "$out_dir"

version=${GROK_VERSION:-$(git describe --tags --always 2>/dev/null || git rev-parse --short HEAD)}
safe_version=$(printf '%s' "$version" | tr '/ ' '__')
artifact_base="grok-termux-aarch64-${safe_version}"
stage_dir="$out_dir/$artifact_base"
artifact="$out_dir/$artifact_base.tar.gz"

mkdir -p "$stage_dir"
rm -f "$stage_dir/grok" "$stage_dir/TERMUX.md" "$stage_dir/build-info.json" "$artifact" "$artifact.sha256"
cp "$binary" "$stage_dir/grok"
cp "$repo_dir/TERMUX.md" "$stage_dir/TERMUX.md"

commit=$(git rev-parse HEAD)
workflow_sha=${GITHUB_SHA:-$commit}
ndk_version=${ANDROID_NDK_VERSION:-unknown}
printf '{\n  "artifact": "%s",\n  "commit": "%s",\n  "workflowSha": "%s",\n  "target": "aarch64-linux-android",\n  "androidApi": %s,\n  "ndkVersion": "%s"\n}\n' \
    "$artifact_base" "$commit" "$workflow_sha" "$android_api" "$ndk_version" \
    > "$stage_dir/build-info.json"

tar -C "$stage_dir" -czf "$artifact" .
(
    cd "$out_dir"
    sha256sum "$(basename "$artifact")" > "$(basename "$artifact").sha256"
)

echo
echo "Built:    $binary"
echo "Artifact: $artifact"
echo "Checksum: $artifact.sha256"
