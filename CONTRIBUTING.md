# Contributing to audible-rs

Thanks for your interest in improving `audible-rs`! Contributions of all
kinds are welcome — bug reports, feature ideas, documentation fixes and
code.

## Before you start

- **Bugs and feature requests** go through [GitHub
  issues](https://github.com/mkb79/audible-rs/issues) — the templates ask
  for what we need to help you.
- **Larger changes:** please open an issue first so we can align on the
  approach before you invest time in a pull request.
- **Security issues:** never report them in public issues or PRs — see
  [SECURITY.md](SECURITY.md) for private reporting.

## Development setup

You need stable Rust (via [rustup](https://rustup.rs)) and a **C
toolchain** — SQLite is bundled and compiled from source (`cc`/`clang`
on Linux/macOS, the MSVC build tools on Windows).

```bash
cargo build
python3 scripts/gen_fixtures.py   # once: generates the golden-test fixtures
cargo test
```

Optional, only for working on `download --decrypt`:
[`aaxclean-cli`](https://crates.io/crates/aaxclean-cli) and/or `ffmpeg`.

## Quality gate

Every change must pass the same gate CI runs:

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Tests run **exclusively against synthetic fixtures**
(`tests/fixtures/`, generated with throwaway keys). Never point tests at
a real account, real auth files or a real library.

## Commits and pull requests

- We use **[Conventional Commits](https://www.conventionalcommits.org)**
  (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`, …).
- PRs are **squash-merged**: the PR title becomes the commit subject —
  and for `feat`/`fix`/`perf`, the user-facing changelog entry
  (the changelog is generated with git-cliff, never edited by hand). So
  phrase the title the way a user should read it.
- Everything committed is **English**: code, comments, doc comments,
  tests, commit messages. (Foreign-language *test data* is fine where it
  covers real i18n behavior.)
- Keep PRs focused — one concern per PR.

## Security ground rules

`audible-rs` handles real account credentials, so a few rules are
non-negotiable:

- **Never commit credentials** — auth files (`*.auth`), tokens, cookies,
  passwords files, Widevine keys (`*.wvd`). Stage files explicitly;
  a gitleaks check runs in CI.
- **No secrets in logs or error messages**, at any verbosity level.
- **No real account or library data** in code, tests, issues or PR
  texts — no real account names, book/podcast titles, ASINs or library
  counts. Use invented examples like `B0EXAMPLE1` or `alice`/`bob`.

## License

By contributing you agree that your contributions are licensed under the
project's [MIT license](LICENSE) (inbound = outbound).
