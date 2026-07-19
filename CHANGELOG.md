# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: breaking changes may occur in any release).

This file is generated from the commit history by [git-cliff](https://git-cliff.org)
— do not edit it by hand (see `cliff.toml`).

## [0.1.0-alpha.8](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha.7..v0.1.0-alpha.8) - 2026-07-19

### Added

- **BREAKING:** *output*: Every command answers -o json with one constant envelope ([#94](https://github.com/mkb79/audible-rs/pull/94)) ([93465b1](https://github.com/mkb79/audible-rs/commit/93465b11c792ff77f561e29abeb3c5d460b2f1a3))
- *download*: Refuse lapsed subscription titles before the licenserequest ([#92](https://github.com/mkb79/audible-rs/pull/92)) ([b300172](https://github.com/mkb79/audible-rs/commit/b3001724a2e0cd98995725286d88a2717f0f187c))
- *auth*: Auto mode sends the access token, signing only for annotations ([#91](https://github.com/mkb79/audible-rs/pull/91)) ([cff4a3a](https://github.com/mkb79/audible-rs/commit/cff4a3a9555d8b3b00df10484df1c7e6e9a5b7ff))
- *download*: File titles outside your library under `__external__` ([#69](https://github.com/mkb79/audible-rs/pull/69)) ([7fa5427](https://github.com/mkb79/audible-rs/commit/7fa5427b237a90cc464fe7af4b28eedb1f3b53b9))
- *cover*: Any cover size without a request, plus `native` for the largest ([#63](https://github.com/mkb79/audible-rs/pull/63)) ([44c7883](https://github.com/mkb79/audible-rs/commit/44c7883a816567eba15d458ac1465d089fda51c4))
- *setup*: Group the wizard into sections and stop asking about disabled features ([#62](https://github.com/mkb79/audible-rs/pull/62)) ([e45818f](https://github.com/mkb79/audible-rs/commit/e45818f5600a23d95b2be3b75253651ebda1702e))
- *library*: 'episodes <SHOW> --missing' lists which episodes are still missing ([#60](https://github.com/mkb79/audible-rs/pull/60)) ([66c383b](https://github.com/mkb79/audible-rs/commit/66c383be383992858be98d4086ce55b9eb282510))
- *download*: Expand podcast shows to episodes, or skip with --exclude-podcasts ([#58](https://github.com/mkb79/audible-rs/pull/58)) ([d60a553](https://github.com/mkb79/audible-rs/commit/d60a5534267976ff9c9217fd54417acca80a1f86))
- *windows*: Add a PowerShell installer (install.ps1) and a Windows-aware upgrade hint ([#57](https://github.com/mkb79/audible-rs/pull/57)) ([dbe0ce1](https://github.com/mkb79/audible-rs/commit/dbe0ce1c62694d157affa680ce4441c23682514a))

### Fixed

- *library*: Serve the local library when offline ([#95](https://github.com/mkb79/audible-rs/pull/95)) ([d247f32](https://github.com/mkb79/audible-rs/commit/d247f32058e9a7d08d08cbb48aa41be23cad1db4))
- *auth*: A live agent no longer rolls back CLI-side auth changes ([#86](https://github.com/mkb79/audible-rs/pull/86)) ([a3cb52d](https://github.com/mkb79/audible-rs/commit/a3cb52d56a368b683ea27b398022b3d8cdb5b61c))
- *agent*: Agent stop no longer signals a recycled PID ([#85](https://github.com/mkb79/audible-rs/pull/85)) ([d68c047](https://github.com/mkb79/audible-rs/commit/d68c04734a78aeea542ef15a06d930f72e95a025))
- *library*: Honest input handling and summaries ([#84](https://github.com/mkb79/audible-rs/pull/84)) ([df67daf](https://github.com/mkb79/audible-rs/commit/df67daf86044495ad6b33568591d10a571425444))
- *login*: A second config-page submit no longer strands the sign-in ([#83](https://github.com/mkb79/audible-rs/pull/83)) ([f2e85bc](https://github.com/mkb79/audible-rs/commit/f2e85bc2b01a70bee1eccae9bef9f378847868a4))
- *download*: Honest artifact labels in summaries and download info ([#82](https://github.com/mkb79/audible-rs/pull/82)) ([a9a69da](https://github.com/mkb79/audible-rs/commit/a9a69dac047a5a25f592ca4d65d5b0de90b9dc38))
- *annotations*: A failed --save counts as a failure ([#81](https://github.com/mkb79/audible-rs/pull/81)) ([abbebc7](https://github.com/mkb79/audible-rs/commit/abbebc7b679ae9e6d3cb2f57e5657fbdfe050774))
- *api*: --save-request saves the request that was actually sent ([#80](https://github.com/mkb79/audible-rs/pull/80)) ([95a72d3](https://github.com/mkb79/audible-rs/commit/95a72d322f3fa6f45a0d501d9b5cc37723f7cd0b))
- *download*: Distinct titles no longer overwrite each other under --jobs ([#79](https://github.com/mkb79/audible-rs/pull/79)) ([1ef7b1f](https://github.com/mkb79/audible-rs/commit/1ef7b1fddc391d92472bf565b4168ba4f69ab9e8))
- *sync*: Episode resolution no longer drops episodes when the announced count is missing ([#78](https://github.com/mkb79/audible-rs/pull/78)) ([0c119c4](https://github.com/mkb79/audible-rs/commit/0c119c42701a5347bf0741d897eac5b2ee590e5d))
- *agent*: Honest HTTP statuses and fail-closed selectors on /v1 ([#77](https://github.com/mkb79/audible-rs/pull/77)) ([e8f5ed8](https://github.com/mkb79/audible-rs/commit/e8f5ed8ce6f7bd4d8a19724e951e4e452dfe3d83))
- *library*: Survive malformed Retry-After headers during sync retries ([#75](https://github.com/mkb79/audible-rs/pull/75)) ([57c4983](https://github.com/mkb79/audible-rs/commit/57c498344681d06b8b4431a8161631a041491d0a))
- *download*: Download orphans no longer deletes resume version markers ([#74](https://github.com/mkb79/audible-rs/pull/74)) ([c1f69c0](https://github.com/mkb79/audible-rs/commit/c1f69c0c044a02b6381c315d113fd0453284ca56))
- *download*: Stop announcing work that never happens ([#71](https://github.com/mkb79/audible-rs/pull/71)) ([70d0bd2](https://github.com/mkb79/audible-rs/commit/70d0bd2e9dd171fc87fedb34c750c828f3fc039e))
- *audit*: Rectify the 2026-07-17 codebase audit (AUD-220) ([#72](https://github.com/mkb79/audible-rs/pull/72)) ([449d36d](https://github.com/mkb79/audible-rs/commit/449d36d375e77f107a9c284526efdaaa09f05121))
- **BREAKING:** *db*: `db library remove` no longer touches your downloads ([#70](https://github.com/mkb79/audible-rs/pull/70)) ([bdbcb7f](https://github.com/mkb79/audible-rs/commit/bdbcb7f9d343a353afe794de240011bef1392a41))
- *download*: Refuse an `--asin` that is not in your library ([#68](https://github.com/mkb79/audible-rs/pull/68)) ([d2c115f](https://github.com/mkb79/audible-rs/commit/d2c115f1618db18f1322d6525e1b2b3f3c4ea65a))
- *reorganize*: A returned title's files keep their names ([#67](https://github.com/mkb79/audible-rs/pull/67)) ([d775fec](https://github.com/mkb79/audible-rs/commit/d775feca559bffef6d1a0c98303e47f7344f969b))
- *download*: A configured decrypt no longer widens `--kind` ([#66](https://github.com/mkb79/audible-rs/pull/66)) ([37f283c](https://github.com/mkb79/audible-rs/commit/37f283cc3fba4b674085fa337057d31cdf1134fc))
- *cover*: Resolve every cover size for podcast episodes, `native` included ([#64](https://github.com/mkb79/audible-rs/pull/64)) ([1d474a3](https://github.com/mkb79/audible-rs/commit/1d474a3613b11ef519b5a8e57ac090cb22f8d2a0))
- *pdf*: Report and fetch a PDF only for titles that actually have one ([#61](https://github.com/mkb79/audible-rs/pull/61)) ([2a456f6](https://github.com/mkb79/audible-rs/commit/2a456f61101469b34971f83fa6526d7d3fc0ab51))
- *podcasts*: A completed show is no longer "missing"; --missing now fetches missing episodes ([#59](https://github.com/mkb79/audible-rs/pull/59)) ([82b5edb](https://github.com/mkb79/audible-rs/commit/82b5edbc97a7d17dabd17b52ee8a221847baba8f))

### Performance

- *sync*: Store every cover size for podcast episodes ([#65](https://github.com/mkb79/audible-rs/pull/65)) ([ae3e249](https://github.com/mkb79/audible-rs/commit/ae3e249f0e2a85b73e1208807c148d8f22e93813))

### Security

- *security*: Create the passwords file owner-only from the first byte ([#76](https://github.com/mkb79/audible-rs/pull/76)) ([8e6fcb1](https://github.com/mkb79/audible-rs/commit/8e6fcb138c957cdb56e6212fa849488f4d30d2ae))

## [0.1.0-alpha.7](https://github.com/mkb79/audible-rs/compare/v0.1.0-alpha.6..v0.1.0-alpha.7) - 2026-07-15

### Added

- *release*: Publish a Windows x86_64 binary (.zip) ([#55](https://github.com/mkb79/audible-rs/pull/55)) ([9ca93ba](https://github.com/mkb79/audible-rs/commit/9ca93ba5e571be7756c568395bbe1fb375d81585))
- *windows*: Point to winget/aaxclean-cli when no decrypt tool is found ([#53](https://github.com/mkb79/audible-rs/pull/53)) ([eb896ef](https://github.com/mkb79/audible-rs/commit/eb896ef6be4b1b0719c7c4ee27fab9d7a3e5314b))

### Fixed

- *naming*: Drop the stray space after a separator replaced with "_" ([#54](https://github.com/mkb79/audible-rs/pull/54)) ([3c889ac](https://github.com/mkb79/audible-rs/commit/3c889ac0295436132b9a2bfcab6f3064e7c88f1e))
- *windows*: Find .exe tools via PATHEXT and use native path separators ([#52](https://github.com/mkb79/audible-rs/pull/52)) ([1e3242a](https://github.com/mkb79/audible-rs/commit/1e3242a082c44702b912196dcaec857cf2d21d1a))

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

