# Termux / Android build

The TUI can be built natively in Termux on Android devices using the device's
Rust toolchain. The supported path is Android `aarch64` (the normal Termux
architecture); other architectures may work when their Rust and native
dependencies are available.

## Prerequisites

In Termux:

```sh
pkg update
pkg install rust clang make pkg-config protobuf
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

The repository also contains a cloud cross-build entry point at
`scripts/build-termux-ci.sh`. GitHub Actions uses the Android NDK to produce
an `aarch64-linux-android` artifact on an Ubuntu runner; the workflow uploads
that artifact for phone validation and does not publish a stable Release
automatically.

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
- `waitpid-any` uses a portable Unix process-existence fallback because the
  upstream Linux `pidfd` module has no Android target branch.
