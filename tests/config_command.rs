//! Integration tests for `audible config` against a temp config dir.

use audible_rs::commands::Command as _;
use audible_rs::commands::config::ConfigCommand;
use audible_rs::config::Config;
use audible_rs::config::ctx::{Ctx, Selectors};

async fn run(dir: &std::path::Path, argv: &[&str]) -> anyhow::Result<()> {
    let command = ConfigCommand;
    let mut full = vec!["config"];
    full.extend_from_slice(argv);
    let matches = command.clap().try_get_matches_from(full)?;
    let ctx = Ctx::with_dir(dir.to_path_buf(), Selectors::default())?;
    command.run(&ctx, &matches).await
}

#[tokio::test]
async fn set_get_unset_roundtrip() {
    let dir = tempfile::tempdir().unwrap();

    run(
        dir.path(),
        &["set", "settings.default.download_dir", "/audio"],
    )
    .await
    .unwrap();
    run(dir.path(), &["set", "db.page_size", "250"])
        .await
        .unwrap();
    run(dir.path(), &["get", "settings.default.download_dir"])
        .await
        .unwrap();
    run(dir.path(), &["list"]).await.unwrap();

    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert_eq!(
        config.settings["default"].download_dir.as_deref(),
        Some(std::path::Path::new("/audio"))
    );
    assert_eq!(config.db.page_size, 250);

    run(dir.path(), &["unset", "settings.default.download_dir"])
        .await
        .unwrap();
    let config = Config::load(&dir.path().join("config.toml")).unwrap();
    assert!(
        config
            .settings
            .get("default")
            .is_none_or(|s| s.download_dir.is_none())
    );
}

#[tokio::test]
async fn invalid_values_never_reach_disk() {
    let dir = tempfile::tempdir().unwrap();
    run(
        dir.path(),
        &["set", "settings.default.download_dir", "/audio"],
    )
    .await
    .unwrap();
    let before = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();

    // Unknown key, type mismatch, dangling reference, bad duration.
    for (key, value) in [
        ("settings.default.download_dirr", "/x"),
        ("db.page_size", "many"),
        ("default_account", "ghost"),
        ("db.sync_max_age", "soon"),
    ] {
        assert!(
            run(dir.path(), &["set", key, value]).await.is_err(),
            "set {key}={value} should fail"
        );
    }

    let after = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert_eq!(before, after, "file must be untouched after rejected sets");
}

#[tokio::test]
async fn get_unset_key_fails() {
    let dir = tempfile::tempdir().unwrap();
    let error = run(dir.path(), &["get", "default_account"])
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not set"));
}
