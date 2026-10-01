//! Signed batches: an Ed25519 signature over a batch's canonical form
//! ([`Batch::to_jsonl`], so line-ending conversion on checkout doesn't break
//! it), prefixed by a domain string the application chooses, so a signature
//! made for one purpose can never be replayed as another.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::files::write_atomic;
use super::Batch;
use crate::{Error, Result};

/// A detached signature: which key, and the signature.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BatchSig {
    /// Id of the signing key (see [`key_id`]).
    pub key: String,
    /// Hex Ed25519 signature.
    pub sig: String,
}

/// A public key someone trusts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TrustedKey {
    pub id: String,
    /// Hex Ed25519 public key.
    pub public: String,
    /// Who holds it, e.g. `hub 01a0f0a2`.
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TrustedKeys {
    pub keys: Vec<TrustedKey>,
}

impl TrustedKeys {
    /// Adds a key (once). Returns whether it was new.
    pub fn trust(&mut self, key: TrustedKey) -> bool {
        if self.keys.iter().any(|k| k.id == key.id) {
            return false;
        }
        self.keys.push(key);
        true
    }
}

/// A signing key.
pub struct Signer(SigningKey);

impl Signer {
    pub fn from_seed(seed: [u8; 32]) -> Signer {
        Signer(SigningKey::from_bytes(&seed))
    }

    pub fn from_seed_hex(s: &str) -> Result<Signer> {
        let bytes: [u8; 32] = hex::decode(s.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| Error::Invalid("signing key must be 32 hex-encoded bytes".into()))?;
        Ok(Signer::from_seed(bytes))
    }

    pub fn seed_hex(&self) -> String {
        hex::encode(self.0.to_bytes())
    }

    pub fn public_hex(&self) -> String {
        hex::encode(self.0.verifying_key().to_bytes())
    }

    pub fn key_id(&self) -> String {
        key_id(&self.public_hex())
    }

    pub fn trusted(&self, name: &str) -> TrustedKey {
        TrustedKey { id: self.key_id(), public: self.public_hex(), name: name.to_string() }
    }

    pub fn sign<O: Serialize + DeserializeOwned>(&self, domain: &[u8], batch: &Batch<O>) -> BatchSig {
        BatchSig { key: self.key_id(), sig: hex::encode(self.0.sign(&message(domain, batch)).to_bytes()) }
    }
}

/// Short, stable id of a public key: the first 16 hex digits of its SHA-256.
pub fn key_id(public_hex: &str) -> String {
    hex::encode(Sha256::digest(public_hex.trim().to_lowercase().as_bytes()))[..16].to_string()
}

fn message<O: Serialize + DeserializeOwned>(domain: &[u8], batch: &Batch<O>) -> Vec<u8> {
    let mut m = domain.to_vec();
    m.extend_from_slice(batch.to_jsonl().as_bytes());
    m
}

/// How a batch stands against a set of trusted keys.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "verdict", content = "detail", rename_all = "snake_case")]
pub enum Verdict {
    /// Signed by this trusted key (its name).
    Signed(String),
    Unsigned,
    /// A signature that doesn't verify, or by a key that isn't trusted.
    Invalid(String),
}

pub fn verify<O: Serialize + DeserializeOwned>(domain: &[u8], batch: &Batch<O>, sig: Option<&BatchSig>, keys: &TrustedKeys) -> Verdict {
    let Some(sig) = sig else { return Verdict::Unsigned };
    let Some(key) = keys.keys.iter().find(|k| k.id == sig.key) else {
        return Verdict::Invalid(format!("signed by key {}, which isn't trusted here", sig.key));
    };
    let public: Option<[u8; 32]> = hex::decode(&key.public).ok().and_then(|b| b.try_into().ok());
    let Some(vk) = public.and_then(|b| VerifyingKey::from_bytes(&b).ok()) else {
        return Verdict::Invalid(format!("trusted key {} is malformed", key.id));
    };
    let signature: Option<[u8; 64]> = hex::decode(&sig.sig).ok().and_then(|b| b.try_into().ok());
    let Some(signature) = signature.map(|b| Signature::from_bytes(&b)) else {
        return Verdict::Invalid("malformed signature".into());
    };
    match vk.verify_strict(&message(domain, batch), &signature) {
        Ok(()) => Verdict::Signed(key.name.clone()),
        Err(_) => Verdict::Invalid("signature doesn't match the batch (edited after signing?)".into()),
    }
}

/// Signatures kept beside batch files: one `<batch id>.sig` each.
#[derive(Clone, Debug)]
pub struct SigDir {
    dir: PathBuf,
}

impl SigDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SigDir { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write(&self, batch: &Uuid, sig: &BatchSig) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        write_atomic(&self.dir.join(format!("{batch}.sig")), &serde_json::to_vec_pretty(sig)?)
    }

    pub fn read(&self) -> Result<BTreeMap<Uuid, BatchSig>> {
        let mut out = BTreeMap::new();
        if self.dir.is_dir() {
            for entry in fs::read_dir(&self.dir)? {
                let path = entry?.path();
                let id = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<Uuid>().ok());
                if let (Some(id), Some("sig")) = (id, path.extension().and_then(|e| e.to_str())) {
                    out.insert(id, serde_json::from_slice(&fs::read(&path)?)?);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::batch;
    use super::*;

    const DOMAIN: &[u8] = b"sylva-test-v1\n";

    #[test]
    fn signs_and_verifies_with_a_domain() {
        let hub = Signer::from_seed([7; 32]);
        let keys = TrustedKeys { keys: vec![hub.trusted("hub")] };
        let b = batch(1, None, vec![1]);
        let sig = hub.sign(DOMAIN, &b);
        assert_eq!(verify(DOMAIN, &b, Some(&sig), &keys), Verdict::Signed("hub".into()));
        assert_eq!(verify(DOMAIN, &b, None, &keys), Verdict::Unsigned);
        assert!(matches!(verify(b"other-v1\n", &b, Some(&sig), &keys), Verdict::Invalid(_)), "another domain");
        let mut edited = b.clone();
        edited.header.author = "agent:sneaky".into();
        assert!(matches!(verify(DOMAIN, &edited, Some(&sig), &keys), Verdict::Invalid(_)));
        let other = Signer::from_seed([8; 32]);
        assert!(matches!(verify(DOMAIN, &b, Some(&other.sign(DOMAIN, &b)), &keys), Verdict::Invalid(m) if m.contains("isn't trusted")));
        assert_eq!(Signer::from_seed_hex(&hub.seed_hex()).unwrap().key_id(), hub.key_id());

        let dir = tempfile::tempdir().unwrap();
        let sigs = SigDir::new(dir.path().join("sigs"));
        sigs.write(&b.header.batch, &sig).unwrap();
        assert_eq!(sigs.read().unwrap()[&b.header.batch], sig);
    }
}
