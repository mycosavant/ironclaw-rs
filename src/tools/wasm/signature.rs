//! Ed25519 signature verification for installed WASM tools.
//!
//! # Overview
//!
//! Registry manifests can include an Ed25519 signature over the downloaded WASM
//! binary's SHA-256 hash.  Before any WASM binary is installed, IronClaw verifies:
//!
//! 1. **SHA-256 hash integrity** — the downloaded bytes match the SHA-256 declared in
//!    the registry manifest (`artifacts.wasm32-wasip2.sha256`).
//! 2. **Publisher signature** — if `publisher_pubkey` and `signature` are present, the
//!    Ed25519 signature is valid under a key in the local trust store
//!    (`~/.ironclaw/trusted_keys/*.hex`).
//!
//! # Signing Format
//!
//! The signed message is the UTF-8 bytes of:
//! ```text
//! {name}:{version}:{sha256_hex}
//! ```
//! where `sha256_hex` is the lowercase hex encoding of the SHA-256 hash.
//!
//! # Trust Key Store
//!
//! Trusted public keys are stored as `.hex` files in `~/.ironclaw/trusted_keys/`.
//! Each file contains a single 64-character lowercase hex string representing the
//! 32-byte Ed25519 verifying key.  Any key in this directory will be trusted for
//! signature verification.

use std::path::Path;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Failure modes for WASM binary verification.
#[derive(Debug, Error)]
pub enum VerificationError {
    /// The downloaded binary's SHA-256 does not match the manifest.
    #[error("SHA-256 hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    /// The Ed25519 signature did not verify under any trusted key.
    #[error("Ed25519 signature verification failed for '{name}'")]
    SignatureInvalid { name: String },

    /// A hex string in the manifest or key store is malformed.
    #[error("Invalid hex encoding: {0}")]
    InvalidHex(String),

    /// The 32-byte public key is not a valid Ed25519 verifying key.
    #[error("Invalid Ed25519 public key: {0}")]
    InvalidPublicKey(String),

    /// The 64-byte signature is not a valid Ed25519 signature.
    #[error("Invalid Ed25519 signature format: {0}")]
    InvalidSignature(String),

    /// The trust store directory could not be read.
    #[error("Could not read trusted key directory '{path}': {reason}")]
    TrustStoreIo { path: String, reason: String },
}

// ---------------------------------------------------------------------------
// SignedManifest
// ---------------------------------------------------------------------------

/// Cryptographic metadata accompanying a downloaded WASM tool.
///
/// Contains the expected SHA-256 hash, the publisher's Ed25519 verifying key
/// (as raw 32 bytes), and the signature (as raw 64 bytes).
#[derive(Debug, Clone)]
pub struct SignedManifest {
    /// Expected SHA-256 hash of the WASM binary.
    pub binary_sha256: [u8; 32],
    /// Raw 32-byte Ed25519 verifying key bytes.
    pub publisher_pubkey: [u8; 32],
    /// Raw 64-byte Ed25519 signature bytes.
    pub signature: [u8; 64],
}

impl SignedManifest {
    /// Parse from hex-encoded strings as they appear in registry manifests.
    ///
    /// Returns `None` if any field is missing; returns `Err` if hex decoding fails.
    pub fn from_hex(
        sha256_hex: Option<&str>,
        pubkey_hex: Option<&str>,
        signature_hex: Option<&str>,
    ) -> Result<Option<Self>, VerificationError> {
        let (Some(sha256_hex), Some(pubkey_hex), Some(sig_hex)) =
            (sha256_hex, pubkey_hex, signature_hex)
        else {
            return Ok(None); // Incomplete — skip signature verification
        };

        let sha256_bytes = hex::decode(sha256_hex)
            .map_err(|e| VerificationError::InvalidHex(format!("sha256: {}", e)))?;
        let pubkey_bytes = hex::decode(pubkey_hex)
            .map_err(|e| VerificationError::InvalidHex(format!("publisher_pubkey: {}", e)))?;
        let sig_bytes = hex::decode(sig_hex)
            .map_err(|e| VerificationError::InvalidHex(format!("signature: {}", e)))?;

        let binary_sha256: [u8; 32] = sha256_bytes.try_into().map_err(|_| {
            VerificationError::InvalidHex("sha256 must be 32 bytes (64 hex chars)".to_string())
        })?;
        let pubkey_arr: [u8; 32] = pubkey_bytes.try_into().map_err(|_| {
            VerificationError::InvalidPublicKey(
                "publisher_pubkey must be 32 bytes (64 hex chars)".to_string(),
            )
        })?;
        let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| {
            VerificationError::InvalidSignature(
                "signature must be 64 bytes (128 hex chars)".to_string(),
            )
        })?;

        Ok(Some(Self {
            binary_sha256,
            publisher_pubkey: pubkey_arr,
            signature: sig_arr,
        }))
    }

    /// Compute the canonical signing message: `{name}:{version}:{sha256_hex}`.
    pub fn signing_message(name: &str, version: &str, sha256: &[u8; 32]) -> Vec<u8> {
        format!("{}:{}:{}", name, version, hex::encode(sha256)).into_bytes()
    }

    /// Verify the signature using the embedded public key.
    ///
    /// Does NOT check whether the key is trusted — call this after confirming the
    /// key appears in [`TrustedKeyStore`].
    pub fn verify_signature(&self, name: &str, version: &str) -> Result<(), VerificationError> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};

        let vk = VerifyingKey::from_bytes(&self.publisher_pubkey)
            .map_err(|e| VerificationError::InvalidPublicKey(e.to_string()))?;
        let sig = Signature::from_bytes(&self.signature);
        let msg = Self::signing_message(name, version, &self.binary_sha256);

        vk.verify(&msg, &sig)
            .map_err(|_| VerificationError::SignatureInvalid {
                name: name.to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// TrustedKeyStore
// ---------------------------------------------------------------------------

/// A collection of trusted Ed25519 verifying keys loaded from the local keystore.
///
/// Keys are stored as `.hex` files in `~/.ironclaw/trusted_keys/`.  Each file
/// contains a single 64-character lowercase hex string (32 bytes = Ed25519 key).
#[derive(Debug, Default, Clone)]
pub struct TrustedKeyStore {
    /// Trusted verifying key bytes (32 bytes each).
    key_bytes: Vec<[u8; 32]>,
}

impl TrustedKeyStore {
    /// Create an empty key store (no pre-trusted keys).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load trusted keys from a directory.
    ///
    /// Reads all `*.hex` files and adds valid keys to the store.  Invalid or
    /// unreadable key files emit a warning and are skipped rather than aborting.
    pub async fn from_dir(dir: &Path) -> Result<Self, VerificationError> {
        let mut keys = Vec::new();

        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            // Directory doesn't exist yet — empty store, not an error
            return Ok(Self::empty());
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let is_hex = path.extension().and_then(|e| e.to_str()) == Some("hex");
            if !is_hex {
                continue;
            }

            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    let trimmed = content.trim();
                    match hex::decode(trimmed) {
                        Ok(bytes) => match bytes.try_into() {
                            Ok(key_bytes) => {
                                keys.push(key_bytes);
                            }
                            Err(_) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    "Trusted key file has wrong length (expected 32 bytes); skipping"
                                );
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "Could not hex-decode trusted key file; skipping"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Could not read trusted key file; skipping"
                    );
                }
            }
        }

        Ok(Self { key_bytes: keys })
    }

    /// Number of keys in the store.
    pub fn len(&self) -> usize {
        self.key_bytes.len()
    }

    /// Return `true` if the store has no trusted keys.
    pub fn is_empty(&self) -> bool {
        self.key_bytes.is_empty()
    }

    /// Return `true` if `pubkey_bytes` appears in this trust store.
    ///
    /// Uses constant-time comparison to prevent timing-based key oracle attacks.
    pub fn is_trusted(&self, pubkey_bytes: &[u8; 32]) -> bool {
        self.key_bytes
            .iter()
            .any(|k| k.ct_eq(pubkey_bytes).unwrap_u8() == 1)
    }
}

// ---------------------------------------------------------------------------
// DownloadVerification
// ---------------------------------------------------------------------------

/// Verification metadata to check after downloading a WASM binary.
///
/// Populated from `ArtifactSpec` in the registry manifest and passed to
/// `download_and_install_wasm`.  If both `sha256` and the signing fields are
/// present, both checks are enforced.
#[derive(Debug, Clone)]
pub struct DownloadVerification {
    /// Version string from the registry manifest, used in the signing message.
    pub version: String,
    /// Expected SHA-256 of the downloaded binary, if declared in the manifest.
    pub sha256: Option<[u8; 32]>,
    /// Signed manifest (hash + pubkey + signature), if all three fields are present.
    pub signed_manifest: Option<SignedManifest>,
}

impl DownloadVerification {
    /// Build from raw `ArtifactSpec` fields as they appear in the registry JSON.
    ///
    /// Returns `None` if neither a hash nor signing info is present (nothing to verify).
    pub fn from_artifact(
        version: &str,
        sha256_hex: Option<&str>,
        publisher_pubkey_hex: Option<&str>,
        signature_hex: Option<&str>,
    ) -> Result<Option<Self>, VerificationError> {
        // Parse SHA-256 (optional)
        let sha256 = match sha256_hex {
            None => None,
            Some(hex_str) => {
                let bytes = hex::decode(hex_str)
                    .map_err(|e| VerificationError::InvalidHex(format!("sha256: {}", e)))?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| {
                    VerificationError::InvalidHex(
                        "sha256 must be 32 bytes (64 hex chars)".to_string(),
                    )
                })?;
                Some(arr)
            }
        };

        // Parse SignedManifest (requires all three fields)
        let signed_manifest =
            SignedManifest::from_hex(sha256_hex, publisher_pubkey_hex, signature_hex)?;

        if sha256.is_none() && signed_manifest.is_none() {
            return Ok(None); // Nothing to verify
        }

        Ok(Some(Self {
            version: version.to_string(),
            sha256,
            signed_manifest,
        }))
    }

    /// Verify the SHA-256 of downloaded bytes against the manifest.
    ///
    /// Returns `Ok(())` if no hash was declared (skips check).
    pub fn verify_hash(&self, bytes: &[u8]) -> Result<(), VerificationError> {
        let Some(expected) = self.sha256 else {
            return Ok(()); // No hash declared — skip
        };

        let actual_bytes: [u8; 32] = Sha256::digest(bytes).into();
        // Use constant-time comparison to avoid timing side-channels.
        if actual_bytes.ct_eq(&expected[..]).unwrap_u8() == 0 {
            return Err(VerificationError::HashMismatch {
                expected: hex::encode(expected),
                actual: hex::encode(actual_bytes),
            });
        }

        Ok(())
    }

    /// Verify the Ed25519 signature against the trusted key store.
    ///
    /// **Trust check**: the publisher's key must appear in `store`.  If the store is
    /// empty (no keys loaded), or the manifest pubkey is not trusted, the check fails.
    ///
    /// Returns `Ok(())` if no signed manifest is present (skips check).
    pub fn verify_signature(
        &self,
        name: &str,
        store: &TrustedKeyStore,
    ) -> Result<(), VerificationError> {
        let Some(ref manifest) = self.signed_manifest else {
            return Ok(()); // No signature declared — skip
        };

        // Key-trust check first
        if !store.is_trusted(&manifest.publisher_pubkey) {
            tracing::warn!(
                tool = %name,
                pubkey = %hex::encode(manifest.publisher_pubkey),
                "Publisher key not in trust store — signature verification skipped"
            );
            // Policy: unknown keys are treated as untrusted → reject
            return Err(VerificationError::SignatureInvalid {
                name: name.to_string(),
            });
        }

        manifest.verify_signature(name, &self.version)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use ed25519_dalek::{Signature, Signer, SigningKey};
    use rand::rngs::OsRng;

    fn make_keypair() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn sign_manifest(sk: &SigningKey, name: &str, version: &str, sha256: &[u8; 32]) -> [u8; 64] {
        let msg = SignedManifest::signing_message(name, version, sha256);
        let sig: Signature = sk.sign(&msg);
        sig.to_bytes()
    }

    fn sha256_of(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    // -----------------------------------------------------------------------
    // Hash verification
    // -----------------------------------------------------------------------

    #[test]
    fn hash_verification_passes_when_correct() {
        let data = b"fake wasm data";
        let expected = sha256_of(data);
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: Some(expected),
            signed_manifest: None,
        };
        assert!(verif.verify_hash(data).is_ok());
    }

    #[test]
    fn hash_verification_fails_when_tampered() {
        let data = b"fake wasm data";
        let wrong = [0xFFu8; 32];
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: Some(wrong),
            signed_manifest: None,
        };
        assert!(matches!(
            verif.verify_hash(data),
            Err(VerificationError::HashMismatch { .. })
        ));
    }

    #[test]
    fn hash_verification_skipped_when_none() {
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: None,
            signed_manifest: None,
        };
        assert!(verif.verify_hash(b"anything").is_ok());
    }

    // -----------------------------------------------------------------------
    // Signature verification
    // -----------------------------------------------------------------------

    #[test]
    fn signature_verification_passes_with_trusted_key() {
        let sk = make_keypair();
        let vk = sk.verifying_key();
        let vk_bytes = vk.to_bytes();

        let data = b"real wasm binary";
        let sha256 = sha256_of(data);
        let sig_bytes = sign_manifest(&sk, "my-tool", "0.1.0", &sha256);

        let manifest = SignedManifest {
            binary_sha256: sha256,
            publisher_pubkey: vk_bytes,
            signature: sig_bytes,
        };
        let store = TrustedKeyStore {
            key_bytes: vec![vk_bytes],
        };
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: Some(sha256),
            signed_manifest: Some(manifest),
        };

        assert!(verif.verify_signature("my-tool", &store).is_ok());
    }

    #[test]
    fn signature_verification_fails_with_untrusted_key() {
        let sk = make_keypair();
        let vk_bytes = sk.verifying_key().to_bytes();

        let data = b"real wasm binary";
        let sha256 = sha256_of(data);
        let sig_bytes = sign_manifest(&sk, "my-tool", "0.1.0", &sha256);

        let manifest = SignedManifest {
            binary_sha256: sha256,
            publisher_pubkey: vk_bytes,
            signature: sig_bytes,
        };
        // Empty trust store — key not trusted
        let store = TrustedKeyStore::empty();
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: Some(sha256),
            signed_manifest: Some(manifest),
        };

        assert!(matches!(
            verif.verify_signature("my-tool", &store),
            Err(VerificationError::SignatureInvalid { .. })
        ));
    }

    #[test]
    fn signature_verification_fails_with_wrong_signature() {
        let sk = make_keypair();
        let vk_bytes = sk.verifying_key().to_bytes();

        let data = b"real wasm binary";
        let sha256 = sha256_of(data);
        // Sign with wrong name
        let sig_bytes = sign_manifest(&sk, "other-tool", "0.1.0", &sha256);

        let manifest = SignedManifest {
            binary_sha256: sha256,
            publisher_pubkey: vk_bytes,
            signature: sig_bytes,
        };
        let store = TrustedKeyStore {
            key_bytes: vec![vk_bytes],
        };
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: Some(sha256),
            signed_manifest: Some(manifest),
        };

        assert!(matches!(
            verif.verify_signature("my-tool", &store),
            Err(VerificationError::SignatureInvalid { .. })
        ));
    }

    #[test]
    fn signature_verification_skipped_when_no_manifest() {
        let verif = DownloadVerification {
            version: "0.1.0".to_string(),
            sha256: None,
            signed_manifest: None,
        };
        assert!(
            verif
                .verify_signature("any-tool", &TrustedKeyStore::empty())
                .is_ok()
        );
    }

    // -----------------------------------------------------------------------
    // from_artifact / SignedManifest::from_hex
    // -----------------------------------------------------------------------

    #[test]
    fn from_artifact_returns_none_when_all_fields_missing() {
        let result = DownloadVerification::from_artifact("0.1.0", None, None, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn from_artifact_parses_sha256_only() {
        let sha256 = sha256_of(b"test");
        let hex_str = hex::encode(sha256);
        let result =
            DownloadVerification::from_artifact("0.1.0", Some(&hex_str), None, None).unwrap();
        let verif = result.unwrap();
        assert_eq!(verif.sha256, Some(sha256));
        assert!(verif.signed_manifest.is_none());
    }

    #[test]
    fn from_artifact_fails_on_wrong_length_sha256() {
        let err = DownloadVerification::from_artifact("0.1.0", Some("aabbcc"), None, None);
        assert!(matches!(err, Err(VerificationError::InvalidHex(_))));
    }

    // -----------------------------------------------------------------------
    // TrustedKeyStore helpers
    // -----------------------------------------------------------------------

    #[test]
    fn trusted_key_store_identifies_trusted_key() {
        let sk = make_keypair();
        let vk_bytes = sk.verifying_key().to_bytes();
        let store = TrustedKeyStore {
            key_bytes: vec![vk_bytes],
        };
        assert!(store.is_trusted(&vk_bytes));
    }

    #[test]
    fn trusted_key_store_rejects_unknown_key() {
        let sk = make_keypair();
        let vk_bytes = sk.verifying_key().to_bytes();
        let store = TrustedKeyStore::empty();
        assert!(!store.is_trusted(&vk_bytes));
    }

    #[test]
    fn empty_trust_store_rejects_signed_manifest_fail_closed() {
        // A signed manifest with no keys in the trust store MUST be rejected,
        // never silently accepted.
        let sk = make_keypair();
        let data = b"some wasm binary";
        let sha256 = sha256_of(data);
        let msg = SignedManifest::signing_message("my-tool", "1.0.0", &sha256);
        let sig = sk.sign(&msg).to_bytes();
        let manifest = SignedManifest {
            binary_sha256: sha256,
            publisher_pubkey: sk.verifying_key().to_bytes(),
            signature: sig,
        };
        let verif = DownloadVerification {
            version: "1.0.0".to_string(),
            sha256: Some(sha256),
            signed_manifest: Some(manifest),
        };
        // Empty store — must fail even with a valid signature
        let result = verif.verify_signature("my-tool", &TrustedKeyStore::empty());
        assert!(
            matches!(result, Err(VerificationError::SignatureInvalid { .. })),
            "empty trust store must reject all signatures; got {:?}",
            result
        );
    }

    #[test]
    fn wrong_length_sha256_hex_returns_error() {
        // 30 hex chars = 15 bytes, not 32
        let err = DownloadVerification::from_artifact(
            "1.0.0",
            Some("aabbccddeeff001122334455"),
            None,
            None,
        );
        assert!(matches!(err, Err(VerificationError::InvalidHex(_))));
    }

    #[test]
    fn modified_binary_fails_hash_check() {
        let original = b"original wasm binary";
        let sha256 = sha256_of(original);
        let verif = DownloadVerification {
            version: "1.0.0".to_string(),
            sha256: Some(sha256),
            signed_manifest: None,
        };
        // A different binary must fail
        let modified = b"tampered wasm binary!";
        let result = verif.verify_hash(modified);
        assert!(
            matches!(result, Err(VerificationError::HashMismatch { .. })),
            "tampered binary must not pass hash check; got {:?}",
            result
        );
    }
}
