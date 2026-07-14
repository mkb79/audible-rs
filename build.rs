//! Build script.
//!
//! 1. Generates Rust types for the Widevine license protocol (AUD-56) from
//!    `proto/license_protocol.proto`. Uses `protox` (a pure-Rust protobuf
//!    compiler) so no native `protoc` is required. Only `prost` (the small
//!    runtime) is linked into the binary; the codegen tooling here is
//!    build-time only.
//! 2. Emits `AUDIBLE_BUILD_VERSION`, the version the binary reports — the
//!    crate version, plus the commit when the build is not a published
//!    release (AUD-180).

use std::path::Path;
use std::process::Command;

fn main() {
    let proto = "proto/license_protocol.proto";
    let descriptors = protox::compile([proto], ["proto"]).expect("compile license_protocol.proto");
    prost_build::Config::new()
        .compile_fds(descriptors)
        .expect("generate prost types");
    println!("cargo:rerun-if-changed={proto}");

    emit_build_version();
}

/// Emits `AUDIBLE_BUILD_VERSION` for `env!` at compile time.
///
/// A binary built from source between two releases would otherwise report the
/// last release's version and pass for that release — `self check` would say
/// "up to date", and a bug report would name a version its author is not
/// running. Such a build therefore carries its commit as SemVer build
/// metadata (`0.1.0-alpha.4+g1a2b3c4`). Only SemVer *precedence* ignores that
/// metadata (`Version::cmp_precedence`), while the derived `==`/`<` do not —
/// which is why `self` selects releases by precedence.
///
/// Deliberately no "dirty" marker: cargo reruns this script when tracked
/// inputs change, which unstaged edits need not touch — a staleness-prone
/// flag claiming a clean tree would be worse than no flag at all.
fn emit_build_version() {
    let base = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    println!("cargo:rustc-env=AUDIBLE_BUILD_VERSION={}", version(&base));

    println!("cargo:rerun-if-env-changed=AUDIBLE_RELEASE_TAG");
    // Without these the commit is baked in once and goes stale on the next
    // checkout. A path that does not exist would force a rerun on every
    // build, so only track what is actually there.
    for path in [".git/HEAD", ".git/packed-refs"] {
        if Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(reference) = std::fs::read_to_string(".git/HEAD").ok().and_then(|head| {
        head.strip_prefix("ref: ")
            .map(|r| format!(".git/{}", r.trim()))
    }) && Path::new(&reference).exists()
    {
        println!("cargo:rerun-if-changed={reference}");
    }
}

/// The version to report: bare for a release build, `+g<commit>` otherwise.
fn version(base: &str) -> String {
    // `AUDIBLE_RELEASE_TAG` is authoritative, and the release workflow sets
    // it. Git alone would not do: the workflow's build job checks out shallow
    // and without tags, so every release binary would look like a dev build.
    if let Ok(tag) = std::env::var("AUDIBLE_RELEASE_TAG") {
        let tag = tag.trim();
        if !tag.is_empty() {
            let tagged = tag.strip_prefix('v').unwrap_or(tag);
            // A mismatch means the release pipeline is broken (tag and
            // Cargo.toml disagree). Fail loudly rather than ship a binary
            // that misnames itself.
            assert!(
                tagged == base,
                "AUDIBLE_RELEASE_TAG is {tag}, but the crate version is {base} — \
                 refusing to build a release binary that misreports its version"
            );
            return base.to_owned();
        }
    }

    // No git — a source tarball or `cargo install`. Nothing to add, and a
    // tarball of a release tag is exactly what that looks like.
    let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return base.to_owned();
    };
    // A checkout sitting exactly on this version's tag is a release build
    // after all — someone rebuilding the release from source.
    let tags = git(&["tag", "--points-at", "HEAD"]).unwrap_or_default();
    if tags
        .lines()
        .map(str::trim)
        .any(|tag| tag.strip_prefix('v').unwrap_or(tag) == base)
    {
        return base.to_owned();
    }
    format!("{base}+g{commit}")
}

/// Runs a git command. `None` if git is absent, this is not a repository, or
/// the command fails — each of which simply means "no commit known".
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
