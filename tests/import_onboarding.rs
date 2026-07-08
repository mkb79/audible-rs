//! Integration test for the account-based flow: `account import`
//! registers the account (with its marketplace axis) in the config, and
//! Ctx resolves it back to a working authenticator. Synthetic fixtures
//! only.

use audible_rs::commands::Command as _;
use audible_rs::commands::account::AccountCommand;
use audible_rs::config::Config;
use audible_rs::config::ctx::{Ctx, Selectors};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path)
        .unwrap_or_else(|_| panic!("missing {path} — run scripts/gen_fixtures.py once"))
}

async fn run_import(config_dir: &std::path::Path, extra: &[&str]) -> anyhow::Result<()> {
    let legacy = config_dir.join("input-legacy.json");
    std::fs::write(&legacy, fixture("legacy_plain.json")).unwrap();

    let mut argv = vec![
        "account",
        "import",
        legacy.to_str().unwrap(),
        "--name",
        "alice",
        "--plain", // no password prompts in tests
    ];
    argv.extend_from_slice(extra);

    let command = AccountCommand;
    let matches = command.clap().try_get_matches_from(argv)?;
    let ctx = Ctx::with_dir(config_dir.to_path_buf(), Selectors::default())?;
    command.run(&ctx, &matches).await
}

#[tokio::test]
async fn import_registers_account_marketplaces_and_default() {
    let dir = tempfile::tempdir().unwrap();
    run_import(dir.path(), &[]).await.unwrap();

    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(
        config.accounts["alice"].auth_file,
        std::path::PathBuf::from("alice.auth")
    );
    // The fixture registers in `de`, which seeds the marketplace axis.
    assert_eq!(config.accounts["alice"].marketplaces, vec!["de"]);
    assert_eq!(config.accounts["alice"].default_marketplaces, vec!["de"]);
    assert_eq!(config.default_account.as_deref(), Some("alice"));
    assert!(dir.path().join("alice.auth").exists());

    // A fresh Ctx resolves the account back to working auth material.
    let ctx = Ctx::with_dir(dir.path().to_path_buf(), Selectors::default()).unwrap();
    assert_eq!(ctx.account_name().unwrap(), "alice");
    assert_eq!(ctx.marketplaces().unwrap(), vec!["de"]);
    let auth = ctx.authenticator().await.unwrap();
    assert!(auth.signer().is_some());
    assert_eq!(auth.locale().country_code, "de");
    // And the client builds for that account.
    ctx.client().await.unwrap();
}

#[tokio::test]
async fn import_with_custom_marketplaces() {
    let dir = tempfile::tempdir().unwrap();
    run_import(dir.path(), &["--marketplaces", "us,de"])
        .await
        .unwrap();

    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(config.accounts["alice"].marketplaces, vec!["us", "de"]);
    // The first entry is the account's default marketplace.
    assert_eq!(config.accounts["alice"].default_marketplaces, vec!["us"]);
    assert_eq!(config.default_account.as_deref(), Some("alice"));
}

#[tokio::test]
async fn reimport_requires_force() {
    let dir = tempfile::tempdir().unwrap();
    run_import(dir.path(), &[]).await.unwrap();

    let error = run_import(dir.path(), &[]).await.unwrap_err().to_string();
    assert!(error.contains("--force"), "unhelpful error: {error}");

    run_import(dir.path(), &["--force"]).await.unwrap();
}

#[tokio::test]
async fn unknown_account_override_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    run_import(dir.path(), &[]).await.unwrap();

    let selectors = Selectors {
        account: Some("nope".into()),
        ..Selectors::default()
    };
    let ctx = Ctx::with_dir(dir.path().to_path_buf(), selectors).unwrap();
    let error = ctx.account_name().unwrap_err().to_string();
    assert!(error.contains("nope"));
}
