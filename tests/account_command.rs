//! Integration tests for `audible account list` and `remove` against a
//! temp config dir, seeded through the import flow.

use audible_rs::commands::Command as _;
use audible_rs::commands::account::AccountCommand;
use audible_rs::config::Config;
use audible_rs::config::ctx::{Ctx, Selectors};

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

async fn seed_account(dir: &std::path::Path, name: &str) {
    let legacy = dir.join("input-legacy.json");
    std::fs::write(&legacy, fixture("legacy_plain.json")).unwrap();
    run(
        dir,
        &[
            "import",
            legacy.to_str().unwrap(),
            "--name",
            name,
            "--plain",
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn list_runs_with_and_without_accounts() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["list"]).await.unwrap();
    seed_account(dir.path(), "alice").await;
    run(dir.path(), &["list"]).await.unwrap();
}

#[tokio::test]
async fn remove_deletes_account_and_clears_default() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;

    run(dir.path(), &["remove", "alice", "--yes"])
        .await
        .unwrap();

    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert!(config.accounts.is_empty());
    assert!(config.default_account.is_none());
    // The auth file is kept by default.
    assert!(dir.path().join("alice.auth").exists());
}

#[tokio::test]
async fn remove_can_delete_the_auth_file() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;

    run(
        dir.path(),
        &["remove", "alice", "--yes", "--delete-auth-file"],
    )
    .await
    .unwrap();
    assert!(!dir.path().join("alice.auth").exists());
}

#[tokio::test]
async fn remove_keeps_other_accounts_untouched() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;
    seed_account(dir.path(), "bob").await;

    run(dir.path(), &["remove", "bob", "--yes"]).await.unwrap();

    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert!(config.accounts.contains_key("alice"));
    // alice was the first import and stays the default account.
    assert_eq!(config.default_account.as_deref(), Some("alice"));
    assert!(!config.accounts.contains_key("bob"));
}

#[tokio::test]
async fn remove_requires_confirmation_or_yes() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;

    // Test processes have no TTY: without --yes this must refuse.
    let error = run(dir.path(), &["remove", "alice"])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("--yes"), "unhelpful error: {error}");
}

#[tokio::test]
async fn remove_unknown_account_fails() {
    let dir = tempfile::tempdir().unwrap();
    let error = run(dir.path(), &["remove", "ghost", "--yes"])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ghost"));
}

#[tokio::test]
async fn rename_updates_default_account_and_keeps_auth_file() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;

    run(dir.path(), &["rename", "alice", "alice-de"])
        .await
        .unwrap();

    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert!(config.accounts.contains_key("alice-de"));
    assert!(!config.accounts.contains_key("alice"));
    // alice was the first import and the default; the pointer follows.
    assert_eq!(config.default_account.as_deref(), Some("alice-de"));
    // The auth file is left in place; the entry keeps pointing at it.
    assert_eq!(
        config.accounts["alice-de"].auth_file,
        std::path::PathBuf::from("alice.auth")
    );
    assert!(dir.path().join("alice.auth").exists());
}

#[tokio::test]
async fn rename_rejects_unknown_and_existing_names() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;
    seed_account(dir.path(), "bob").await;

    // Unknown source account.
    let error = run(dir.path(), &["rename", "ghost", "x"])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ghost"), "{error}");

    // Target name already taken.
    let error = run(dir.path(), &["rename", "alice", "bob"])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("already exists"), "{error}");
}

#[tokio::test]
async fn marketplaces_add_default_flag() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;

    // --default adds to both the available and the default set.
    run(dir.path(), &["marketplaces", "add", "us", "--default"])
        .await
        .unwrap();
    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    let account = &config.accounts["alice"];
    assert!(account.marketplaces.contains(&"us".to_owned()));
    assert!(account.default_marketplaces.contains(&"us".to_owned()));

    // Without the flag, only the available set grows; the default is left.
    run(dir.path(), &["marketplaces", "add", "uk"])
        .await
        .unwrap();
    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    let account = &config.accounts["alice"];
    assert!(account.marketplaces.contains(&"uk".to_owned()));
    assert!(!account.default_marketplaces.contains(&"uk".to_owned()));
}

#[tokio::test]
async fn import_sets_marketplaces_and_default() {
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
            "--marketplaces",
            "de,us",
            "--default-marketplaces",
            "de,us",
        ],
    )
    .await
    .unwrap();
    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    let account = &config.accounts["alice"];
    assert_eq!(account.marketplaces, vec!["de", "us"]);
    assert_eq!(account.default_marketplaces, vec!["de", "us"]);

    // --default-marketplaces must be a subset of --marketplaces.
    let dir2 = tempfile::tempdir().unwrap();
    let legacy2 = dir2.path().join("input-legacy.json");
    std::fs::write(&legacy2, fixture("legacy_plain.json")).unwrap();
    let error = run(
        dir2.path(),
        &[
            "import",
            legacy2.to_str().unwrap(),
            "--name",
            "alice",
            "--plain",
            "--marketplaces",
            "de",
            "--default-marketplaces",
            "us",
        ],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("not in --marketplaces"), "{error}");
}

#[tokio::test]
async fn export_python_round_trips_through_the_legacy_reader() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;
    let out = dir.path().join("export-python.json");

    run(
        dir.path(),
        &[
            "export",
            "--out",
            out.to_str().unwrap(),
            "--format",
            "python",
        ],
    )
    .await
    .unwrap();

    // Flat Python layout, readable by the legacy import path.
    let raw = std::fs::read_to_string(&out).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(value.get("locale_code").is_some());
    assert!(value.get("with_username").is_some());
    // Unknown customer_info fields survive the legacy import (AUD-80): the
    // fixture's given_name must still be present after import + export.
    assert_eq!(value["customer_info"]["given_name"], "Fixture");
    let back = audible_rs::auth::Authenticator::from_legacy_value(value).unwrap();
    assert_eq!(
        back.locale().country_code,
        audible_rs::auth::Authenticator::load_file(&dir.path().join("alice.auth"), None)
            .await
            .unwrap()
            .locale()
            .country_code
    );

    // Owner-only permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "export must be 0600, got {mode:o}");
    }
}

#[tokio::test]
async fn export_plain_is_a_loadable_new_format_file() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;
    let out = dir.path().join("export-plain.auth");

    run(
        dir.path(),
        &[
            "export",
            "--out",
            out.to_str().unwrap(),
            "--format",
            "plain",
        ],
    )
    .await
    .unwrap();

    // The export is a regular new-format auth file.
    let back = audible_rs::auth::Authenticator::load_file(&out, None)
        .await
        .unwrap();
    assert!(back.refresh_token().is_some());
}

#[tokio::test]
async fn export_refuses_existing_files_without_force() {
    let dir = tempfile::tempdir().unwrap();
    seed_account(dir.path(), "alice").await;
    let out = dir.path().join("export.json");
    std::fs::write(&out, b"precious").unwrap();

    let error = run(
        dir.path(),
        &[
            "export",
            "--out",
            out.to_str().unwrap(),
            "--format",
            "python",
        ],
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
    assert_eq!(std::fs::read(&out).unwrap(), b"precious");

    // --force overwrites.
    run(
        dir.path(),
        &[
            "export",
            "--out",
            out.to_str().unwrap(),
            "--format",
            "python",
            "--force",
        ],
    )
    .await
    .unwrap();
    assert_ne!(std::fs::read(&out).unwrap(), b"precious");
}
