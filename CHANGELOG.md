# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: breaking changes may occur in any release).

This file is generated from the commit history by [git-cliff](https://git-cliff.org)
— do not edit it by hand (see `cliff.toml`).

## [0.1.0-alpha.2](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha..v0.1.0-alpha.2) - 2026-07-09

### Added

- *cli*: Shell completions via `audible completions` (AUD-143, Phase 1) ([#5](https://github.com/mkb79/audible-rs/pull/5)) ([4b7c50e](https://github.com/mkb79/audible-rs/commit/4b7c50ec45c0ac74758c2378a9c67ca56f7fc22f))
- *release*: Generated changelog (git-cliff) + one-button dispatch releases (AUD-145) ([#6](https://github.com/mkb79/audible-rs/pull/6)) ([d8934e1](https://github.com/mkb79/audible-rs/commit/d8934e1a5063a656698c980eae6693cd624f6d7b))
- *cli*: Group and order --help options (AUD-18) ([#4](https://github.com/mkb79/audible-rs/pull/4)) ([96d75fd](https://github.com/mkb79/audible-rs/commit/96d75fdbaa59aab39e66f148c5a05cbe75efc7bf))
- *release*: Auto-detect prerelease from the tag; installer defaults to stable (AUD-140) ([b45b022](https://github.com/mkb79/audible-rs/commit/b45b02289b625d1ffbc58801f9d8518f879c37fa))

## [0.1.0-alpha] - 2026-07-08

### Added

- *release*: Pre-release binaries + curl installer (AUD-140, Phase 1) ([#1](https://github.com/mkb79/audible-rs/pull/1)) ([99a9e56](https://github.com/mkb79/audible-rs/commit/99a9e5678d05d54dde9427297c8f61c060d14e34))

### Fixed

- *release*: Build x86_64-apple-darwin on Apple Silicon; Rosetta-safe installer (AUD-140) ([10332d7](https://github.com/mkb79/audible-rs/commit/10332d76327bde76ac0cc5be18355fa45d2e124d))

