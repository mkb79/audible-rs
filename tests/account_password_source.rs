//! Integration test for `account password source` and the passwords-file
//! write-back. One sequential test function: it manipulates process-wide
//! environment variables (the non-interactive password paths), so it must
//! be the only test in this binary.

use audible_rs::auth::Authenticator;
use audible_rs::commands::Command as _;
use audible_rs::commands::account::AccountCommand;
use audible_rs::config::ctx::{Ctx, Selectors};
use secrecy::SecretString;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("missing {path}"))
}

async fn run(dir: &std::path::Path, argv: &[&str]) -> anyhow::Result<()> {
    let command = AccountCommand;
    let mut full = vec!["account"];
    full.extend_from_slice(argv);
    let matches = command.clap().try_get_matches_from(full)?;
    let ctx = Ctx::with_dir(dir.to_path_buf(), Selectors::default())?;
    command.run(&ctx, &matches).await
}

fn passwords(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("passwords")).unwrap_or_default()
}

#[tokio::test]
async fn password_source_file_write_back_lifecycle() {
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

    // Encrypt the account with pw1.
    unsafe { std::env::set_var("AUDIBLE_NEW_AUTH_PASSWORD", "pw1") };
    run(dir.path(), &["password", "set"]).await.unwrap();

    // Move it onto the passwords file (current password from the env
    // fallback). The entry is written and the source is recorded.
    unsafe { std::env::set_var("AUDIBLE_AUTH_PASSWORD", "pw1") };
    run(dir.path(), &["password", "source", "file"])
        .await
        .unwrap();

    assert!(
        passwords(dir.path()).lines().any(|l| l == "alice pw1"),
        "entry written: {:?}",
        passwords(dir.path())
    );
    let config = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(config.contains("password_source = \"file\""), "{config}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.path().join("passwords"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    // With no env fallback, changing the password must resolve the current
    // one from the passwords file and write the new one back.
    unsafe {
        std::env::remove_var("AUDIBLE_AUTH_PASSWORD");
        std::env::set_var("AUDIBLE_NEW_AUTH_PASSWORD", "pw2");
    }
    run(dir.path(), &["password", "set"]).await.unwrap();
    assert!(
        passwords(dir.path()).lines().any(|l| l == "alice pw2"),
        "entry updated: {:?}",
        passwords(dir.path())
    );
    Authenticator::load_file(
        dir.path().join("alice.auth"),
        Some(SecretString::from("pw2")),
    )
    .await
    .expect("auth opens with the changed password");

    // Switching away from file removes the entry.
    run(dir.path(), &["password", "source", "prompt"])
        .await
        .unwrap();
    assert!(
        !passwords(dir.path())
            .lines()
            .any(|l| l.starts_with("alice ")),
        "entry removed: {:?}",
        passwords(dir.path())
    );

    unsafe {
        std::env::remove_var("AUDIBLE_NEW_AUTH_PASSWORD");
    }
}
