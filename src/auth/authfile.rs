//! Encrypted auth file envelope (archived architecture §6).
//!
//! The envelope is a JSON document whose header (everything except the
//! ciphertext) is authenticated as associated data:
//!
//! ```json
//! {
//!   "format": "audible-rs-auth",
//!   "version": 1,
//!   "kdf": {
//!     "algorithm": "argon2id",
//!     "m_cost": 65536,
//!     "t_cost": 3,
//!     "p_cost": 4,
//!     "salt": "<base64, 16 bytes>"
//!   },
//!   "cipher": {
//!     "algorithm": "xchacha20poly1305",
//!     "nonce": "<base64, 24 bytes, fresh per write>"
//!   },
//!   "ciphertext": "<base64>"
//! }
//! ```
//!
//! The AAD is a canonical string derived from the header fields (see
//! [`aad_string`]), so any header manipulation fails authentication. KDF
//! parameters live in the header, which makes later hardening of the
//! defaults a non-breaking change. Unencrypted auth files are plain JSON
//! without this envelope.
//!
//! Argon2id is CPU- and memory-heavy by design; async callers must wrap
//! [`encrypt`]/[`decrypt`] in `tokio::task::spawn_blocking`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{self, CryptoError, XCHACHA_NONCE_LEN};

/// Marker in the `format` field of the envelope.
pub const ENVELOPE_FORMAT: &str = "audible-rs-auth";
/// Current envelope version.
pub const ENVELOPE_VERSION: u32 = 1;

const KDF_ALGORITHM: &str = "argon2id";
const CIPHER_ALGORITHM: &str = "xchacha20poly1305";
const SALT_LEN: usize = 16;

/// Argon2id parameters stored in the envelope header.
///
/// `m_cost` is in KiB. The defaults are the archived architecture §6 values:
/// m=64 MiB, t=3, p=4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory cost in KiB.
    pub m_cost: u32,
    /// Number of iterations.
    pub t_cost: u32,
    /// Degree of parallelism.
    pub p_cost: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_cost: 64 * 1024,
            t_cost: 3,
            p_cost: 4,
        }
    }
}

/// Errors raised while reading or writing an envelope.
#[derive(Debug, thiserror::Error)]
pub enum AuthFileError {
    /// The file is not a JSON envelope.
    #[error("malformed auth file envelope: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The `format` field does not identify an audible-rs auth envelope.
    #[error("unsupported auth file format {0:?}")]
    UnsupportedFormat(String),
    /// The envelope version is newer than this build understands.
    #[error("unsupported auth file version {0}")]
    UnsupportedVersion(u32),
    /// The KDF or cipher algorithm is not supported.
    #[error("unsupported algorithm {0:?} in auth file")]
    UnsupportedAlgorithm(String),
    /// A base64 field or a salt/nonce length is invalid.
    #[error("invalid field encoding in auth file envelope")]
    InvalidEncoding,
    /// The file is encrypted but no password was supplied.
    #[error("auth file is encrypted; a password is required")]
    PasswordRequired,
    /// Decryption succeeded but the plaintext is not a JSON document.
    #[error("auth file plaintext is not valid JSON")]
    InvalidPlaintext,
    /// Key derivation, encryption or decryption failed.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

/// How an auth file is protected on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// Unencrypted plain envelope.
    Plain,
    /// Encrypted envelope with these KDF parameters.
    Encrypted(KdfParams),
}

/// Result of [`read`]: the contained auth data plus how the file was
/// protected, so a write-back can preserve the protection.
#[derive(Debug)]
pub struct LoadedAuthFile {
    /// The decoded auth data.
    pub data: serde_json::Value,
    /// Protection found on disk.
    pub protection: Protection,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: String,
    version: u32,
    kdf: KdfHeader,
    cipher: CipherHeader,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct KdfHeader {
    algorithm: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: String,
}

#[derive(Serialize, Deserialize)]
struct CipherHeader {
    algorithm: String,
    nonce: String,
}

/// Unencrypted variant: same format marker, the auth data inline.
#[derive(Serialize, Deserialize)]
struct PlainEnvelope {
    format: String,
    version: u32,
    data: serde_json::Value,
}

/// Canonical AAD representation of the envelope header.
///
/// This string — not the JSON serialization, which is formatting
/// dependent — is what the AEAD authenticates. It must never change for
/// version 1 envelopes.
fn aad_string(envelope: &Envelope) -> String {
    format!(
        "{}\nv{}\n{}\nm={},t={},p={}\nsalt={}\n{}\nnonce={}",
        envelope.format,
        envelope.version,
        envelope.kdf.algorithm,
        envelope.kdf.m_cost,
        envelope.kdf.t_cost,
        envelope.kdf.p_cost,
        envelope.kdf.salt,
        envelope.cipher.algorithm,
        envelope.cipher.nonce,
    )
}

/// Encrypts auth data into an envelope with the default KDF parameters.
pub fn encrypt(plaintext: &[u8], password: &SecretString) -> Result<String, AuthFileError> {
    encrypt_with_params(plaintext, password, KdfParams::default())
}

/// Encrypts auth data into an envelope, using a fresh salt and nonce.
pub fn encrypt_with_params(
    plaintext: &[u8],
    password: &SecretString,
    params: KdfParams,
) -> Result<String, AuthFileError> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let mut nonce = [0u8; XCHACHA_NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);

    let mut envelope = Envelope {
        format: ENVELOPE_FORMAT.to_owned(),
        version: ENVELOPE_VERSION,
        kdf: KdfHeader {
            algorithm: KDF_ALGORITHM.to_owned(),
            m_cost: params.m_cost,
            t_cost: params.t_cost,
            p_cost: params.p_cost,
            salt: BASE64.encode(salt),
        },
        cipher: CipherHeader {
            algorithm: CIPHER_ALGORITHM.to_owned(),
            nonce: BASE64.encode(nonce),
        },
        ciphertext: String::new(),
    };

    let key = crypto::argon2id_derive(
        password.expose_secret().as_bytes(),
        &salt,
        params.m_cost,
        params.t_cost,
        params.p_cost,
    )?;
    let ciphertext = crypto::xchacha20poly1305_encrypt(
        &key,
        &nonce,
        aad_string(&envelope).as_bytes(),
        plaintext,
    )?;
    envelope.ciphertext = BASE64.encode(ciphertext);

    Ok(serde_json::to_string_pretty(&envelope)?)
}

/// Decrypts an envelope produced by [`encrypt`].
///
/// The returned plaintext is zeroized on drop.
pub fn decrypt(
    envelope_json: &str,
    password: &SecretString,
) -> Result<Zeroizing<Vec<u8>>, AuthFileError> {
    let envelope: Envelope = serde_json::from_str(envelope_json)?;

    if envelope.format != ENVELOPE_FORMAT {
        return Err(AuthFileError::UnsupportedFormat(envelope.format));
    }
    if envelope.version != ENVELOPE_VERSION {
        return Err(AuthFileError::UnsupportedVersion(envelope.version));
    }
    if envelope.kdf.algorithm != KDF_ALGORITHM {
        return Err(AuthFileError::UnsupportedAlgorithm(envelope.kdf.algorithm));
    }
    if envelope.cipher.algorithm != CIPHER_ALGORITHM {
        return Err(AuthFileError::UnsupportedAlgorithm(
            envelope.cipher.algorithm,
        ));
    }

    let salt: [u8; SALT_LEN] = BASE64
        .decode(&envelope.kdf.salt)
        .map_err(|_| AuthFileError::InvalidEncoding)?
        .try_into()
        .map_err(|_| AuthFileError::InvalidEncoding)?;
    let nonce: [u8; XCHACHA_NONCE_LEN] = BASE64
        .decode(&envelope.cipher.nonce)
        .map_err(|_| AuthFileError::InvalidEncoding)?
        .try_into()
        .map_err(|_| AuthFileError::InvalidEncoding)?;
    let ciphertext = BASE64
        .decode(&envelope.ciphertext)
        .map_err(|_| AuthFileError::InvalidEncoding)?;

    let key = crypto::argon2id_derive(
        password.expose_secret().as_bytes(),
        &salt,
        envelope.kdf.m_cost,
        envelope.kdf.t_cost,
        envelope.kdf.p_cost,
    )?;
    let plaintext = crypto::xchacha20poly1305_decrypt(
        &key,
        &nonce,
        aad_string(&envelope).as_bytes(),
        &ciphertext,
    )?;

    Ok(plaintext)
}

/// Reads an auth file (plain or encrypted envelope), returning the auth
/// data and the protection found on disk.
pub fn read(
    content: &str,
    password: Option<&SecretString>,
) -> Result<LoadedAuthFile, AuthFileError> {
    let value: serde_json::Value = serde_json::from_str(content)?;
    let format = value
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if format != ENVELOPE_FORMAT {
        return Err(AuthFileError::UnsupportedFormat(format.to_owned()));
    }

    if value.get("ciphertext").is_some() {
        let password = password.ok_or(AuthFileError::PasswordRequired)?;
        // decrypt re-validates format, version and algorithms.
        let plaintext = decrypt(content, password)?;
        let data =
            serde_json::from_slice(&plaintext).map_err(|_| AuthFileError::InvalidPlaintext)?;
        let envelope: Envelope = serde_json::from_str(content)?;
        let params = KdfParams {
            m_cost: envelope.kdf.m_cost,
            t_cost: envelope.kdf.t_cost,
            p_cost: envelope.kdf.p_cost,
        };
        Ok(LoadedAuthFile {
            data,
            protection: Protection::Encrypted(params),
        })
    } else {
        let plain: PlainEnvelope = serde_json::from_str(content)?;
        if plain.version != ENVELOPE_VERSION {
            return Err(AuthFileError::UnsupportedVersion(plain.version));
        }
        Ok(LoadedAuthFile {
            data: plain.data,
            protection: Protection::Plain,
        })
    }
}

/// Serializes auth data into file content with the given protection.
///
/// Encrypted writes use a fresh salt and nonce every time.
pub fn write(
    data: &serde_json::Value,
    protection: Protection,
    password: Option<&SecretString>,
) -> Result<String, AuthFileError> {
    match protection {
        Protection::Plain => Ok(serde_json::to_string_pretty(&PlainEnvelope {
            format: ENVELOPE_FORMAT.to_owned(),
            version: ENVELOPE_VERSION,
            data: data.clone(),
        })?),
        Protection::Encrypted(params) => {
            let password = password.ok_or(AuthFileError::PasswordRequired)?;
            let plaintext = Zeroizing::new(serde_json::to_vec(data)?);
            encrypt_with_params(&plaintext, password, params)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn password() -> SecretString {
        SecretString::from("correct horse battery staple")
    }

    // Weak parameters keep most tests fast; the defaults are exercised in
    // roundtrip_with_default_params.
    const TEST_PARAMS: KdfParams = KdfParams {
        m_cost: 1024,
        t_cost: 1,
        p_cost: 1,
    };

    const PLAINTEXT: &[u8] = br#"{"adp_token": "synthetic", "expires": 1.0}"#;

    #[test]
    fn roundtrip_with_default_params() {
        let envelope = encrypt(PLAINTEXT, &password()).unwrap();
        let decrypted = decrypt(&envelope, &password()).unwrap();
        assert_eq!(decrypted.as_slice(), PLAINTEXT);
    }

    #[test]
    fn roundtrip_with_custom_params() {
        let envelope = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let decrypted = decrypt(&envelope, &password()).unwrap();
        assert_eq!(decrypted.as_slice(), PLAINTEXT);
    }

    #[test]
    fn envelope_header_contains_declared_parameters() {
        let envelope = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        assert_eq!(value["format"], ENVELOPE_FORMAT);
        assert_eq!(value["version"], ENVELOPE_VERSION);
        assert_eq!(value["kdf"]["algorithm"], "argon2id");
        assert_eq!(value["kdf"]["m_cost"], 1024);
        assert_eq!(value["cipher"]["algorithm"], "xchacha20poly1305");
        let salt = BASE64
            .decode(value["kdf"]["salt"].as_str().unwrap())
            .unwrap();
        assert_eq!(salt.len(), SALT_LEN);
        let nonce = BASE64
            .decode(value["cipher"]["nonce"].as_str().unwrap())
            .unwrap();
        assert_eq!(nonce.len(), XCHACHA_NONCE_LEN);
    }

    #[test]
    fn fresh_salt_and_nonce_per_write() {
        let a = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let b = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let a: serde_json::Value = serde_json::from_str(&a).unwrap();
        let b: serde_json::Value = serde_json::from_str(&b).unwrap();
        assert_ne!(a["kdf"]["salt"], b["kdf"]["salt"]);
        assert_ne!(a["cipher"]["nonce"], b["cipher"]["nonce"]);
    }

    #[test]
    fn wrong_password_fails() {
        let envelope = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let result = decrypt(&envelope, &SecretString::from("wrong password"));
        assert!(matches!(
            result,
            Err(AuthFileError::Crypto(CryptoError::Decrypt))
        ));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let envelope = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        let mut ciphertext = BASE64
            .decode(value["ciphertext"].as_str().unwrap())
            .unwrap();
        ciphertext[0] ^= 0x01;
        value["ciphertext"] = BASE64.encode(ciphertext).into();
        let result = decrypt(&value.to_string(), &password());
        assert!(matches!(
            result,
            Err(AuthFileError::Crypto(CryptoError::Decrypt))
        ));
    }

    #[test]
    fn tampered_header_fails() {
        // Weakening t_cost in the header must break decryption: the header
        // is bound as AAD (and feeds the KDF).
        let envelope = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        value["kdf"]["t_cost"] = 2.into();
        let result = decrypt(&value.to_string(), &password());
        assert!(matches!(
            result,
            Err(AuthFileError::Crypto(CryptoError::Decrypt))
        ));
    }

    #[test]
    fn read_write_plain_roundtrip() {
        let data = serde_json::json!({"adp_token": "synthetic", "expires": 1.0});
        let content = write(&data, Protection::Plain, None).unwrap();
        let loaded = read(&content, None).unwrap();
        assert_eq!(loaded.data, data);
        assert_eq!(loaded.protection, Protection::Plain);
    }

    #[test]
    fn read_write_encrypted_roundtrip_preserves_params() {
        let data = serde_json::json!({"adp_token": "synthetic", "expires": 1.0});
        let content = write(&data, Protection::Encrypted(TEST_PARAMS), Some(&password())).unwrap();
        let loaded = read(&content, Some(&password())).unwrap();
        assert_eq!(loaded.data, data);
        assert_eq!(loaded.protection, Protection::Encrypted(TEST_PARAMS));
    }

    #[test]
    fn read_encrypted_without_password_is_rejected() {
        let data = serde_json::json!({"adp_token": "synthetic"});
        let content = write(&data, Protection::Encrypted(TEST_PARAMS), Some(&password())).unwrap();
        assert!(matches!(
            read(&content, None),
            Err(AuthFileError::PasswordRequired)
        ));
    }

    #[test]
    fn read_rejects_foreign_json() {
        // A legacy Python auth file has no format marker.
        let result = read(r#"{"adp_token": "x", "expires": 1.0}"#, None);
        assert!(matches!(result, Err(AuthFileError::UnsupportedFormat(_))));
    }

    #[test]
    fn unknown_format_and_version_are_rejected() {
        let envelope = encrypt_with_params(PLAINTEXT, &password(), TEST_PARAMS).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        value["version"] = 99.into();
        assert!(matches!(
            decrypt(&value.to_string(), &password()),
            Err(AuthFileError::UnsupportedVersion(99))
        ));

        let mut value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        value["format"] = "something-else".into();
        assert!(matches!(
            decrypt(&value.to_string(), &password()),
            Err(AuthFileError::UnsupportedFormat(_))
        ));
    }
}
