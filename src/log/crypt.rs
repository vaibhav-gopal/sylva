//! Encryption at rest for a stored log (feature `crypt`).
//!
//! XChaCha20-Poly1305 (pure Rust, so it builds the same everywhere) under one
//! random master key per store. Keys for each purpose are derived from the
//! master key with HMAC-SHA256 under the application's name
//! (`<app>/v1/<purpose>`), so two applications never share a key even if
//! they share a master key.
//!
//! [`Encrypted`] is a [`Codec`](super::sqlite::Codec) for
//! [`ChainLog`](super::sqlite::ChainLog): each op is sealed with its hash as
//! associated data, so a payload can't be moved to another row undetected,
//! and stored as `e1:` + base64(nonce || ciphertext). Rows without the prefix
//! are read as plain JSON, so a store can switch encryption on in place.
//!
//! Where the master key lives (the OS keychain, an environment variable,
//! escrow) is the application's business.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::Sha256;

use crate::{Error, Result};

const NONCE_LEN: usize = 24;

/// Marks a sealed payload: `e1:` + base64(nonce || ciphertext).
pub const PREFIX: &str = "e1:";

/// A store's root secret. Never written to the log, or to disk in the clear.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn generate() -> Self {
        MasterKey(XChaCha20Poly1305::generate_key(&mut OsRng).into())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        MasterKey(bytes)
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes =
            hex::decode(s.trim()).map_err(|_| Error::Invalid("master key is not hex".into()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::Invalid("master key must be 32 bytes".into()))?;
        Ok(MasterKey(bytes))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// A key for one purpose, e.g. `alfred/v1/data`.
    pub fn derive(&self, purpose: &str) -> [u8; 32] {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(&self.0).expect("HMAC takes any key length");
        mac.update(purpose.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// Seals and opens data under an application's data key.
#[derive(Clone)]
pub struct Cipher {
    aead: XChaCha20Poly1305,
}

impl Cipher {
    /// The cipher for `app`'s data (key `<app>/v1/data`).
    pub fn new(key: &MasterKey, app: &str) -> Self {
        Cipher {
            aead: XChaCha20Poly1305::new(&key.derive(&format!("{app}/v1/data")).into()),
        }
    }

    /// `nonce || ciphertext`. `aad` binds the ciphertext to its context (an
    /// op's hash, a store's id), so it can't be moved elsewhere undetected.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut out = nonce.to_vec();
        out.extend(
            self.aead
                .encrypt(
                    &nonce,
                    Payload {
                        msg: plaintext,
                        aad,
                    },
                )
                .expect("encryption cannot fail"),
        );
        out
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN {
            return Err(Error::Integrity {
                seq: 0,
                reason: "ciphertext too short".into(),
            });
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        self.aead
            .decrypt(XNonce::from_slice(nonce), Payload { msg: body, aad })
            .map_err(|_| Error::Integrity {
                seq: 0,
                reason: "decryption failed (wrong key or tampered data)".into(),
            })
    }

    /// `e1:` + base64 of [`Cipher::seal`].
    pub fn seal_text(&self, plaintext: &[u8], aad: &[u8]) -> String {
        format!("{PREFIX}{}", B64.encode(self.seal(plaintext, aad)))
    }

    /// Opens what [`Cipher::seal_text`] wrote.
    pub fn open_text(&self, stored: &str, aad: &[u8]) -> Result<Vec<u8>> {
        let body = stored
            .strip_prefix(PREFIX)
            .ok_or_else(|| Error::Invalid("not a sealed payload".into()))?;
        let sealed = B64.decode(body).map_err(|_| Error::Integrity {
            seq: 0,
            reason: "sealed payload is not base64".into(),
        })?;
        self.open(&sealed, aad)
    }
}

/// A [`Codec`](super::sqlite::Codec): ops sealed with their hash as associated
/// data when there's a cipher, plain JSON otherwise. Reads both.
#[cfg(feature = "sqlite")]
pub struct Encrypted<'a>(pub Option<&'a Cipher>);

#[cfg(feature = "sqlite")]
impl<O: Serialize + DeserializeOwned> super::sqlite::Codec<O> for Encrypted<'_> {
    fn encode(&self, op: &O, hash: &str) -> Result<String> {
        let json = serde_json::to_vec(op)?;
        Ok(match self.0 {
            Some(c) => c.seal_text(&json, hash.as_bytes()),
            None => String::from_utf8(json).expect("serde_json writes UTF-8"),
        })
    }

    fn decode(&self, stored: &str, hash: &str) -> Result<O> {
        if !stored.starts_with(PREFIX) {
            return Ok(serde_json::from_str(stored)?);
        }
        let cipher = self.0.ok_or_else(|| {
            Error::Invalid("an encrypted op in a store opened without its key".into())
        })?;
        Ok(serde_json::from_slice(
            &cipher.open_text(stored, hash.as_bytes())?,
        )?)
    }
}

/// Decodes plain or sealed payloads without a codec (e.g. to convert a store).
pub fn open_payload(cipher: Option<&Cipher>, stored: &str, hash: &str) -> Result<Vec<u8>> {
    match (stored.starts_with(PREFIX), cipher) {
        (false, _) => Ok(stored.as_bytes().to_vec()),
        (true, Some(c)) => c.open_text(stored, hash.as_bytes()),
        (true, None) => Err(Error::Invalid(
            "an encrypted payload without its key".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_binds_and_separates_apps() {
        let key = MasterKey::generate();
        let (a, b) = (Cipher::new(&key, "alfred"), Cipher::new(&key, "tend"));
        let sealed = a.seal_text(b"{\"op\":1}", b"hash-1");
        assert!(sealed.starts_with(PREFIX));
        assert_eq!(a.open_text(&sealed, b"hash-1").unwrap(), b"{\"op\":1}");
        assert!(
            a.open_text(&sealed, b"hash-2").is_err(),
            "bound to its hash"
        );
        assert!(
            b.open_text(&sealed, b"hash-1").is_err(),
            "another app's key"
        );
        assert_eq!(
            MasterKey::from_hex(&key.to_hex()).unwrap().derive("x"),
            key.derive("x")
        );
        assert!(MasterKey::from_hex("abc").is_err());
    }

    /// The derivation and layout are Alfred's (`alfred/v1/data`, nonce || ciphertext):
    /// a ciphertext from one opens with the other.
    #[test]
    fn matches_alfreds_format() {
        let key = MasterKey::from_bytes([3; 32]);
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&[3; 32]).unwrap();
        mac.update(b"alfred/v1/data");
        let theirs = XChaCha20Poly1305::new(&<[u8; 32]>::from(mac.finalize().into_bytes()).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut sealed = nonce.to_vec();
        sealed.extend(
            theirs
                .encrypt(
                    &nonce,
                    Payload {
                        msg: b"hi",
                        aad: b"h",
                    },
                )
                .unwrap(),
        );
        assert_eq!(
            Cipher::new(&key, "alfred").open(&sealed, b"h").unwrap(),
            b"hi"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn the_codec_reads_plain_and_sealed_rows() {
        use super::super::sqlite::Codec;
        let c = Cipher::new(&MasterKey::generate(), "test");
        let sealed: String = Encrypted(Some(&c)).encode(&vec![1, 2], "h").unwrap();
        let back: Vec<i32> = Encrypted(Some(&c)).decode(&sealed, "h").unwrap();
        assert_eq!(back, [1, 2]);
        let plain: Vec<i32> = Encrypted(Some(&c)).decode("[3]", "h").unwrap();
        assert_eq!(plain, [3], "rows written before encryption was on");
        assert!(Codec::<Vec<i32>>::decode(&Encrypted(None), &sealed, "h").is_err());
        assert_eq!(open_payload(Some(&c), &sealed, "h").unwrap(), b"[1,2]");
    }
}
