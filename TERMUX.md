# Termux / Android build

The TUI can be built natively in Termux on Android devices using the device's
Rust toolchain. The supported path is Android `aarch64` (the normal Termux
architecture); other architectures may work when their Rust and native
dependencies are available.

## Prerequisites

In Termux:

```sh
pkg update
pkg install rust clang make pkg-config protobuf ripgrep
```

The repository's `bin/protoc` launcher is not an Android DotSlash artifact, so
the build script explicitly uses the Termux `protoc` binary. You can override
it with `PROTOC=/path/to/protoc`.

## Build

From the repository root:

```sh
sh scripts/build-termux.sh
```

Termux provides the shared C++ runtime (`libc++_shared.so`), while Rust's
built-in Android target requests the static name by default. The build script
uses a local linker wrapper to translate that request to the Termux shared
runtime automatically.

The release binary is written to:

```text
target/release/xai-grok-pager
```

To use it as `grok` without copying the build output:

```sh
alias grok="$PWD/target/release/xai-grok-pager"
```

## Linux cross-build and CI

The `scripts/build-termux-ci.sh` entry point works on a Linux computer as well
as in GitHub Actions. It uses the Android NDK to produce an
`aarch64-linux-android` artifact. Install Rust with `rustup`, the NDK, and the
host tools `clang`, `lld`, `binutils`, `libssl-dev`, `pkg-config`, and
`protobuf-compiler`, then run:

```sh
rustup target add aarch64-linux-android
export ANDROID_NDK_ROOT=/path/to/android-ndk
export ANDROID_API=24
./scripts/build-termux-ci.sh
```

The script writes the packaged archive and checksum to `dist/`. GitHub Actions
runs the same script on an Ubuntu runner and uploads the artifact for
validation. Publishing a GitHub Release is a separate step after the artifact
has been checked.

For normal users, the intended path is to download a validated Release rather
than compile on the phone. The native script remains useful as a fallback for
maintainers and for reproducing device-specific failures.

Set `GROK_TERMUX_JOBS` to control parallelism. `1` is the default because the
full workspace closure is large and Android devices can run out of memory
during parallel native builds.

## Android-specific behavior

- The kernel sandbox backend is disabled on Android. Sandbox profile parsing
  and configuration remain available, but no Landlock/Seatbelt enforcement is
  attempted.
- Clipboard text uses `termux-clipboard-get` and `termux-clipboard-set` when
  Termux:API is installed. Image clipboard integration is not available yet.
- Voice capture uses the Android/Oboe `cpal` backend. Android microphone
  permission and the Termux:API audio environment still apply.
- The telemetry machine identifier uses a Termux-stable environment value and
  falls back to the cached agent identifier.
- Ripgrep is resolved from Termux's `PATH`; Android builds deliberately do not
  embed a Linux host ripgrep binary. An explicit Android-native binary can be
  supplied with `GROK_TOOLS_BUNDLE_RG_PATH` and `GROK_SHELL_BUNDLE_RG_PATH`.
- `waitpid-any` uses a portable Unix process-existence fallback because the
  upstream Linux `pidfd` module has no Android target branch.
