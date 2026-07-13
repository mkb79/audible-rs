# audible-rs

[![Project status: WIP](https://www.repostatus.org/badges/latest/wip.svg)](https://www.repostatus.org/#wip)
[![Version](https://img.shields.io/github/v/release/mkb79/audible-rs?include_prereleases&label=version)](https://github.com/mkb79/audible-rs/releases)
[![CI](https://github.com/mkb79/audible-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mkb79/audible-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A command-line tool and Rust library for your own Audible account — a
ground-up reimplementation of [`mkb79/Audible`](https://github.com/mkb79/Audible)
and [`mkb79/audible-cli`](https://github.com/mkb79/audible-cli) as a single
Rust crate. It installs one binary, `audible`.

> [!WARNING]
> **Alpha — no stability guarantee.** Alpha releases are tagged (see
> [Releases](https://github.com/mkb79/audible-rs/releases)), but commands,
> flags, the config file and the on-disk database format may change at any
> time **without a migration path** (before the first stable release the
> database is recreated rather than migrated). Use it on a throwaway config
> first, and expect breakage.

## What it does

- **Accounts** — sign in (scripted, external-browser, or a local
  browser-proxy with QR for headless boxes), import a legacy
  `audible`/`audible-cli` auth file, manage the auth-file password and
  its source, deregister.
- **Library** — sync your library into a local SQLite database (full and
  delta), list, full-text search, export (JSON/CSV), review the change
  log, browse series and podcasts.
- **Download** — owned titles as `aaxc` with resume, plus chapters,
  cover and PDF; optional lossless decrypt to a playable `m4b`.
- **Collections** — wishlist and archive (list, add, remove).
- **Raw API** — send authenticated requests to the Audible API for
  anything the higher-level commands don't cover.
- **Plugins & agent** *(Unix only for now)* — a capability-scoped plugin
  system and a resident session agent for backend/web-frontend use.

## Install

Linux and macOS are supported (x86-64 and arm64). Windows support for the
core commands is in progress; the plugin and agent subsystems are Unix-only.

### Prebuilt binary (recommended)

Download and install the latest release binary for your platform:

```sh
curl -fsSL https://raw.githubusercontent.com/mkb79/audible-rs/main/install.sh | sh
```

It installs `audible` into `~/.local/bin` (override with `--bin-dir <dir>`)
and verifies the download against the release checksums. By default it
installs the latest **stable** release; while the project is in alpha (no
stable release yet) it installs the newest pre-release, and once a stable
release exists you can pass `--pre` to keep tracking pre-releases.

audible-rs is the successor to `audible-cli` and shares the command name
`audible`. If you already have `audible-cli` installed, the installer asks
before replacing its command (pass `--force` to skip, or `--bin-dir` to
install elsewhere); the config directories are separate, so `audible-cli`'s
data is left untouched. Replacing an older audible-rs is a silent upgrade.

### Manual download

Grab the archive for your target from the
[Releases](https://github.com/mkb79/audible-rs/releases) page, verify it,
and place the binary on your `PATH`:

| Platform | Asset |
| --- | --- |
| Linux x86-64 | `audible-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `audible-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `audible-<version>-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `audible-<version>-aarch64-apple-darwin.tar.gz` |

The Linux binaries are statically linked (musl) and run on any distribution.
Verify the download against `SHA256SUMS` from the same release:

```sh
sha256sum -c SHA256SUMS --ignore-missing   # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing  # macOS
```

### From source

Requires a **Rust toolchain** and a **C compiler** (the bundled SQLite and
the TLS backend build from source):

```sh
cargo install --git https://github.com/mkb79/audible-rs
# or, from a clone:
cargo build --release   # binary at target/release/audible
```

### Optional tools for `download --decrypt`

`audible download --decrypt` needs one of these on `PATH` (or pointed at via
`AUDIBLE_FFMPEG` / `AUDIBLE_AAXCLEAN_CLI`):

- **ffmpeg** (≥ 4.4), or
- **[aaxclean-cli](https://github.com/Mbucari/aaxclean-cli) by Mbucari** —
  purpose-built and noticeably faster.

## Getting started

```sh
audible setup                 # 1. one-time interactive defaults
audible account login -m de   # 2. register an account (pick your marketplace)
audible library sync          # 3. pull your library into the local database
audible library list          #    …then work with it
```

1. **`audible setup`** — run once after installing; it configures
   installation-wide defaults interactively.
2. **Add an account** — either register a fresh one with
   `audible account login` (pick the marketplace with `-m`, e.g. `-m de`,
   `-m us`, `-m uk`), or bring one over from the Python tools with
   `audible account import <file>`.
3. **`audible library sync` first** — every library command reads from a
   **local database**, so you must sync at least once before `list`,
   `search`, `download --missing`, `series`, `library episodes` etc. return
   anything. Run it again whenever your library changes (new purchases,
   returns); it does an incremental delta sync when it can.

Most commands accept `-o table|json|plain` to choose the output format,
and global selectors `-a/--account`, `-m/--marketplace`,
`-s/--settings`. Run `audible --help` for the full command tree, or
`audible <command> --help` for any subcommand.

## Shell completions

Quickest — `audible completions --install` writes the script to the right
directory for your shell (bash, zsh or fish; detected from `$SHELL`), then
open a new shell:

```sh
audible completions --install          # current shell
audible completions bash --install     # or name a shell
```

(zsh: the target dir must be on your `$fpath`.) To place it yourself
instead, print the script and redirect it where your shell looks:

```sh
audible completions bash > ~/.local/share/bash-completion/completions/audible
audible completions zsh  > ~/.local/share/zsh/site-functions/_audible
audible completions fish > ~/.config/fish/completions/audible.fish
```

`audible completions <shell>` also covers `powershell` and `elvish`
(print-and-redirect only). Or let the installer set it up in one step:
`install.sh --completions` runs `--install` for the shells it finds.

## Commands

Commands are grouped by noun; each has subcommands (`audible <noun>
--help`):

| Command | What it covers |
| --- | --- |
| `setup` | One-time interactive defaults. |
| `account` | Sign in / import / logout, marketplaces, password, cookies, token, activation bytes, Widevine CDM, export. |
| `settings` | Reusable settings bundles (download options, filename scheme, …). |
| `config` | Get/set/unset raw configuration values. |
| `library` | `sync`, `list` (with `--kind book,podcast,episode`), `search`, `episodes`, `export`, `changes`, `add`, `remove`. |
| `series` | Series volumes, including the ones you are missing. |
| `download` | Download owned titles (audio/chapter/cover/pdf), decrypt, reorganize, orphans, info. |
| `collections` | Wishlist and archive (`list`, `add`, `remove`). |
| `annotations` | Bookmarks, notes, clips and last-position. |
| `api` | Send raw authenticated requests to the Audible API. |
| `db` | Maintain the local database (backup/restore, vacuum, integrity, downloads bookkeeping). |
| `plugin` / `agent` | Plugin discovery and the resident session agent *(Unix only)*. |

## Where things live

`audible` follows platform conventions (and honours `XDG_*` on Linux).
Set `AUDIBLE_CONFIG_DIR` to override the config location (useful for
throwaway setups).

| | Linux / macOS | Windows |
| --- | --- | --- |
| Config + auth files (`config.toml`, `*.auth`) | `~/.config/audible` | `%APPDATA%\audible` |
| Data — library database, downloads | `~/.local/share/audible` | `%LOCALAPPDATA%\audible` |

The library database lives under the data directory in `db/`. There is
one SQLite file **per Audible identity (`user_id`)**, not per config
account or per marketplace — so two accounts that resolve to the same
`user_id`, or the same account across several marketplaces, all share
one database (marketplace is a column, not a separate file). Downloads
default to `downloads/` under the data directory unless you set a
`download_dir`.

## Security

- Auth material is stored in an encrypted envelope (Argon2id +
  XChaCha20-Poly1305) by default; an unencrypted mode exists but is not
  recommended.
- Credentials never appear in logs, errors or command output at any
  verbosity level.
- The tool only ever touches **your own** account and the content you
  own.

## Documentation

This README is a starting overview. Fuller documentation will live under
`docs/` as the project matures; until then, `--help` on any command is
the authoritative reference.

Changes between releases are tracked in [CHANGELOG.md](CHANGELOG.md)
(Keep a Changelog format). The file — like the GitHub release notes — is
generated from the commit history with [git-cliff](https://git-cliff.org),
so both always match; don't edit it by hand.

## License

MIT — see [LICENSE](LICENSE).

## Disclaimer

This project is **not affiliated with, endorsed by, or connected to
Audible or Amazon**. "Audible" is a trademark of its respective owner.
It is an independent tool for accessing **your own** Audible account and
**your own** purchased content; use it in accordance with Audible's terms
of service and the laws that apply to you.
