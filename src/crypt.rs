//! Encryption at rest for the learned store (M6).
//!
//! The store is a map of everything the user considers private, so on disk it can be
//! sealed with a passphrase: Argon2id stretches it into a key, AES-256-GCM encrypts and
//! authenticates the JSON. The file becomes an envelope:
//!
//! ```json
//! {"portcullis_store":1,"kdf":"argon2id","m_kib":65536,"t":3,"p":1,
//!  "salt":"…","nonce":"…","ciphertext":"…"}
//! ```
//!
//! Every field except the ciphertext is bound as AEAD associated data, so tampering with
//! the KDF parameters or salt fails authentication instead of quietly weakening the key.
//! A fresh salt and nonce are drawn on every save.
//!
//! What this protects: the file at rest (stolen disk, backup, a stray `git add`). What it
//! does not: a running process holds the decrypted store in memory, and whoever can read
//! the process, the environment or the key file has the key.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

const FORMAT_VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
/// A tampered file must not be able to demand gigabytes of RAM from the KDF.
const MAX_M_KIB: u32 = 1 << 20;
const MAX_T: u32 = 16;
const MAX_P: u32 = 16;
/// Below this a passphrase is guessable however good the KDF is.
const RECOMMENDED_MIN_LEN: usize = 16;

/// Argon2id cost parameters. The defaults follow the OWASP guidance (64 MiB, 3 passes).
#[derive(Debug, Clone, Copy)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        }
    }
}

/// The passphrase that seals the store.
#[derive(Clone)]
pub struct StoreKey {
    passphrase: Vec<u8>,
    params: KdfParams,
}

// Deliberately no `Debug` that prints the passphrase.
impl std::fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoreKey(<redacted>)")
    }
}

impl StoreKey {
    pub fn new(passphrase: impl Into<Vec<u8>>) -> Self {
        Self {
            passphrase: passphrase.into(),
            params: KdfParams::default(),
        }
    }

    /// Override the KDF cost (tests use a cheap setting; production should not).
    pub fn with_params(mut self, params: KdfParams) -> Self {
        self.params = params;
        self
    }

    /// Read the key from `PORTCULLIS_STORE_KEY` or the file named by
    /// `PORTCULLIS_STORE_KEY_FILE`. `Ok(None)` means encryption is not configured.
    pub fn from_env() -> Result<Option<Self>> {
        let inline = std::env::var("PORTCULLIS_STORE_KEY").ok();
        let file = std::env::var("PORTCULLIS_STORE_KEY_FILE").ok();
        let passphrase = match (inline, file) {
            (None, None) => return Ok(None),
            (Some(_), Some(_)) => {
                bail!("set only one of PORTCULLIS_STORE_KEY and PORTCULLIS_STORE_KEY_FILE")
            }
            (Some(k), None) => k,
            (None, Some(path)) => read_key_file(Path::new(&path))?,
        };
        if passphrase.is_empty() {
            bail!("the store key is empty");
        }
        if passphrase.len() < RECOMMENDED_MIN_LEN {
            tracing::warn!(
                "the store passphrase is shorter than {RECOMMENDED_MIN_LEN} characters; \
                 use a long random one (e.g. `openssl rand -base64 32`)"
            );
        }
        Ok(Some(Self::new(passphrase)))
    }

    fn derive(&self, salt: &[u8], params: KdfParams) -> Result<[u8; KEY_LEN]> {
        let params = Params::new(params.m_kib, params.t, params.p, Some(KEY_LEN))
            .map_err(|e| anyhow!("invalid KDF parameters: {e}"))?;
        let mut key = [0u8; KEY_LEN];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(&self.passphrase, salt, &mut key)
            .map_err(|e| anyhow!("key derivation failed: {e}"))?;
        Ok(key)
    }
}

fn read_key_file(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("cannot read the store key file {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                "the store key file {} is readable by other users (mode {:o}); run `chmod 600`",
                path.display(),
                mode & 0o777
            );
        }
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the store key file {}", path.display()))?;
    Ok(raw.trim_end_matches(['\n', '\r']).to_string())
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    portcullis_store: u32,
    kdf: String,
    m_kib: u32,
    t: u32,
    p: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

impl Envelope {
    /// Everything but the ciphertext, bound to it as associated data.
    fn aad(&self) -> String {
        format!(
            "portcullis-store|v{}|{}|{}|{}|{}|{}|{}",
            self.portcullis_store, self.kdf, self.m_kib, self.t, self.p, self.salt, self.nonce
        )
    }
}

/// Is `bytes` an encrypted-store envelope (as opposed to plaintext store JSON)?
pub fn is_encrypted(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Envelope>(bytes).is_ok()
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("no secure randomness available: {e}"))?;
    Ok(buf)
}

/// Encrypt `plaintext` into an envelope.
pub fn seal(key: &StoreKey, plaintext: &[u8]) -> Result<Vec<u8>> {
    let salt = random::<SALT_LEN>()?;
    let nonce = random::<NONCE_LEN>()?;
    let mut env = Envelope {
        portcullis_store: FORMAT_VERSION,
        kdf: "argon2id".into(),
        m_kib: key.params.m_kib,
        t: key.params.t,
        p: key.params.p,
        salt: hex::encode(salt),
        nonce: hex::encode(nonce),
        ciphertext: String::new(),
    };
    let derived = key.derive(&salt, key.params)?;
    let cipher = Aes256Gcm::new_from_slice(&derived).map_err(|e| anyhow!("bad key: {e}"))?;
    let aad = env.aad();
    let sealed = cipher
        .encrypt(
            &Nonce::try_from(&nonce[..]).map_err(|_| anyhow!("bad nonce length"))?,
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("encryption failed"))?;
    env.ciphertext = hex::encode(sealed);
    Ok(serde_json::to_vec_pretty(&env)?)
}

/// Decrypt an envelope. A wrong key and a corrupted or tampered file are
/// indistinguishable by design, and report the same error.
pub fn open(key: &StoreKey, envelope: &[u8]) -> Result<Vec<u8>> {
    let env: Envelope =
        serde_json::from_slice(envelope).context("the store file is not a valid envelope")?;
    if env.portcullis_store != FORMAT_VERSION || env.kdf != "argon2id" {
        bail!(
            "unsupported store format (version {}, kdf {})",
            env.portcullis_store,
            env.kdf
        );
    }
    if env.m_kib > MAX_M_KIB || env.t > MAX_T || env.p > MAX_P {
        bail!("the store's KDF parameters are out of range");
    }
    let salt = hex::decode(&env.salt).context("bad salt")?;
    let nonce = hex::decode(&env.nonce).context("bad nonce")?;
    let ciphertext = hex::decode(&env.ciphertext).context("bad ciphertext")?;
    if nonce.len() != NONCE_LEN {
        bail!("bad nonce length");
    }

    let derived = key.derive(
        &salt,
        KdfParams {
            m_kib: env.m_kib,
            t: env.t,
            p: env.p,
        },
    )?;
    let cipher = Aes256Gcm::new_from_slice(&derived).map_err(|e| anyhow!("bad key: {e}"))?;
    let aad = env.aad();
    cipher
        .decrypt(
            &Nonce::try_from(&nonce[..]).map_err(|_| anyhow!("bad nonce length"))?,
            Payload {
                msg: &ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("cannot decrypt the store: wrong key, or the file is corrupted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap(pass: &str) -> StoreKey {
        StoreKey::new(pass).with_params(KdfParams {
            m_kib: 8,
            t: 1,
            p: 1,
        })
    }

    #[test]
    fn round_trips_and_hides_the_plaintext() {
        let key = cheap("correct horse battery staple");
        let sealed = seal(&key, br#"{"deny":[{"term":"Cartalian"}]}"#).unwrap();
        assert!(is_encrypted(&sealed));
        assert!(!String::from_utf8_lossy(&sealed).contains("Cartalian"));
        assert_eq!(
            open(&key, &sealed).unwrap(),
            br#"{"deny":[{"term":"Cartalian"}]}"#
        );
    }

    #[test]
    fn every_save_uses_a_fresh_salt_and_nonce() {
        let key = cheap("pass");
        let a = seal(&key, b"same").unwrap();
        let b = seal(&key, b"same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_wrong_key_is_rejected() {
        let sealed = seal(&cheap("right"), b"secret").unwrap();
        let err = open(&cheap("wrong"), &sealed).unwrap_err().to_string();
        assert!(err.contains("wrong key"), "{err}");
    }

    #[test]
    fn tampering_with_any_field_is_detected() {
        let key = cheap("pass");
        let sealed = seal(&key, b"secret").unwrap();
        let mut env: serde_json::Value = serde_json::from_slice(&sealed).unwrap();

        // Flip a bit of the ciphertext.
        let mut ct = hex::decode(env["ciphertext"].as_str().unwrap()).unwrap();
        ct[0] ^= 1;
        let mut bad = env.clone();
        bad["ciphertext"] = hex::encode(ct).into();
        assert!(open(&key, &serde_json::to_vec(&bad).unwrap()).is_err());

        // Weaken the KDF: bound as associated data, so the same key no longer opens it.
        env["t"] = 2.into();
        assert!(open(&key, &serde_json::to_vec(&env).unwrap()).is_err());
    }

    #[test]
    fn absurd_kdf_parameters_are_refused_before_any_work() {
        let key = cheap("pass");
        let mut env: serde_json::Value =
            serde_json::from_slice(&seal(&key, b"x").unwrap()).unwrap();
        env["m_kib"] = u32::MAX.into();
        let err = open(&key, &serde_json::to_vec(&env).unwrap()).unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn plaintext_store_json_is_not_mistaken_for_an_envelope() {
        assert!(!is_encrypted(br#"{"deny":[],"allow":[]}"#));
        assert!(!is_encrypted(b"not json"));
    }

    #[test]
    fn the_key_never_appears_in_debug_output() {
        assert!(!format!("{:?}", cheap("hunter2hunter2")).contains("hunter2"));
    }
}
