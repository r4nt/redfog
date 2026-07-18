//! Pairing crypto primitives for the GameStream/Moonlight PIN pairing handshake.
//!
//! The handshake binds two facts together with a chain of SHA-256 hashes: each
//! side proves it holds the AES key derived from `salt || PIN` (only known to
//! parties that were told the PIN out-of-band), and each side proves it holds
//! the private key matching the X.509 cert it presented earlier, by RSA-signing
//! a secret with it. AES here is raw ECB, block-by-block — the payloads are
//! always exact multiples of 16 bytes, so there's no padding and no chaining.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Key as GcmKey, Nonce};
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::{Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};

pub type CryptoError = String;

/// The DER `DigestInfo` prefix for SHA-256, per RFC 8017 appendix — a fixed,
/// version-independent constant. Built by hand (rather than via
/// `Pkcs1v15Sign::new::<Sha256>()`) because that path requires `Sha256:
/// AssociatedOid`, and this workspace pulls in two incompatible const-oid
/// versions (one via `rsa`'s own pkcs1/pkcs8 chain, one via `sha2`/`digest`),
/// which makes that trait bound unsatisfiable.
fn pkcs1v15_sha256() -> Pkcs1v15Sign {
    const SHA256_DIGESTINFO_PREFIX: [u8; 19] = [
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04,
        0x20,
    ];
    Pkcs1v15Sign {
        hash_len: Some(32),
        prefix: Box::new(SHA256_DIGESTINFO_PREFIX),
    }
}

/// `SHA256(salt || pin)[..16]`, the AES-128 key shared between client and
/// server once a human has relayed the PIN.
pub fn derive_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(pin.as_bytes());
    let hash = hasher.finalize();
    hash[..16].try_into().expect("sha256 output is 32 bytes")
}

fn aes128(key: &[u8; 16]) -> aes::Aes128 {
    aes::Aes128::new_from_slice(key).expect("key is exactly 16 bytes")
}

/// Encrypt `data` (must be a multiple of 16 bytes) with AES-128 in raw ECB
/// mode: each block is encrypted independently, no IV, no padding.
pub fn ecb_encrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, CryptoError> {
    if data.len() % 16 != 0 {
        return Err(format!("data length {} is not a multiple of 16", data.len()));
    }
    let cipher = aes128(key);
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        let block: &mut aes::Block = block.into();
        cipher.encrypt_block(block);
    }
    Ok(out)
}

/// Inverse of [`ecb_encrypt`].
pub fn ecb_decrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, CryptoError> {
    if data.len() % 16 != 0 {
        return Err(format!("data length {} is not a multiple of 16", data.len()));
    }
    let cipher = aes128(key);
    let mut out = data.to_vec();
    for block in out.chunks_mut(16) {
        let block: &mut aes::Block = block.into();
        cipher.decrypt_block(block);
    }
    Ok(out)
}

/// The raw ASN.1 `signatureValue` bit-string bytes of an X.509 cert (PEM).
/// Fed into the hash chain as a stand-in for "this exact certificate".
pub fn cert_signature_bytes(pem: &str) -> Result<Vec<u8>, CryptoError> {
    let (_, pem_obj) =
        x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|e| format!("invalid PEM: {e}"))?;
    let cert = pem_obj
        .parse_x509()
        .map_err(|e| format!("invalid X.509 cert: {e}"))?;
    Ok(cert.signature_value.data.to_vec())
}

/// SHA-256 fingerprint of the DER contents of a PEM cert, used as a stable key
/// for the paired-clients store.
pub fn cert_fingerprint(pem: &str) -> Result<String, CryptoError> {
    let (_, pem_obj) =
        x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|e| format!("invalid PEM: {e}"))?;
    Ok(cert_fingerprint_der(&pem_obj.contents))
}

/// Same digest as `cert_fingerprint`, but straight from DER bytes — for the
/// live TLS peer certificate (`CertificateDer`), which never went through PEM.
pub fn cert_fingerprint_der(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Decrypt an AES-128-GCM control-channel message: `key` is the client's
/// `rikey` (from `/launch`'s `rikey` param), `iv` is 12 bytes, `tag` is the
/// trailing 16-byte GCM authentication tag.
pub fn gcm_decrypt(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 12], tag: &[u8; 16]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes128Gcm::new(GcmKey::<Aes128Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    let mut buffer = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(nonce, b"", &mut buffer, tag.into())
        .map_err(|_| "gcm decryption/authentication failed".to_string())?;
    Ok(buffer)
}

/// Encrypt an AES-128-GCM control-channel message (server -> client, e.g.
/// rumble/HDR-mode messages). Returns `(ciphertext, tag)`.
pub fn gcm_encrypt(plaintext: &[u8], key: &[u8; 16], iv: &[u8; 12]) -> Result<(Vec<u8>, [u8; 16]), CryptoError> {
    let cipher = Aes128Gcm::new(GcmKey::<Aes128Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(iv);
    let mut buffer = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(nonce, b"", &mut buffer)
        .map_err(|e| format!("gcm encryption failed: {e}"))?;
    Ok((buffer, tag.into()))
}

/// Encrypt with AES-128-CBC + PKCS7 padding — the base GameStream protocol's
/// audio wire encryption. Unlike the control channel's AES-GCM (gated by the
/// `x-ss-general.encryptionSupported` CONTROL_V2 bit), audio encryption is
/// unconditional: confirmed by reading moonlight-common-rust's
/// `AudioDepayloader::poll_frame` (not vendored into git, see
/// scripts/fetch-patched-deps.sh), which decrypts every packet whenever a
/// `SunshineEncryption` key is configured at all, with no SDP negotiation
/// gating it. `key` is the client's `rikey` (from `/launch`'s `rikey` param);
/// `iv` is derived by the caller from `rikeyid` + the packet's RTP sequence
/// number (see `AudioStream.c`'s scheme, same source).
pub fn cbc_encrypt(plaintext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
    let mut buffer = plaintext.to_vec();
    let padded_len = plaintext.len().div_ceil(16) * 16 + 16;
    buffer.resize(padded_len, 0);
    let len = Aes128CbcEnc::new(key.into(), iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, plaintext.len())
        .expect("buffer sized for pkcs7 padding")
        .len();
    buffer.truncate(len);
    buffer
}

/// RSA-PKCS1v15-SHA256 sign `data` with the server's private key (PEM,
/// PKCS8).
pub fn rsa_sign(data: &[u8], private_key_pem: &str) -> Result<Vec<u8>, CryptoError> {
    let key = RsaPrivateKey::from_pkcs8_pem(private_key_pem).map_err(|e| format!("invalid private key: {e}"))?;
    let digest = Sha256::digest(data);
    let mut rng = rand::thread_rng();
    key.sign_with_rng(&mut rng, pkcs1v15_sha256(), &digest)
        .map_err(|e| format!("rsa sign failed: {e}"))
}

/// Verify an RSA-PKCS1v15-SHA256 signature over `data` using the public key
/// embedded in `cert_pem` (the peer's X.509 cert, PEM).
pub fn rsa_verify(data: &[u8], signature: &[u8], cert_pem: &str) -> Result<(), CryptoError> {
    let (_, pem_obj) =
        x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).map_err(|e| format!("invalid PEM: {e}"))?;
    let cert = pem_obj
        .parse_x509()
        .map_err(|e| format!("invalid X.509 cert: {e}"))?;
    let spki_bytes = cert.tbs_certificate.subject_pki.subject_public_key.data.as_ref();
    let public_key =
        RsaPublicKey::from_pkcs1_der(spki_bytes).map_err(|e| format!("invalid RSA public key: {e}"))?;
    let digest = Sha256::digest(data);
    public_key
        .verify(pkcs1v15_sha256(), &digest, signature)
        .map_err(|_| "signature verification failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockDecryptMut;

    /// Independent decrypt path (not `cbc_encrypt`'s own code) — catches an
    /// encode/decode asymmetry that a round-trip through the exact same
    /// function couldn't.
    fn cbc_decrypt_reference(ciphertext: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
        type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
        let mut buffer = ciphertext.to_vec();
        let plaintext = Aes128CbcDec::new(key.into(), iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut buffer)
            .expect("valid pkcs7 padding");
        plaintext.to_vec()
    }

    #[test]
    fn cbc_encrypt_round_trips() {
        let key = [0x42u8; 16];
        let iv = [0x11u8; 16];
        for len in [0, 1, 15, 16, 17, 100, 960] {
            let plaintext: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let ciphertext = cbc_encrypt(&plaintext, &key, &iv);
            // PKCS7 always adds at least one byte of padding, even when the
            // input is already block-aligned.
            assert_eq!(ciphertext.len(), (len / 16 + 1) * 16);
            assert_eq!(cbc_decrypt_reference(&ciphertext, &key, &iv), plaintext);
        }
    }
}
