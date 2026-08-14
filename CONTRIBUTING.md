# Contributing

This repository maintains the Android/Termux port of
[Grok Build](https://github.com/xai-org/grok-build). Its focus is the platform
port, build and release tooling, CI, and documentation. The upstream source is
kept in the tree because it is the build input, but this repository is not a
separate development home for Grok features.

## Good fits for this repository

- Android or Termux compatibility fixes
- Build, packaging, linker, or release-script improvements
- GitHub Actions and artifact-validation improvements
- Documentation for installing, building, and using the Termux port
- Reproducible reports for Android-specific failures

Changes that are independent of Android or Termux are generally better suited
to the [upstream repository](https://github.com/xai-org/grok-build).

## Issues and pull requests

Before opening an issue or pull request, check whether it is specific to the
Termux port. Include the Android architecture, Termux environment, command
used, and the relevant error output when reporting a build or runtime problem.

Keep changes focused. Never include credentials or other sensitive local data;
generated build output does not belong in a pull request.

For shell-script changes, run:

```sh
bash -n scripts/*.sh
```

When possible, also run a local Termux build with:

```sh
sh scripts/build-termux.sh
```

Focused pull requests are welcome. Please describe what changed, how it was
tested, and whether the change affects the published Android artifact.

## Security reports

Do not open a public issue for a security vulnerability. For issues specific to
this repository's scripts or release artifacts, use a
[private GitHub Security Advisory](https://github.com/Duro02/grok-build-termux/security/advisories/new).
For vulnerabilities in the upstream Grok project, follow the
[upstream security policy](https://github.com/xai-org/grok-build/blob/main/SECURITY.md).

## License

Contributions are subject to the Apache License, Version 2.0. See
[`LICENSE`](LICENSE) and [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) for the
applicable licensing information.
