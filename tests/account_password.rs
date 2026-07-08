//! Integration test for `account password set|remove`. One sequential
//! test function: it manipulates process-wide environment variables for
//! the non-interactive password paths, so steps must not run in
//! parallel (this file is its own test binary).

use audible_rs::auth::Authenticator;
use audible_rs::commands::Command as _;
use audible_rs::commands::account::AccountCommand;
use audible_rs::config::ctx::{Ctx, Selectors};
use secrecy::SecretString;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path)
        .unwrap_or_else(|_| panic!("missing {path} — run scripts/gen_fixtures.py once"))
}

async fn run(dir: &std::path::Path, argv: &[&str]) -> anyhow::Result<()> {
    let command = AccountCommand;
    let mut full = vec!["account"];
    full.extend_from_slice(argv);
    let matches = command.clap().try_get_matches_from(full)?;
    let ctx = Ctx::with_dir(dir.to_path_buf(), Selectors::default())?;
    command.run(&ctx, &matches).await
}

/// On-disk protection, read from the format marker only.
fn protection(dir: &std::path::Path) -> &'static str {
    let content = std::fs::read(dir.join("alice.auth")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&content).unwrap();
    if value.get("ciphertext").is_some() {
        "encrypted"
    } else {
        "plain"
    }
}

#[tokio::test]
async fn password_set_and_remove_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("input-legacy.json");
    std::fs::write(&legacy, fixture("legacy_plain.json")).unwrap();
    run(
        dir.path(),
        &[
            "import",
            legacy.to_str().unwrap(),
            "--name",
            "alice",
            "--plain",
        ],
    )
    .await
    .unwrap();
    assert_eq!(protection(dir.path()), "plain");

    // SAFETY: this test binary runs this single test; no concurrent
    // env access.
    unsafe { std::env::set_var("AUDIBLE_NEW_AUTH_PASSWORD", "geheim-1") };
    run(dir.path(), &["password", "set"]).await.unwrap();
    assert_eq!(protection(dir.path()), "encrypted");
    Authenticator::load_file(
        dir.path().join("alice.auth"),
        Some(SecretString::from("geheim-1")),
    )
    .await
    .expect("file must open with the new password");

    // Change the password: current from env, new from env.
    unsafe {
        std::env::set_var("AUDIBLE_AUTH_PASSWORD", "geheim-1");
        std::env::set_var("AUDIBLE_NEW_AUTH_PASSWORD", "geheim-2");
    }
    run(dir.path(), &["password", "set"]).await.unwrap();
    Authenticator::load_file(
        dir.path().join("alice.auth"),
        Some(SecretString::from("geheim-2")),
    )
    .await
    .expect("file must open with the changed password");

    // Remove encryption (current password now geheim-2).
    unsafe { std::env::set_var("AUDIBLE_AUTH_PASSWORD", "geheim-2") };
    run(dir.path(), &["password", "remove", "--yes"])
        .await
        .unwrap();
    assert_eq!(protection(dir.path()), "plain");
    let auth = Authenticator::load_file(dir.path().join("alice.auth"), None)
        .await
        .unwrap();
    assert!(
        auth.signer().is_some(),
        "auth material survives the round trip"
    );

    // remove on an already plain file is an error.
    let error = run(dir.path(), &["password", "remove", "--yes"])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("already"));

    // Unknown account (selected via -a) is rejected.
    {
        let command = AccountCommand;
        let matches = command
            .clap()
            .try_get_matches_from(["account", "password", "set"])
            .unwrap();
        let ctx = Ctx::with_dir(
            dir.path().to_path_buf(),
            Selectors {
                account: Some("ghost".into()),
                ..Selectors::default()
            },
        )
        .unwrap();
        assert!(command.run(&ctx, &matches).await.is_err());
    }

    unsafe {
        std::env::remove_var("AUDIBLE_AUTH_PASSWORD");
        std::env::remove_var("AUDIBLE_NEW_AUTH_PASSWORD");
    }
}
