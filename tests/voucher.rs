//! Golden test for aaxc voucher decryption against a fixture generated
//! by scripts/gen_fixtures.py with the Python reference's AES scheme.

use audible_rs::models::content::DownloadLicense;
use serde::Deserialize;

#[derive(Deserialize)]
struct VoucherFixture {
    device_type: String,
    device_serial: String,
    customer_id: String,
    asin: String,
    encrypted_voucher: String,
    expected: Expected,
}

#[derive(Deserialize)]
struct Expected {
    key: String,
    iv: String,
}

#[test]
fn decrypts_voucher_against_reference_fixture() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/voucher.json"
    ))
    .expect("missing tests/fixtures/voucher.json — run scripts/gen_fixtures.py once");
    let fixture: VoucherFixture = serde_json::from_str(&raw).unwrap();

    // Build a minimal granted license carrying the encrypted voucher.
    let license = DownloadLicense::from_response(serde_json::json!({
        "content_license": {
            "status_code": "Granted",
            "asin": fixture.asin,
            "drm_type": "Adrm",
            "license_response": fixture.encrypted_voucher,
            "content_metadata": {
                "content_url": {"offline_url": "https://x/y.aaxc"}
            }
        }
    }))
    .unwrap();

    let voucher = license
        .decrypt_voucher(
            &fixture.device_type,
            &fixture.device_serial,
            &fixture.customer_id,
        )
        .expect("voucher must decrypt");
    assert_eq!(voucher.key, fixture.expected.key);
    assert_eq!(voucher.iv, fixture.expected.iv);
    assert!(
        voucher.raw.get("rules").is_some(),
        "full document preserved"
    );
}

#[test]
fn wrong_device_data_fails_to_parse() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/voucher.json"
    ))
    .unwrap();
    let fixture: VoucherFixture = serde_json::from_str(&raw).unwrap();
    let license = DownloadLicense::from_response(serde_json::json!({
        "content_license": {
            "status_code": "Granted",
            "asin": fixture.asin,
            "license_response": fixture.encrypted_voucher,
            "content_metadata": {"content_url": {"offline_url": "https://x"}}
        }
    }))
    .unwrap();

    // A different device serial derives a different key → garbage plaintext.
    let result = license.decrypt_voucher(
        &fixture.device_type,
        "WRONGSERIAL00000",
        &fixture.customer_id,
    );
    assert!(result.is_err());
}
