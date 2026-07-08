//! Cryptographic building blocks: Argon2id and XChaCha20-Poly1305 for the
//! auth file envelope, AES-CBC and PBKDF2 for the legacy import and the
//! Python-compatible export.
//!
//! Everything in here is synchronous and CPU-bound; async callers must
//! wrap these functions in `tokio::task::spawn_blocking`.

use aes::{Aes128, Aes256};
use argon2::{Algorithm, Argon2, Params, Version};
use cbc::cipher::block_padding::{NoPadding, Pkcs7};
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use sha2::Sha256;
use zeroize::Zeroizing;

/// Length of all symmetric keys used here (AES-256, XChaCha20).
pub const KEY_LEN: usize = 32;
/// XChaCha20-Poly1305 nonce length.
pub const XCHACHA_NONCE_LEN: usize = 24;

/// Errors from the crypto primitives.
///
/// Failure causes are deliberately not detailed: a failed decryption must
/// not reveal whether the password was wrong or the data corrupted, and no
/// error ever contains key or plaintext material.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// Key derivation failed (invalid parameters or internal error).
    #[error("key derivation failed")]
    KeyDerivation,
    /// Encryption failed.
    #[error("encryption failed")]
    Encrypt,
    /// Decryption failed — wrong password or corrupted data.
    #[error("decryption failed (wrong password or corrupted data)")]
    Decrypt,
}

/// Derives a 32-byte key from a password with Argon2id (v1.3).
///
/// `m_cost` is in KiB, matching the `argon2` crate and the envelope
/// header (archived architecture §6 defaults: m=65536 KiB, t=3, p=4).
pub fn argon2id_derive(
    password: &[u8],
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, CryptoError> {
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|_| CryptoError::KeyDerivation)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon2
        .hash_password_into(password, salt, key.as_mut())
        .map_err(|_| CryptoError::KeyDerivation)?;
    Ok(key)
}

/// Derives a 32-byte key with PBKDF2-HMAC-SHA256 (legacy Python format).
pub fn pbkdf2_sha256_derive(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Zeroizing<[u8; KEY_LEN]> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, key.as_mut());
    key
}

/// Encrypts with XChaCha20-Poly1305, binding `aad` into the tag.
pub fn xchacha20poly1305_encrypt(
    key: &[u8; KEY_LEN],
    nonce: &[u8; XCHACHA_NONCE_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .encrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)
}

/// Decrypts XChaCha20-Poly1305 data, authenticating `aad` along with it.
pub fn xchacha20poly1305_decrypt(
    key: &[u8; KEY_LEN],
    nonce: &[u8; XCHACHA_NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

/// Decrypts AES-128-CBC data without padding (aaxc voucher).
///
/// The ciphertext length must be a multiple of the 16-byte block size;
/// the caller strips any trailing NUL padding from the plaintext.
pub fn aes128_cbc_decrypt_nopad(
    key: &[u8; 16],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let decryptor =
        cbc::Decryptor::<Aes128>::new_from_slices(key, iv).map_err(|_| CryptoError::Decrypt)?;
    decryptor
        .decrypt_padded_vec_mut::<NoPadding>(ciphertext)
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}

/// Decrypts AES-256-CBC data with PKCS#7 padding (legacy Python format).
pub fn aes256_cbc_decrypt_pkcs7(
    key: &[u8; KEY_LEN],
    iv: &[u8; 16],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let decryptor =
        cbc::Decryptor::<Aes256>::new_from_slices(key, iv).map_err(|_| CryptoError::Decrypt)?;
    decryptor
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::Decrypt)
}
