//! Integration tests for Authenticator loading and write-back, using the
//! synthetic legacy fixtures and freshly written new-format files.

use audible_rs::auth::authfile::{self, KdfParams, Protection};
use audible_rs::auth::{AccountOrigin, Authenticator};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

const WEAK_PARAMS: KdfParams = KdfParams {
    m_cost: 1024,
    t_cost: 1,
    p_cost: 1,
};

#[derive(Deserialize)]
struct Meta {
    password: String,
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path)
        .unwrap_or_else(|_| panic!("missing {path} — run scripts/gen_fixtures.py once"))
}

fn fixture_password() -> SecretString {
    let meta: Meta = serde_json::from_slice(&fixture("legacy_meta.json")).unwrap();
    SecretString::from(meta.password)
}

/// New-format auth data derived from the legacy plain fixture.
fn new_format_data() -> serde_json::Value {
    let legacy: serde_json::Value = serde_json::from_slice(&fixture("legacy_plain.json")).unwrap();
    serde_json::json!({
        "country_code": legacy["locale_code"],
        "identity": { "customer_id": legacy["customer_info"]["user_id"], "account_pool": "Amazon" },
        "signing": {
            "adp_token": legacy["adp_token"],
            "device_private_key": legacy["device_private_key"],
        },
        "bearer": {
            "access_token": legacy["access_token"],
            "refresh_token": legacy["refresh_token"],
            "expires": legacy["expires"],
        },
    })
}

#[tokio::test]
async fn imports_legacy_fixture_files() {
    let dir = tempfile::tempdir().unwrap();

    for (name, password) in [
        ("legacy_plain.json", None),
        ("legacy_encrypted_json.json", Some(fixture_password())),
        ("legacy_encrypted_bytes.bin", Some(fixture_password())),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, fixture(name)).unwrap();
        let auth = Authenticator::import_file(&path, password)
            .await
            .unwrap_or_else(|e| panic!("importing {name} failed: {e}"));
        assert_eq!(auth.locale().country_code, "de");
        assert_eq!(auth.origin(), AccountOrigin::Amazon);
        assert!(auth.signer().is_some(), "{name} carries signing material");
    }
}

#[tokio::test]
async fn load_file_rejects_legacy_files_with_import_hint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_plain.json");
    std::fs::write(&path, fixture("legacy_plain.json")).unwrap();

    let error = Authenticator::load_file(&path, None).await.unwrap_err();
    assert!(
        error.to_string().contains("account import"),
        "error must point to the import command: {error}"
    );
}

#[tokio::test]
async fn loads_new_format_encrypted_file_and_saves_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("account.authfile");
    let password = SecretString::from("test-password");

    let content = authfile::write(
        &new_format_data(),
        Protection::Encrypted(WEAK_PARAMS),
        Some(&password),
    )
    .unwrap();
    std::fs::write(&path, content).unwrap();

    let mut auth = Authenticator::load_file(&path, Some(password.clone()))
        .await
        .unwrap();
    assert!(auth.signer().is_some());

    auth.apply_token_refresh(SecretString::from("Atna|refreshed-token"), 9999999999.0);
    auth.save().await.unwrap();

    // The rewritten file must decrypt with the same password, keep the
    // weak KDF parameters and contain the new token.
    let reloaded =
        authfile::read(&std::fs::read_to_string(&path).unwrap(), Some(&password)).unwrap();
    assert_eq!(reloaded.protection, Protection::Encrypted(WEAK_PARAMS));
    assert_eq!(
        reloaded.data["bearer"]["access_token"],
        "Atna|refreshed-token"
    );
    assert_eq!(reloaded.data["bearer"]["expires"], 9999999999.0);
}

#[tokio::test]
async fn loads_new_format_plain_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("account.authfile");
    let content = authfile::write(&new_format_data(), Protection::Plain, None).unwrap();
    std::fs::write(&path, content).unwrap();

    let auth = Authenticator::load_file(&path, None).await.unwrap();
    assert_eq!(
        auth.access_token().unwrap().expose_secret(),
        new_format_data()["bearer"]["access_token"]
            .as_str()
            .unwrap()
    );
}

#[tokio::test]
async fn imported_file_is_never_written_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_plain.json");
    let original = fixture("legacy_plain.json");
    std::fs::write(&path, &original).unwrap();

    // import_file never configures a write-back: save() is a no-op.
    let mut auth = Authenticator::import_file(&path, None).await.unwrap();
    auth.apply_token_refresh(SecretString::from("Atna|refreshed"), 9999999999.0);
    auth.save().await.unwrap();

    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "import input files must stay untouched"
    );
}

#[tokio::test]
async fn save_to_converts_legacy_to_new_format() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_path = dir.path().join("legacy.json");
    let new_path = dir.path().join("converted.authfile");
    std::fs::write(&legacy_path, fixture("legacy_encrypted_json.json")).unwrap();

    // The account import flow: load legacy, save as new format with a
    // fresh password.
    let auth = Authenticator::import_file(&legacy_path, Some(fixture_password()))
        .await
        .unwrap();
    let new_password = SecretString::from("brand-new-password");
    auth.save_to(&new_path, Some(new_password.clone()), WEAK_PARAMS)
        .await
        .unwrap();

    let converted = Authenticator::load_file(&new_path, Some(new_password))
        .await
        .unwrap();
    assert!(converted.signer().is_some());
    assert_eq!(converted.locale().country_code, "de");
    assert_eq!(converted.origin(), AccountOrigin::Amazon);

    let expected: serde_json::Value =
        serde_json::from_slice(&fixture("legacy_plain.json")).unwrap();
    assert_eq!(
        converted.access_token().unwrap().expose_secret(),
        expected["access_token"].as_str().unwrap()
    );

    // The converted file has a write-back target: a refresh persists.
    let mut converted = converted;
    converted.apply_token_refresh(SecretString::from("Atna|persisted"), 9999999999.0);
    converted.save().await.unwrap();
    let reloaded = authfile::read(
        &std::fs::read_to_string(&new_path).unwrap(),
        Some(&SecretString::from("brand-new-password")),
    )
    .unwrap();
    assert_eq!(reloaded.data["bearer"]["access_token"], "Atna|persisted");
}

#[tokio::test]
async fn save_to_without_password_writes_plain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.authfile");
    let auth = Authenticator::from_value(new_format_data()).unwrap();
    auth.save_to(&path, None, WEAK_PARAMS).await.unwrap();

    let loaded = authfile::read(&std::fs::read_to_string(&path).unwrap(), None).unwrap();
    assert_eq!(loaded.protection, Protection::Plain);
    assert_eq!(loaded.data["country_code"], "de");
}
