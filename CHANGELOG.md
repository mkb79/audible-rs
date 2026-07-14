# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: breaking changes may occur in any release).

This file is generated from the commit history by [git-cliff](https://git-cliff.org)
— do not edit it by hand (see `cliff.toml`).

## [0.1.0-alpha.6](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha.5..v0.1.0-alpha.6) - 2026-07-14

### Added

- *release*: Install via Homebrew — brew install mkb79/tap/audible-rs ([#30](https://github.com/mkb79/audible-rs/pull/30)) ([f230d0a](https://github.com/mkb79/audible-rs/commit/f230d0ac3f4564a3ccbba11f4cb18718c2a036e3))

## [0.1.0-alpha.5](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha.4..v0.1.0-alpha.5) - 2026-07-14

### Added

- *self*: Mark builds from source, so a version is never claimed falsely ([#29](https://github.com/mkb79/audible-rs/pull/29)) ([7dc4999](https://github.com/mkb79/audible-rs/commit/7dc49992dd919f913f4b1e44a7f5efca844627d8))
- *self*: Self check and self changelog — release awareness ([#28](https://github.com/mkb79/audible-rs/pull/28)) ([ab110bc](https://github.com/mkb79/audible-rs/commit/ab110bc87e266ba436fe0a10403b3559fbdf1747))

## [0.1.0-alpha.4](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha.3..v0.1.0-alpha.4) - 2026-07-14

### Added

- *download*: --title finds podcast episodes ([#26](https://github.com/mkb79/audible-rs/pull/26)) ([a30a151](https://github.com/mkb79/audible-rs/commit/a30a151c2fbebf0878a2926d4194255126604cae))
- *library*: Library episodes — podcasts noun deprecated (removal in v0.1.0) ([#24](https://github.com/mkb79/audible-rs/pull/24)) ([071996c](https://github.com/mkb79/audible-rs/commit/071996cd522df7402aa48301d9ae77e1544deae8))
- **BREAKING:** *library*: Content kinds — shared --kind filter, episode change tracking ([#23](https://github.com/mkb79/audible-rs/pull/23)) ([da7c7cb](https://github.com/mkb79/audible-rs/commit/da7c7cb4919181622c041f419020e6f529f93f0f))
- *library*: Add and remove subscription titles, podcasts and episodes ([#21](https://github.com/mkb79/audible-rs/pull/21)) ([f963d6d](https://github.com/mkb79/audible-rs/commit/f963d6d3ae74647ea2724bc4eb15784183e00224))

### Fixed

- *login*: Detect Amazon's anti-automation page instead of asking for a code ([#27](https://github.com/mkb79/audible-rs/pull/27)) ([1eec27a](https://github.com/mkb79/audible-rs/commit/1eec27a2108b6f0677da679d524354afe79901f4))
- *download*: Accept audio/mp4 and audio/mp3 for plain-audio episodes ([#20](https://github.com/mkb79/audible-rs/pull/20)) ([832b4f1](https://github.com/mkb79/audible-rs/commit/832b4f181ea2e3f7df5d3b9ff1bcce54f30bc812))

## [0.1.0-alpha.3](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha.2..v0.1.0-alpha.3) - 2026-07-11

### Added

- *plugins*: Library-gui example plugin — local library dashboard in the browser ([#17](https://github.com/mkb79/audible-rs/pull/17)) ([8eecbb2](https://github.com/mkb79/audible-rs/commit/8eecbb2d39a6bf6e7ea54c5b2b3cee8d030d60c9))
- *plugins*: Plugin add|remove — install from local files or https URLs ([#16](https://github.com/mkb79/audible-rs/pull/16)) ([744eca6](https://github.com/mkb79/audible-rs/commit/744eca6228968243dac82df3a9ca613db796adf2))
- *plugins*: Discovery only in the plugin dir (drop the PATH scan) ([#15](https://github.com/mkb79/audible-rs/pull/15)) ([7a1d8e0](https://github.com/mkb79/audible-rs/commit/7a1d8e052401ce762cc2029af622a89b0722caba))
- *cli*: `audible stats` — listening time per month/year, per marketplace ([#12](https://github.com/mkb79/audible-rs/pull/12)) ([3f9ee0e](https://github.com/mkb79/audible-rs/commit/3f9ee0e2eed18ce8260028f00c7318ecae06290b))
- *library*: `list --borrowed` lists titles you don't own (eligible vs other plans) ([#11](https://github.com/mkb79/audible-rs/pull/11)) ([2102f1a](https://github.com/mkb79/audible-rs/commit/2102f1aed531204c917fbef05858c50474332362))
- *account*: `account status` — membership overview (AUD-152) ([#10](https://github.com/mkb79/audible-rs/pull/10)) ([8d441d9](https://github.com/mkb79/audible-rs/commit/8d441d9ef84ea82cb8a022dd489de3cc1b659e0a))

### Fixed

- *plugins*: Describe probes get no TTY and broken reasons carry stderr ([#14](https://github.com/mkb79/audible-rs/pull/14)) ([f9e2cfa](https://github.com/mkb79/audible-rs/commit/f9e2cfa6db189c47aa65b3bb4400938f2d51b81c))
- *download*: Write decrypted m4b moov-first (faststart) ([#13](https://github.com/mkb79/audible-rs/pull/13)) ([8758374](https://github.com/mkb79/audible-rs/commit/8758374f00c450775863a0837424d51f201c9264))

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

