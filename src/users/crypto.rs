//! Token-blob encryption for the credential store.
//!
//! Blobs are self-describing: a leading version byte selects the scheme. The
//! ciphertext is bound to its owner via AEAD associated data (the caller passes
//! the row's `discord_user_id`), so a DB-write attacker can't swap one user's
//! ciphertext into another's row. When a key is configured, plaintext blobs are
//! rejected (no silent downgrade).

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use sha2::Sha256;

const V_PLAIN: u8 = 0x00;
const V_XCHACHA_AAD: u8 = 0x02; // XChaCha20-Poly1305 with owner-bound AAD
const NONCE_LEN: usize = 24;
/// Fixed application salt for stretching TOKEN_ENC_KEY. Salt and iteration
/// count are part of the frozen storage format: changing either changes every
/// derived key and deployed blobs stop opening (pinned by the golden-blob test).
const KDF_SALT: &[u8] = b"discord-spotify-player:token-enc:v1";
// Same derivation code under test, only the count differs (unoptimized debug
// builds make 600k-iteration PBKDF2 too slow for the suite).
#[cfg(not(test))]
const KDF_ITERATIONS: u32 = 600_000;
#[cfg(test)]
const KDF_ITERATIONS: u32 = 1_000;

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
                // Stretch the key with PBKDF2-HMAC-SHA256 so a weak env key is
                // costly to brute-force from a stolen DB.
                let mut derived = [0u8; 32];
                pbkdf2::pbkdf2_hmac::<Sha256>(k.as_bytes(), KDF_SALT, KDF_ITERATIONS, &mut derived);
                let cipher = XChaCha20Poly1305::new_from_slice(&derived)
                    .expect("derived key is always 32 bytes");
                TokenCipher::Encrypted(Box::new(cipher))
            }
            _ => TokenCipher::Plain,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self, TokenCipher::Encrypted(_))
    }

    /// Seal plaintext into a versioned blob, binding it to `aad` (the owner id).
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
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
                    .encrypt(nonce, Payload { msg: plaintext, aad })
                    .expect("xchacha20poly1305 encrypt");
                let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
                out.push(V_XCHACHA_AAD);
                out.extend_from_slice(&nonce_bytes);
                out.extend_from_slice(&ct);
                out
            }
        }
    }

    /// Open a stored blob, verifying it was sealed for this `aad` (owner id).
    /// Returns None on a truncated blob, a bad tag, an AAD mismatch, an
    /// encrypted row with no/wrong key, or a plaintext row when a key IS set
    /// (a downgrade attempt).
    pub fn open(&self, blob: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        match blob.split_first() {
            Some((&V_PLAIN, rest)) => match self {
                // A plaintext row is only legitimate when no key is configured.
                TokenCipher::Plain => Some(rest.to_vec()),
                TokenCipher::Encrypted(_) => None,
            },
            Some((&V_XCHACHA_AAD, rest)) => {
                let cipher = match self {
                    TokenCipher::Encrypted(c) => c,
                    TokenCipher::Plain => return None,
                };
                if rest.len() < NONCE_LEN {
                    return None;
                }
                let (nonce_bytes, ct) = rest.split_at(NONCE_LEN);
                cipher
                    .decrypt(XNonce::from_slice(nonce_bytes), Payload { msg: ct, aad })
                    .ok()
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAD: &[u8] = b"discord-user-123";

    #[test]
    fn plaintext_roundtrip() {
        let c = TokenCipher::new(None);
        let blob = c.seal(b"hello tokens", AAD);
        assert_eq!(blob[0], V_PLAIN);
        assert_eq!(c.open(&blob, AAD).as_deref(), Some(&b"hello tokens"[..]));
    }

    #[test]
    fn encrypted_roundtrip() {
        let c = TokenCipher::new(Some("a-long-random-key"));
        let blob = c.seal(b"secret tokens", AAD);
        assert_eq!(blob[0], V_XCHACHA_AAD);
        assert!(!blob.windows(6).any(|w| w == b"secret"));
        assert_eq!(c.open(&blob, AAD).as_deref(), Some(&b"secret tokens"[..]));
    }

    #[test]
    fn encrypted_blob_needs_the_right_key() {
        let good = TokenCipher::new(Some("key-one"));
        let blob = good.seal(b"secret", AAD);
        let wrong = TokenCipher::new(Some("key-two"));
        assert!(wrong.open(&blob, AAD).is_none());
    }

    #[test]
    fn ciphertext_is_bound_to_its_owner() {
        // A blob sealed for one user must not open under another's id — this is
        // what stops a DB-write attacker row-swapping ciphertext between users.
        let c = TokenCipher::new(Some("k"));
        let blob = c.seal(b"secret", b"user-A");
        assert!(c.open(&blob, b"user-B").is_none());
        assert_eq!(c.open(&blob, b"user-A").as_deref(), Some(&b"secret"[..]));
    }

    #[test]
    fn encrypted_store_rejects_plaintext_downgrade() {
        // With a key set, a forged V_PLAIN row must not be accepted.
        let plain_blob = TokenCipher::new(None).seal(b"forged", AAD);
        let with_key = TokenCipher::new(Some("new-key"));
        assert!(with_key.open(&plain_blob, AAD).is_none());
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let c = TokenCipher::new(Some("k"));
        assert_ne!(c.seal(b"x", AAD), c.seal(b"x", AAD));
    }

    #[test]
    fn malformed_blobs_open_to_none() {
        let c = TokenCipher::new(Some("k"));
        // Empty blob: no version byte at all.
        assert!(c.open(&[], AAD).is_none());
        // Unknown version byte — including the retired 0x01 scheme, which must
        // NOT decode (there is deliberately no 0x01 handler).
        assert!(c.open(&[0x01, 1, 2, 3], AAD).is_none());
        assert!(c.open(&[0xFF], AAD).is_none());
        // Truncated: a valid version byte but fewer than NONCE_LEN bytes after.
        let mut truncated = vec![V_XCHACHA_AAD];
        truncated.extend_from_slice(&[0u8; NONCE_LEN - 1]);
        assert!(c.open(&truncated, AAD).is_none());
        // Nonce present but ciphertext cut short of a whole tag.
        let sealed = c.seal(b"secret", AAD);
        assert!(c.open(&sealed[..sealed.len() - 1], AAD).is_none());
        // A keyless cipher must not accept encrypted rows.
        assert!(TokenCipher::new(None).open(&sealed, AAD).is_none());
    }

    /// Pins the storage format end-to-end: a blob sealed by THIS code today is
    /// hardcoded below, and every future build must still open it. Any change
    /// to KDF_SALT, the (test) iteration count, the scheme byte, the nonce
    /// layout, or the AAD binding fails here — the failure mode it guards is a
    /// "refactor" that silently orphans every deployed row. If the format ever
    /// changes ON PURPOSE, bump the scheme byte, keep this vector openable via
    /// the old path, and add a new one. Regenerate with:
    /// `cargo test golden_blob_generator -- --ignored --nocapture`
    /// (Test builds derive with 1_000 iterations; the production constant is
    /// 600_000, same derivation code.)
    #[test]
    fn golden_blob_still_opens() {
        let hex = "0218553551173e90b1f25cc3a8633cbd0de9e95a196b5127e17d1729b4a6ce132e34f39a805c0522d968d05028e94bf34faa22b6d6e2a0f815";
        let blob: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let c = TokenCipher::new(Some("golden-key"));
        assert_eq!(
            c.open(&blob, b"golden-user").as_deref(),
            Some(&b"golden-plaintext"[..]),
            "stored-format compatibility broken: deployed rows would no longer decrypt"
        );
    }

    /// Not a test — regenerates the golden vector after an INTENTIONAL format
    /// change. Run with `cargo test golden_blob_generator -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn golden_blob_generator() {
        let c = TokenCipher::new(Some("golden-key"));
        let blob = c.seal(b"golden-plaintext", b"golden-user");
        let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
        println!("golden blob hex: {hex}");
    }
}
