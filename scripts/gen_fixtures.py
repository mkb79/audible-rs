#!/usr/bin/env python3
"""Generate synthetic golden-test fixtures for audible-rs.

This script uses the Python reference implementation (mkb79/Audible) to
produce fixtures for the Rust golden tests:

* ``tests/fixtures/signing.json`` — request-signing test vectors: a
  throwaway RSA key, a made-up adp_token, a fixed timestamp and the
  headers produced by ``audible.auth.sign_request`` for several requests.
* ``tests/fixtures/legacy_plain.json`` — synthetic auth data in the
  legacy (Python) auth-file layout, unencrypted.
* ``tests/fixtures/legacy_encrypted_json.json`` /
  ``tests/fixtures/legacy_encrypted_bytes.bin`` — the same data encrypted
  by ``audible.aescipher.AESCipher`` (PBKDF2 + AES-256-CBC) in both
  legacy encryption styles ("json" and "bytes").
* ``tests/fixtures/legacy_meta.json`` — password and file map for the
  legacy fixtures.

Security: everything here is THROWAWAY material — a freshly generated
RSA keypair and invented tokens. The script never reads existing auth
files and must never be pointed at real credentials. The RSA key and the
salts/IVs are random per run, so a re-run rewrites all fixtures as a new,
internally consistent set; signatures themselves are deterministic
(PKCS#1 v1.5 with a fixed timestamp). Run once and commit the result.

Usage:
    python3 -m venv scripts/.venv
    scripts/.venv/bin/pip install audible==0.10.0
    scripts/.venv/bin/python scripts/gen_fixtures.py
"""

from __future__ import annotations

import base64
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

try:
    import rsa

    import audible
    import audible.auth
except ImportError as exc:
    sys.exit(
        f"missing dependency: {exc}\n"
        "Create a venv and install the reference implementation first:\n"
        "  python3 -m venv scripts/.venv\n"
        "  scripts/.venv/bin/pip install audible==0.10.0\n"
        "  scripts/.venv/bin/python scripts/gen_fixtures.py"
    )

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURE_DIR = REPO_ROOT / "tests" / "fixtures"

PASSWORD = "fixture-password-1234"
FIXED_DT = datetime(2026, 1, 2, 3, 4, 5, 678901, tzinfo=timezone.utc)
# Exactly what audible.auth.sign_request computes from FIXED_DT.
TIMESTAMP = FIXED_DT.isoformat("T") + "Z"


def fake_adp_token() -> str:
    """Build an invented adp_token in the shape of a real one."""

    def b64(data: bytes) -> str:
        return base64.b64encode(data).decode()

    parts = {
        "enc": b64(b"synthetic adp token enc payload " * 8),
        "key": b64(b"synthetic adp token key payload " * 4),
        "iv": b64(b"synthetic iv payload"),
        "name": b64(b"ADPToken throwaway fixture - not a real token"),
        # The reference implementation validates this literal serial value.
        "serial": "Mg==",
    }
    return "".join(f"{{{k}:{v}}}" for k, v in parts.items())


def make_auth_data(private_key_pem: str, adp_token: str) -> dict:
    """Synthetic auth data matching Authenticator.to_dict() layout."""
    return {
        "website_cookies": {
            "session-id": "000-0000000-0000000",
            "ubid-acbde": "000-0000000-0000000",
            "x-acbde": '"synthetic-cookie-value"',
        },
        "adp_token": adp_token,
        "access_token": "Atna|SyntheticAccessTokenForFixturesOnly0123456789",
        "refresh_token": "Atnr|SyntheticRefreshTokenForFixturesOnly0123456789",
        "device_private_key": private_key_pem,
        "store_authentication_cookie": {
            "cookie": "synthetic|store|authentication|cookie"
        },
        "device_info": {
            "device_name": "Synthetic Fixture Device",
            "device_serial_number": "0123456789ABCDEF",
            "device_type": "A2CZJZGLK2JJVM",
        },
        "customer_info": {
            "account_pool": "Amazon",
            "user_id": "amzn1.account.SYNTHETICFIXTUREUSER",
            "home_region": "EU",
            "name": "Fixture User",
            "given_name": "Fixture",
        },
        "expires": 1767312000.0,
        "locale_code": "de",
        "with_username": False,
        "activation_bytes": None,
    }


SIGNING_CASES = [
    {
        "name": "get_simple",
        "method": "GET",
        "path": "/1.0/account/information",
        "body": "",
    },
    {
        "name": "get_with_query",
        "method": "GET",
        "path": "/1.0/library?response_groups=product_desc,pdf_url&num_results=999",
        "body": "",
    },
    {
        "name": "post_json_body",
        "method": "POST",
        "path": "/1.0/wishlist",
        "body": '{"asin": "B07RJZJ8L9"}',
    },
    {
        "name": "post_unicode_body",
        "method": "POST",
        "path": "/1.0/collections",
        "body": '{"name": "Hörbücher – Lieblingsstücke"}',
    },
    {
        "name": "delete",
        "method": "DELETE",
        "path": "/1.0/wishlist/B07RJZJ8L9",
        "body": "",
    },
]


class _FrozenDatetime:
    """Replacement for datetime inside audible.auth with a fixed clock."""

    @staticmethod
    def now(tz: timezone | None = None) -> datetime:
        return FIXED_DT if tz is None else FIXED_DT.astimezone(tz)


def gen_signing_fixture(private_key_pem: str, adp_token: str) -> None:
    audible.auth.datetime = _FrozenDatetime  # freeze sign_request's clock

    cases = []
    for case in SIGNING_CASES:
        headers = audible.auth.sign_request(
            method=case["method"],
            path=case["path"],
            body=case["body"].encode("utf-8"),
            adp_token=adp_token,
            private_key=private_key_pem,
        )
        assert headers["x-adp-signature"].endswith(f":{TIMESTAMP}")
        cases.append({**case, "expected_headers": headers})

    fixture = {
        "_comment": "Synthetic signing test vectors generated by "
        "scripts/gen_fixtures.py with a throwaway RSA key. Not real "
        "credentials.",
        "generator": f"audible=={audible.__version__}",
        "adp_token": adp_token,
        "device_private_key": private_key_pem,
        "timestamp": TIMESTAMP,
        "cases": cases,
    }
    path = FIXTURE_DIR / "signing.json"
    path.write_text(json.dumps(fixture, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {path} ({len(cases)} cases)")


def gen_legacy_fixtures(auth_data: dict) -> None:
    plain_path = FIXTURE_DIR / "legacy_plain.json"
    json_path = FIXTURE_DIR / "legacy_encrypted_json.json"
    bytes_path = FIXTURE_DIR / "legacy_encrypted_bytes.bin"
    meta_path = FIXTURE_DIR / "legacy_meta.json"

    auth = audible.Authenticator.from_dict(dict(auth_data))
    assert auth.to_dict() == auth_data

    auth.to_file(plain_path, encryption=False, set_default=False)
    auth.to_file(json_path, password=PASSWORD, encryption="json", set_default=False)
    auth.to_file(bytes_path, password=PASSWORD, encryption="bytes", set_default=False)

    # Round-trip: the reference implementation must read back its own files.
    for path in (json_path, bytes_path):
        reloaded = audible.Authenticator.from_file(path, password=PASSWORD)
        assert reloaded.to_dict() == auth_data, f"round-trip failed for {path}"
    reloaded = audible.Authenticator.from_file(plain_path)
    assert reloaded.to_dict() == auth_data

    meta = {
        "_comment": "Metadata for the synthetic legacy auth-file fixtures.",
        "generator": f"audible=={audible.__version__}",
        "password": PASSWORD,
        "files": {
            "plain": plain_path.name,
            "encrypted_json": json_path.name,
            "encrypted_bytes": bytes_path.name,
        },
        "kdf": "PBKDF2-HMAC-SHA256, iterations packed into the salt header",
        "cipher": "AES-256-CBC, PKCS#7 padding",
    }
    meta_path.write_text(json.dumps(meta, indent=2) + "\n")
    for path in (plain_path, json_path, bytes_path, meta_path):
        print(f"wrote {path}")


def gen_voucher_fixture() -> None:
    """Encrypt a known voucher so the Rust voucher-decrypt has a golden.

    Mirrors audible.aescipher._decrypt_voucher: the AES-128 key/iv are
    sha256(device_type + device_serial + customer_id + asin)[:16]/[16:],
    the inner {key, iv} JSON is AES-CBC encrypted without padding (NUL
    padded to the block size).
    """
    from hashlib import sha256

    from audible.aescipher import aes_cbc_encrypt

    device_type = "A2CZJZGLK2JJVM"
    device_serial = "0123456789ABCDEF"
    customer_id = "amzn1.account.SYNTHETICFIXTUREUSER"
    asin = "B0SYNTHETIC1"
    inner = {
        "key": "00112233445566778899aabbccddeeff",
        "iv": "ffeeddccbbaa99887766554433221100",
        "rules": [{"name": "DefaultExpiresRule", "parameters": []}],
    }

    digest = sha256((device_type + device_serial + customer_id + asin).encode("ascii")).digest()
    key, iv = digest[:16], digest[16:]

    plaintext = json.dumps(inner)
    padded = plaintext + "\x00" * (-len(plaintext) % 16)
    ciphertext = aes_cbc_encrypt(key, iv, padded, padding="none")
    voucher_b64 = base64.b64encode(ciphertext).decode()

    fixture = {
        "_comment": "Synthetic aaxc voucher generated by scripts/gen_fixtures.py.",
        "device_type": device_type,
        "device_serial": device_serial,
        "customer_id": customer_id,
        "asin": asin,
        "encrypted_voucher": voucher_b64,
        "expected": {"key": inner["key"], "iv": inner["iv"]},
    }
    path = FIXTURE_DIR / "voucher.json"
    path.write_text(json.dumps(fixture, indent=2) + "\n")
    print(f"wrote {path}")


def main() -> None:
    FIXTURE_DIR.mkdir(parents=True, exist_ok=True)

    print("generating throwaway RSA-2048 keypair ...")
    _, private_key = rsa.newkeys(2048)
    private_key_pem = private_key.save_pkcs1().decode("utf-8")
    adp_token = fake_adp_token()

    gen_signing_fixture(private_key_pem, adp_token)
    gen_legacy_fixtures(make_auth_data(private_key_pem, adp_token))
    gen_voucher_fixture()
    print("done")


if __name__ == "__main__":
    main()
