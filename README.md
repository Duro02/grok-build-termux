# Grok Build for Termux

An unofficial Android/Termux port of [Grok Build](https://github.com/xai-org/grok-build).
This repository packages the upstream Rust TUI for Termux and maintains the
Android compatibility changes, build scripts, CI, and release packaging needed
to run it on Android.

> This is a community port. It is not an official Android or Termux release
> from xAI.

The currently supported target is Android `aarch64` (`aarch64-linux-android`).

## Install a prebuilt release

Download the latest Android `aarch64` archive and its checksum from
[Releases](https://github.com/Duro02/grok-build-termux/releases). In the
directory containing both files, run:

```sh
sha256sum -c grok-termux-aarch64-*.tar.gz.sha256
tar -xzf grok-termux-aarch64-*.tar.gz
install -m 755 grok "$PREFIX/bin/grok"
grok --version
```

## Build on Termux

Install the native build prerequisites:

```sh
pkg update
pkg install rust clang make pkg-config protobuf ripgrep
```

Then build from the repository root:

```sh
sh scripts/build-termux.sh
```

The binary is written to `target/release/xai-grok-pager`. You can run it
directly or use a temporary shell alias:

```sh
alias grok="$PWD/target/release/xai-grok-pager"
```

See [`TERMUX.md`](TERMUX.md) for the complete local-build, cross-build, and
Android compatibility guide.

## Cross-compile on Linux

You can build the Android `aarch64` release from a Linux computer without
compiling on the phone. Install Rust with `rustup`, the Android NDK, and the
host build tools:

```sh
sudo apt update
sudo apt install clang lld binutils libssl-dev pkg-config protobuf-compiler
rustup target add aarch64-linux-android
```

Set the NDK location and run the cross-build script from the repository root:

```sh
export ANDROID_NDK_ROOT=/path/to/android-ndk
export ANDROID_API=24
./scripts/build-termux-ci.sh
```

The packaged archive and SHA256 checksum are written to `dist/`:

```text
dist/grok-termux-aarch64-*.tar.gz
dist/grok-termux-aarch64-*.tar.gz.sha256
```

## Authentication and configuration

On first launch, `grok` opens a browser for authentication. See the bundled
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

Configuration and custom model settings are documented in the
[configuration guide](crates/codegen/xai-grok-pager/docs/user-guide/05-configuration.md)
and [custom models guide](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md).

## Android notes

- Android `aarch64` is the supported release architecture.
- Linux kernel sandbox enforcement is unavailable on Android.
- Some clipboard and voice features require Termux:API and the corresponding
  Android permissions.

## Contributing

This repository focuses on the Android/Termux port, build and release tooling,
and documentation. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for contribution
scope and testing expectations.

## Upstream and license

The upstream project is [xai-org/grok-build](https://github.com/xai-org/grok-build).
First-party code is licensed under the Apache License, Version 2.0; see
[`LICENSE`](LICENSE). Third-party and vendored code remains under its original
licenses; see [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) and
[`third_party/NOTICE`](third_party/NOTICE).
