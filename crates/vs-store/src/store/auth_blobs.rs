//! `auth_blobs` table CRUD with AES-256-GCM encryption.

use rusqlite::params;

use super::{epoch_secs, Store};
use crate::auth::{decrypt, encrypt, EncryptedBlob, MasterKey};
use crate::error::{Result, StoreError};
use crate::types::AuthBlobMeta;

impl Store {
    /// Encrypt `plaintext` under `key` and store under `name`. Replaces
    /// any existing blob with the same name.
    pub fn save_auth(&mut self, name: &str, key: &MasterKey, plaintext: &[u8]) -> Result<()> {
        let blob = encrypt(key, plaintext)?;
        let now = epoch_secs();
        self.conn().execute(
            "INSERT INTO auth_blobs(name, ciphertext, nonce, created_at, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(name) DO UPDATE
                SET ciphertext=excluded.ciphertext,
                    nonce=excluded.nonce,
                    created_at=excluded.created_at,
                    last_used_at=NULL",
            params![name, blob.ciphertext, blob.nonce.to_vec(), now],
        )?;
        Ok(())
    }

    /// Decrypt the named blob. Stamps `last_used_at` on success.
    pub fn load_auth(&mut self, name: &str, key: &MasterKey) -> Result<Vec<u8>> {
        let (ciphertext, nonce_vec) = {
            let mut stmt = self
                .conn()
                .prepare("SELECT ciphertext, nonce FROM auth_blobs WHERE name=?1")?;
            let mut rows = stmt.query([name])?;
            let row = rows.next()?.ok_or(StoreError::NotFound {
                kind: "auth_blob",
                id: name.to_string(),
            })?;
            let c: Vec<u8> = row.get(0)?;
            let n: Vec<u8> = row.get(1)?;
            (c, n)
        };
        if nonce_vec.len() != 12 {
            return Err(StoreError::Crypto("nonce shape"));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&nonce_vec);
        let plaintext = decrypt(key, &EncryptedBlob { ciphertext, nonce })?;
        let now = epoch_secs();
        self.conn().execute(
            "UPDATE auth_blobs SET last_used_at=?2 WHERE name=?1",
            params![name, now],
        )?;
        Ok(plaintext)
    }

    pub fn list_auth(&self) -> Result<Vec<AuthBlobMeta>> {
        let mut stmt = self
            .conn()
            .prepare("SELECT name, created_at, last_used_at FROM auth_blobs ORDER BY name ASC")?;
        let rows = stmt.query_map([], AuthBlobMeta::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_auth(&mut self, name: &str) -> Result<()> {
        let n = self
            .conn()
            .execute("DELETE FROM auth_blobs WHERE name=?1", params![name])?;
        if n == 0 {
            return Err(StoreError::NotFound {
                kind: "auth_blob",
                id: name.to_string(),
            });
        }
        Ok(())
    }
}
