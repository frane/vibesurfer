//! Auth blob encryption and master-key resolution.
//!
//! Auth blobs are AES-256-GCM ciphertexts of opaque cookie/storage
//! state. The key is local to the host (encryption is *not* portable
//! across machines). Resolution order on the daemon:
//!
//! 1. OS keyring (`keyring` crate) under service `"vibesurfer"`,
//!    account `"default"`.
//! 2. Fallback: a 32-byte file at `~/.vibesurfer/key`.
//!
//! Tests skip the keyring (it would prompt the user) and pass keys
//! explicitly via [`MasterKey::from_bytes`].

use std::fs;
use std::path::Path;

use ring::aead::{
    Aad, BoundKey, Nonce, NonceSequence, OpeningKey, SealingKey, UnboundKey, AES_256_GCM,
};
use ring::error::Unspecified;
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::{Result, StoreError};

/// One-time installer for the keyring-core default credential store.
/// keyring-core 1.x split the per-platform implementations into
/// separate crates (`apple-native-keyring-store`,
/// `linux-keyutils-keyring-store`, `windows-native-keyring-store`);
/// each lib has to register itself before `Entry::new` will work.
fn ensure_default_keyring_store() -> Result<()> {
    use std::sync::OnceLock;
    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let r = INIT.get_or_init(install_default_store);
    match r {
        Ok(()) => Ok(()),
        Err(msg) => Err(StoreError::Keyring(msg.clone())),
    }
}

#[cfg(target_os = "macos")]
fn install_default_store() -> std::result::Result<(), String> {
    let store = apple_native_keyring_store::keychain::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_default_store() -> std::result::Result<(), String> {
    let store = linux_keyutils_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_default_store() -> std::result::Result<(), String> {
    let store = windows_native_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn install_default_store() -> std::result::Result<(), String> {
    Err("no native keyring store available for this target".into())
}

/// Service name used for the OS keyring.
pub const KEYRING_SERVICE: &str = "vibesurfer";
/// Account name used for the OS keyring.
pub const KEYRING_ACCOUNT: &str = "default";

const KEY_LEN: usize = 32; // AES-256 key
const NONCE_LEN: usize = 12; // GCM nonce

/// A 32-byte AES-256 master key.
///
/// Construct via [`MasterKey::from_bytes`], [`MasterKey::from_file`],
/// [`MasterKey::from_keyring`], [`MasterKey::resolve`], or
/// [`MasterKey::generate`].
#[derive(Clone)]
pub struct MasterKey([u8; KEY_LEN]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key bytes; tests that need to inspect equality
        // can compare via `MasterKey::from_bytes` round-trips.
        f.write_str("MasterKey([REDACTED])")
    }
}

impl MasterKey {
    /// Construct from explicit bytes. The agent must keep the bytes
    /// confidential; the wrapper offers no extra protection beyond not
    /// implementing `Debug` or `Display`.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Read a 32-byte master key from `path`.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = fs::read(path.as_ref())?;
        if bytes.len() != KEY_LEN {
            return Err(StoreError::KeyFileSize(bytes.len()));
        }
        let mut buf = [0u8; KEY_LEN];
        buf.copy_from_slice(&bytes);
        Ok(Self(buf))
    }

    /// Look up the master key in the OS keyring under
    /// (`vibesurfer`, `default`). Reads the entry as a hex string so it
    /// round-trips through cross-platform secret stores that don't
    /// tolerate raw bytes.
    pub fn from_keyring() -> Result<Self> {
        ensure_default_keyring_store()?;
        let entry = keyring_core::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map_err(|e| StoreError::Keyring(e.to_string()))?;
        let hex = entry
            .get_password()
            .map_err(|e| StoreError::Keyring(e.to_string()))?;
        decode_hex_key(&hex)
    }

    /// Try the OS keyring first; fall back to `path`. Errors only if
    /// both fail.
    pub fn resolve(fallback_path: impl AsRef<Path>) -> Result<Self> {
        match Self::from_keyring() {
            Ok(k) => Ok(k),
            Err(_) => Self::from_file(fallback_path),
        }
    }

    /// Generate a fresh random key from the system CSPRNG.
    pub fn generate() -> Result<Self> {
        let mut buf = [0u8; KEY_LEN];
        SystemRandom::new()
            .fill(&mut buf)
            .map_err(|_| StoreError::Crypto("rng"))?;
        Ok(Self(buf))
    }

    /// Persist the key as 32 raw bytes at `path`. Caller is responsible
    /// for chmod-ing the file (typically 0600) and for ensuring the
    /// destination directory exists.
    pub fn write_to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        fs::write(path.as_ref(), self.0)?;
        Ok(())
    }

    /// Persist the key into the OS keyring as a hex string.
    pub fn write_to_keyring(&self) -> Result<()> {
        use std::fmt::Write as _;
        let mut hex = String::with_capacity(KEY_LEN * 2);
        for b in &self.0 {
            write!(hex, "{b:02x}").expect("write to String never fails");
        }
        ensure_default_keyring_store()?;
        let entry = keyring_core::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map_err(|e| StoreError::Keyring(e.to_string()))?;
        entry
            .set_password(&hex)
            .map_err(|e| StoreError::Keyring(e.to_string()))?;
        Ok(())
    }

    fn raw(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

fn decode_hex_key(s: &str) -> Result<MasterKey> {
    if s.len() != KEY_LEN * 2 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StoreError::Crypto("hex key shape"));
    }
    let mut buf = [0u8; KEY_LEN];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk).map_err(|_| StoreError::Crypto("hex key utf8"))?;
        buf[i] = u8::from_str_radix(pair, 16).map_err(|_| StoreError::Crypto("hex key digit"))?;
    }
    Ok(MasterKey::from_bytes(buf))
}

/// One ciphertext + its 12-byte GCM nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedBlob {
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; NONCE_LEN],
}

/// Encrypt `plaintext` under `key`. The returned blob is suitable for
/// inserting into `auth_blobs.ciphertext` (with `nonce` as a sibling
/// column).
pub fn encrypt(key: &MasterKey, plaintext: &[u8]) -> Result<EncryptedBlob> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| StoreError::Crypto("rng"))?;

    let unbound =
        UnboundKey::new(&AES_256_GCM, key.raw()).map_err(|_| StoreError::Crypto("key"))?;
    let mut sealing = SealingKey::new(unbound, OneNonce(Some(nonce)));

    let mut buf = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(Aad::empty(), &mut buf)
        .map_err(|_| StoreError::Crypto("seal"))?;
    Ok(EncryptedBlob {
        ciphertext: buf,
        nonce,
    })
}

/// Decrypt a previously encrypted blob.
pub fn decrypt(key: &MasterKey, blob: &EncryptedBlob) -> Result<Vec<u8>> {
    let unbound =
        UnboundKey::new(&AES_256_GCM, key.raw()).map_err(|_| StoreError::Crypto("key"))?;
    let mut opening = OpeningKey::new(unbound, OneNonce(Some(blob.nonce)));
    let mut buf = blob.ciphertext.clone();
    let plaintext = opening
        .open_in_place(Aad::empty(), &mut buf)
        .map_err(|_| StoreError::Crypto("open (likely wrong key or tampered blob)"))?;
    Ok(plaintext.to_vec())
}

/// Single-shot nonce sequence. AES-GCM is unsafe to reuse a nonce with
/// the same key; we randomize per-message and never replay, so each
/// `SealingKey`/`OpeningKey` gets exactly one nonce and is then dropped.
struct OneNonce(Option<[u8; NONCE_LEN]>);

impl NonceSequence for OneNonce {
    fn advance(&mut self) -> std::result::Result<Nonce, Unspecified> {
        let bytes = self.0.take().ok_or(Unspecified)?;
        Ok(Nonce::assume_unique_for_key(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> MasterKey {
        MasterKey::from_bytes([7u8; KEY_LEN])
    }

    #[test]
    fn round_trip_empty() {
        let k = fixed_key();
        let blob = encrypt(&k, &[]).unwrap();
        assert_eq!(decrypt(&k, &blob).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_payload() {
        let k = fixed_key();
        let plain = b"cookies=session=abc123; theme=dark";
        let blob = encrypt(&k, plain).unwrap();
        assert_ne!(blob.ciphertext, plain);
        assert_eq!(decrypt(&k, &blob).unwrap(), plain.to_vec());
    }

    #[test]
    fn nonces_are_unique() {
        let k = fixed_key();
        let a = encrypt(&k, b"hello").unwrap();
        let b = encrypt(&k, b"hello").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn wrong_key_rejected() {
        let k1 = MasterKey::from_bytes([1u8; KEY_LEN]);
        let k2 = MasterKey::from_bytes([2u8; KEY_LEN]);
        let blob = encrypt(&k1, b"secret").unwrap();
        assert!(decrypt(&k2, &blob).is_err());
    }

    #[test]
    fn tamper_rejected() {
        let k = fixed_key();
        let mut blob = encrypt(&k, b"secret").unwrap();
        // Flip the first byte of ciphertext.
        blob.ciphertext[0] ^= 0xff;
        assert!(decrypt(&k, &blob).is_err());
    }

    #[test]
    fn key_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let k = MasterKey::generate().unwrap();
        k.write_to_file(&path).unwrap();
        let loaded = MasterKey::from_file(&path).unwrap();
        assert_eq!(k.0, loaded.0);
    }

    #[test]
    fn key_file_wrong_size_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, b"too short").unwrap();
        let err = MasterKey::from_file(&path).unwrap_err();
        matches!(err, StoreError::KeyFileSize(_));
    }
}
