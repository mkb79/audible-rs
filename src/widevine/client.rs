//! The native Widevine CDM client (AUD-56a): build the license challenge from
//! a PSSH and parse the license response into content keys. Reimplements the
//! pywidevine algorithm:
//!
//! * challenge signature — RSA-PSS over SHA-1 of the serialized `LicenseRequest`;
//! * session key — RSA-OAEP (SHA-1) unwrap of `SignedMessage.session_key`;
//! * key derivation — AES-CMAC over `\x01/\x02 + context` (session key as key);
//! * content keys — AES-128-CBC (PKCS7) with the derived encryption key.
//!
//! The device private key, the session key and the content keys are secrets:
//! zeroized where held and never logged.

use aes::Aes128;
use cbc::cipher::{BlockDecryptMut as _, KeyIvInit as _, block_padding::Pkcs7};
use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore as _;
use cmac::Cmac;
use hmac::{Hmac, Mac as _};
use prost::Message as _;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pss::SigningKey;
use rsa::signature::{RandomizedSigner as _, SignatureEncoding as _};
use rsa::{Oaep, RsaPrivateKey};
use sha1::Sha1;
use sha2::Sha256;
use zeroize::Zeroizing;

use super::device::Device;
use super::proto;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// Errors from the Widevine client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The device's RSA private key could not be parsed.
    #[error("invalid device private key")]
    PrivateKey,
    /// The device's client_id is not a valid ClientIdentification.
    #[error("invalid device client_id")]
    ClientId,
    /// The license message could not be decoded.
    #[error("could not decode the license message")]
    License,
    /// The message was not a LICENSE response.
    #[error("license server did not return a LICENSE message")]
    NotALicense,
    /// The session key could not be unwrapped.
    #[error("could not unwrap the session key")]
    SessionKey,
    /// Key derivation failed (unexpected key length).
    #[error("key derivation failed")]
    Derive,
    /// The license signature did not verify.
    #[error("license signature mismatch")]
    Signature,
    /// A content key could not be decrypted.
    #[error("content key decryption failed")]
    Decrypt,
}

/// A content decryption key from a parsed license.
pub struct ContentKey {
    /// The 16-byte key id.
    pub kid: [u8; 16],
    /// The AES content key. Secret — zeroized on drop, never logged.
    pub key: Zeroizing<Vec<u8>>,
}

/// A built license challenge plus the request bytes needed to parse the reply.
pub struct Challenge {
    /// The `SignedMessage` to POST to `drmlicense` (raw bytes; base64 on the wire).
    pub message: Vec<u8>,
    /// The serialized `LicenseRequest` (input to the key-derivation context).
    request: Vec<u8>,
}

/// A Widevine CDM instance bound to one device.
pub struct Cdm {
    private_key: RsaPrivateKey,
    /// Serialized `ClientIdentification` (opaque; embedded into the request).
    client_id: Vec<u8>,
}

impl Cdm {
    /// Builds a CDM from a parsed device blob.
    pub fn from_device(device: &Device) -> Result<Self, ClientError> {
        let private_key = RsaPrivateKey::from_pkcs1_der(device.private_key_der())
            .map_err(|_| ClientError::PrivateKey)?;
        Ok(Self {
            private_key,
            client_id: device.client_id().to_vec(),
        })
    }

    /// Builds a license challenge for a PSSH init-data blob. `offline` selects
    /// an OFFLINE (download) vs STREAMING license.
    pub fn challenge(
        &self,
        pssh_init_data: &[u8],
        offline: bool,
    ) -> Result<Challenge, ClientError> {
        let mut request_id = [0u8; 16];
        OsRng.fill_bytes(&mut request_id);
        let license_type = if offline {
            proto::LicenseType::Offline
        } else {
            proto::LicenseType::Streaming
        };
        let client_id = proto::ClientIdentification::decode(self.client_id.as_slice())
            .map_err(|_| ClientError::ClientId)?;

        use proto::license_request::content_identification::{ContentIdVariant, WidevinePsshData};
        let request = proto::LicenseRequest {
            client_id: Some(client_id),
            content_id: Some(proto::license_request::ContentIdentification {
                content_id_variant: Some(ContentIdVariant::WidevinePsshData(WidevinePsshData {
                    pssh_data: vec![pssh_init_data.to_vec()],
                    license_type: Some(license_type as i32),
                    request_id: Some(request_id.to_vec()),
                })),
            }),
            r#type: Some(proto::license_request::RequestType::New as i32),
            request_time: Some(time::OffsetDateTime::now_utc().unix_timestamp()),
            protocol_version: Some(proto::ProtocolVersion::Version21 as i32),
            key_control_nonce: Some(OsRng.next_u32() % (1 << 31) + 1),
            ..Default::default()
        };
        let request = request.encode_to_vec();

        // RSA-PSS over SHA-1 (salt length = digest length, matching the app).
        let signing_key = SigningKey::<Sha1>::new(self.private_key.clone());
        let mut rng = OsRng;
        let signature = signing_key.sign_with_rng(&mut rng, &request).to_vec();

        let signed = proto::SignedMessage {
            r#type: Some(proto::signed_message::MessageType::LicenseRequest as i32),
            msg: Some(request.clone()),
            signature: Some(signature),
            ..Default::default()
        };
        Ok(Challenge {
            message: signed.encode_to_vec(),
            request,
        })
    }

    /// Parses a license `SignedMessage` (from `drmlicense`) into content keys.
    pub fn parse_license(
        &self,
        challenge: &Challenge,
        license: &[u8],
    ) -> Result<Vec<ContentKey>, ClientError> {
        let signed = proto::SignedMessage::decode(license).map_err(|_| ClientError::License)?;
        if signed.r#type != Some(proto::signed_message::MessageType::License as i32) {
            return Err(ClientError::NotALicense);
        }

        // Unwrap the AES session key with the device RSA key (OAEP/SHA-1).
        let session_key = Zeroizing::new(
            self.private_key
                .decrypt(
                    Oaep::new::<Sha1>(),
                    signed.session_key.as_deref().unwrap_or_default(),
                )
                .map_err(|_| ClientError::SessionKey)?,
        );

        let (enc_context, mac_context) = derive_context(&challenge.request);
        let enc_key = Zeroizing::new(cmac(&session_key, &enc_context, 1)?);
        let mut mac_key_server = cmac(&session_key, &mac_context, 1)?;
        mac_key_server.extend(cmac(&session_key, &mac_context, 2)?);

        // Verify the server's HMAC-SHA256 signature over (oemcrypto || msg).
        let mut hmac =
            Hmac::<Sha256>::new_from_slice(&mac_key_server).map_err(|_| ClientError::Derive)?;
        hmac.update(signed.oemcrypto_core_message.as_deref().unwrap_or_default());
        hmac.update(signed.msg.as_deref().unwrap_or_default());
        hmac.verify_slice(signed.signature.as_deref().unwrap_or_default())
            .map_err(|_| ClientError::Signature)?;

        let licence = proto::License::decode(signed.msg.as_deref().unwrap_or_default())
            .map_err(|_| ClientError::License)?;
        let mut keys = Vec::new();
        for container in &licence.key {
            if container.r#type != Some(proto::license::key_container::KeyType::Content as i32) {
                continue;
            }
            let iv = container.iv.as_deref().unwrap_or_default();
            let ciphertext = container.key.as_deref().unwrap_or_default();
            let plain = Aes128CbcDec::new_from_slices(&enc_key, iv)
                .map_err(|_| ClientError::Decrypt)?
                .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
                .map_err(|_| ClientError::Decrypt)?;
            keys.push(ContentKey {
                kid: kid_16(container.id.as_deref().unwrap_or_default()),
                key: Zeroizing::new(plain),
            });
        }
        Ok(keys)
    }
}

/// The ENCRYPTION and AUTHENTICATION context strings for key derivation.
fn derive_context(request: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let build = |label: &[u8], key_bits: u32| {
        let mut ctx = Vec::with_capacity(label.len() + 1 + request.len() + 4);
        ctx.extend_from_slice(label);
        ctx.push(0);
        ctx.extend_from_slice(request);
        ctx.extend_from_slice(&key_bits.to_be_bytes());
        ctx
    };
    (build(b"ENCRYPTION", 128), build(b"AUTHENTICATION", 512))
}

/// AES-CMAC(session_key, [counter] || context) — one 16-byte derivation block.
fn cmac(session_key: &[u8], context: &[u8], counter: u8) -> Result<Vec<u8>, ClientError> {
    let mut mac = Cmac::<Aes128>::new_from_slice(session_key).map_err(|_| ClientError::Derive)?;
    mac.update(&[counter]);
    mac.update(context);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// Normalises a key id to 16 bytes (right-pad / truncate).
fn kid_16(id: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = id.len().min(16);
    out[..n].copy_from_slice(&id[..n]);
    out
}
