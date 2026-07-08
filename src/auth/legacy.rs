//! Read-only import of legacy auth files written by the Python library
//! (`audible.aescipher`). Writing this format is handled by
//! `account export --format python`, not here.
//!
//! The legacy format comes in three variants:
//!
//! * **plain** — unencrypted JSON containing an `adp_token` key;
//! * **json** — a JSON object with base64 `salt`, `iv` and `ciphertext`;
//! * **bytes** — raw `salt(16) || iv(16) || ciphertext`.
//!
//! The key is PBKDF2-HMAC-SHA256 (32 bytes); the cipher AES-256-CBC with
//! PKCS#7 padding. The 16-byte salt field packs a header: the iteration
//! count as a big-endian u16 wrapped in `$` markers (`$..$` + 12 salt
//! bytes). Files without that marker use the whole field as salt and the
//! default of 1000 iterations, mirroring the Python fallback.
//!
//! PBKDF2 is CPU-bound; async callers must wrap [`read`] in
//! `tokio::task::spawn_blocking`.

use secrecy::{ExposeSecret, SecretString};

use crate::crypto::{self, CryptoError};

const BLOCK_SIZE: usize = 16;
const SALT_MARKER: u8 = b'$';
const DEFAULT_KDF_ITERATIONS: u32 = 1000;

/// Encryption variant of a legacy auth file, as detected from content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyEncryption {
    /// Unencrypted JSON.
    Plain,
    /// JSON object with base64 salt/iv/ciphertext fields.
    Json,
    /// Raw bytes: salt, iv and ciphertext concatenated.
    Bytes,
}

/// Errors raised while reading a legacy auth file.
#[derive(Debug, thiserror::Error)]
pub enum LegacyError {
    /// The file matches no known legacy layout.
    #[error("invalid legacy auth file")]
    Invalid,
    /// The file is encrypted but no password was supplied.
    #[error("legacy auth file is encrypted; a password is required")]
    PasswordRequired,
    /// Key derivation or decryption failed.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// Decryption succeeded formally but did not yield JSON — in
    /// practice a wrong password whose padding happened to validate.
    #[error("decrypted data is not valid JSON (wrong password?)")]
    InvalidPlaintext,
}

/// Detects the encryption variant, like Python's `detect_file_encryption`.
pub fn detect(content: &[u8]) -> LegacyEncryption {
    match serde_json::from_slice::<serde_json::Value>(content) {
        Ok(value) if value.get("adp_token").is_some() => LegacyEncryption::Plain,
        Ok(value) if value.get("ciphertext").is_some() => LegacyEncryption::Json,
        _ => LegacyEncryption::Bytes,
    }
}

/// Reads a legacy auth file and returns the contained auth data as JSON.
///
/// `password` is required for the encrypted variants. Mapping the legacy
/// fields onto the new account model is up to the caller
/// (`account import`).
pub fn read(
    content: &[u8],
    password: Option<&SecretString>,
) -> Result<serde_json::Value, LegacyError> {
    match detect(content) {
        LegacyEncryption::Plain => {
            serde_json::from_slice(content).map_err(|_| LegacyError::Invalid)
        }
        LegacyEncryption::Json => {
            let parts: EncryptedJson =
                serde_json::from_slice(content).map_err(|_| LegacyError::Invalid)?;
            decrypt_parts(
                &parts.decode_field(&parts.salt)?,
                &parts.decode_field(&parts.iv)?,
                &parts.decode_field(&parts.ciphertext)?,
                password,
            )
        }
        LegacyEncryption::Bytes => {
            if content.len() < 2 * BLOCK_SIZE {
                return Err(LegacyError::Invalid);
            }
            let (salt, rest) = content.split_at(BLOCK_SIZE);
            let (iv, ciphertext) = rest.split_at(BLOCK_SIZE);
            decrypt_parts(salt, iv, ciphertext, password)
        }
    }
}

#[derive(serde::Deserialize)]
struct EncryptedJson {
    salt: String,
    iv: String,
    ciphertext: String,
}

impl EncryptedJson {
    fn decode_field(&self, field: &str) -> Result<Vec<u8>, LegacyError> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(field)
            .map_err(|_| LegacyError::Invalid)
    }
}

/// Splits the iteration-count header off a packed salt field.
///
/// Mirrors Python's `unpack_salt` with the default `$` marker, including
/// its fallback: no marker means the whole field is the salt and the
/// default iteration count applies.
fn unpack_salt(packed: &[u8]) -> (&[u8], u32) {
    if packed.len() >= 4 && packed[0] == SALT_MARKER && packed[3] == SALT_MARKER {
        let iterations = u32::from(u16::from_be_bytes([packed[1], packed[2]]));
        (&packed[4..], iterations)
    } else {
        (packed, DEFAULT_KDF_ITERATIONS)
    }
}

fn decrypt_parts(
    packed_salt: &[u8],
    iv: &[u8],
    ciphertext: &[u8],
    password: Option<&SecretString>,
) -> Result<serde_json::Value, LegacyError> {
    let password = password.ok_or(LegacyError::PasswordRequired)?;
    let iv: &[u8; BLOCK_SIZE] = iv.try_into().map_err(|_| LegacyError::Invalid)?;
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(BLOCK_SIZE) {
        return Err(LegacyError::Invalid);
    }

    let (salt, iterations) = unpack_salt(packed_salt);
    let key = crypto::pbkdf2_sha256_derive(password.expose_secret().as_bytes(), salt, iterations);
    let plaintext = crypto::aes256_cbc_decrypt_pkcs7(&key, iv, ciphertext)?;
    serde_json::from_slice(&plaintext).map_err(|_| LegacyError::InvalidPlaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_salt_with_marker() {
        let mut packed = vec![b'$', 0x03, 0xe8, b'$']; // 1000 iterations
        packed.extend_from_slice(&[0xaa; 12]);
        let (salt, iterations) = unpack_salt(&packed);
        assert_eq!(iterations, 1000);
        assert_eq!(salt, &[0xaa; 12]);
    }

    #[test]
    fn unpack_salt_without_marker_falls_back() {
        let packed = [0xbb; 16];
        let (salt, iterations) = unpack_salt(&packed);
        assert_eq!(iterations, DEFAULT_KDF_ITERATIONS);
        assert_eq!(salt, &packed);
    }

    #[test]
    fn detect_variants() {
        assert_eq!(detect(br#"{"adp_token": "x"}"#), LegacyEncryption::Plain);
        assert_eq!(
            detect(br#"{"salt": "a", "iv": "b", "ciphertext": "c"}"#),
            LegacyEncryption::Json
        );
        assert_eq!(detect(&[0x00, 0xff, 0x10]), LegacyEncryption::Bytes);
    }
}
