//! Token-blob encryption for the credential store.
//!
//! Blobs are self-describing: a leading version byte selects the scheme, so a
//! store can move between plaintext and encrypted without a schema change and
//! old rows keep decoding after a key is introduced.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

const V_PLAIN: u8 = 0x00;
const V_XCHACHA: u8 = 0x01;
const NONCE_LEN: usize = 24;

pub enum TokenCipher {
    Plain,
    Encrypted(Box<XChaCha20Poly1305>),
}

impl TokenCipher {
    /// Build a cipher from the optional `TOKEN_ENC_KEY`. Any string works; it
    /// is hashed to a 32-byte key, so length isn't constrained (use a long
    /// random value). Absent key → plaintext blobs (caller should warn).
    pub fn new(key: Option<&str>) -> Self {
        match key {
            Some(k) if !k.is_empty() => {
                let digest = Sha256::digest(k.as_bytes());
                let cipher = XChaCha20Poly1305::new_from_slice(&digest)
                    .expect("sha256 digest is always 32 bytes");
                TokenCipher::Encrypted(Box::new(cipher))
            }
            _ => TokenCipher::Plain,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self, TokenCipher::Encrypted(_))
    }

    /// Seal plaintext into a versioned blob for storage.
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        match self {
            TokenCipher::Plain => {
                let mut out = Vec::with_capacity(1 + plaintext.len());
                out.push(V_PLAIN);
                out.extend_from_slice(plaintext);
                out
            }
            TokenCipher::Encrypted(cipher) => {
                let nonce_bytes = rand::random::<[u8; NONCE_LEN]>();
                let nonce = XNonce::from_slice(&nonce_bytes);
                // In-memory AEAD of a few hundred bytes does not fail.
                let ct = cipher
                    .encrypt(nonce, plaintext)
                    .expect("xchacha20poly1305 encrypt");
                let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
                out.push(V_XCHACHA);
                out.extend_from_slice(&nonce_bytes);
                out.extend_from_slice(&ct);
                out
            }
        }
    }

    /// Open a stored blob. Dispatches on the version byte, so plaintext rows
    /// decode regardless of whether a key is configured. Returns None on a
    /// truncated blob, a bad tag, or an encrypted row with no/ wrong key.
    pub fn open(&self, blob: &[u8]) -> Option<Vec<u8>> {
        match blob.split_first() {
            Some((&V_PLAIN, rest)) => Some(rest.to_vec()),
            Some((&V_XCHACHA, rest)) => {
                let cipher = match self {
                    TokenCipher::Encrypted(c) => c,
                    TokenCipher::Plain => return None,
                };
                if rest.len() < NONCE_LEN {
                    return None;
                }
                let (nonce_bytes, ct) = rest.split_at(NONCE_LEN);
                cipher.decrypt(XNonce::from_slice(nonce_bytes), ct).ok()
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_roundtrip() {
        let c = TokenCipher::new(None);
        let blob = c.seal(b"hello tokens");
        assert_eq!(blob[0], V_PLAIN);
        assert_eq!(c.open(&blob).as_deref(), Some(&b"hello tokens"[..]));
    }

    #[test]
    fn encrypted_roundtrip() {
        let c = TokenCipher::new(Some("a-long-random-key"));
        let blob = c.seal(b"secret tokens");
        assert_eq!(blob[0], V_XCHACHA);
        assert_ne!(&blob[1..], b"secret tokens");
        assert_eq!(c.open(&blob).as_deref(), Some(&b"secret tokens"[..]));
    }

    #[test]
    fn encrypted_blob_needs_the_right_key() {
        let good = TokenCipher::new(Some("key-one"));
        let blob = good.seal(b"secret");
        let wrong = TokenCipher::new(Some("key-two"));
        assert!(wrong.open(&blob).is_none());
    }

    #[test]
    fn encrypted_store_still_reads_legacy_plaintext_rows() {
        // A row written before a key existed must survive key introduction.
        let legacy = TokenCipher::new(None).seal(b"old tokens");
        let with_key = TokenCipher::new(Some("new-key"));
        assert_eq!(with_key.open(&legacy).as_deref(), Some(&b"old tokens"[..]));
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let c = TokenCipher::new(Some("k"));
        assert_ne!(c.seal(b"x"), c.seal(b"x"));
    }
}
